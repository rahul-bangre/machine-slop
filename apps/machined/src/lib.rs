mod preview;

use std::sync::Arc;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{FromRequestParts, Path, Request, State},
    http::{StatusCode, Uri, header, request::Parts},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use hyper::{client::conn::http1::SendRequest, upgrade::OnUpgrade};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use sha2::Digest;
use slop_machines::{
    CreateMachine, CreatePreview, CreateSnapshot, HostReport, HostReportReply, IdempotencyKey,
    Machine, MachineError, MachineInfo, MachineState, Machines, PreviewInfo, Settlement,
    SnapshotInfo, Tenant, TenantUsage, UpdateMachine,
};

pub use preview::serve_previews;
pub use slop_machines::{ApiKeys, parse_tenants};

pub struct Host {
    pub machines: Arc<Machines>,
    pub access: Access,
}

pub enum Access {
    TenantKeys(ApiKeys),
    ControlPlane(ControlKey),
}

pub struct ControlKey([u8; 32]);

impl ControlKey {
    pub fn new(key: &str) -> Self {
        Self(sha2::Sha256::digest(key.trim().as_bytes()).into())
    }

    pub fn admits(&self, presented: &str) -> bool {
        <[u8; 32]>::from(sha2::Sha256::digest(presented.as_bytes())) == self.0
    }
}

pub const TENANT_HEADER: &str = "slop-tenant";
pub const GENERATION_HEADER: &str = "slop-generation";

pub fn app(host: Arc<Host>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .route("/usage", get(usage))
        .route("/machines", get(list).post(create))
        .route("/machines/{id}", get(info).patch(update).delete(destroy))
        .route("/machines/{id}/pause", post(pause))
        .route("/machines/{id}/resume", post(resume))
        .route("/machines/{id}/fork", post(fork))
        .route("/machines/{id}/{*path}", any(proxy))
        .route("/snapshots", get(list_snapshots).post(create_snapshot))
        .route(
            "/snapshots/{id}",
            get(snapshot_info).delete(delete_snapshot),
        )
        .route("/previews", get(list_previews).post(create_preview))
        .route("/previews/{id}", get(preview_info).delete(delete_preview))
        .route("/operations/{key}/settle", post(settle))
        .layer(middleware::from_fn_with_state(
            host.clone(),
            stamp_generation,
        ))
        .with_state(host)
}

async fn stamp_generation(State(host): State<Arc<Host>>, request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    if host.machines.pool().is_some() {
        let generation = host.machines.generation();
        if let Ok(value) =
            header::HeaderValue::from_str(&format!("{}.{}", generation.boot, generation.change))
        {
            response.headers_mut().insert(GENERATION_HEADER, value);
        }
    }
    response
}

async fn settle(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let key = IdempotencyKey::parse(&key)?;
    let (settlement, _) = host.machines.settle(&tenant, key).await?;
    Ok(match settlement {
        Settlement::Made(info) => Json(info).into_response(),
        Settlement::Nothing => (
            StatusCode::NOT_FOUND,
            "no machine exists for this operation, and none will be made for it",
        )
            .into_response(),
    })
}

struct Caller(Arc<Tenant>);

impl FromRequestParts<Arc<Host>> for Caller {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, host: &Arc<Host>) -> Result<Self, Response> {
        let presented = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let tenant = match &host.access {
            Access::TenantKeys(keys) => presented.and_then(|key| keys.tenant_of(key)),
            Access::ControlPlane(control) => presented
                .filter(|key| control.admits(key))
                .and_then(|_| parts.headers.get(TENANT_HEADER))
                .and_then(|value| value.to_str().ok()),
        };
        tenant
            .and_then(|tenant| host.machines.tenant(tenant))
            .map(Self)
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Bearer")],
                    "a valid API key is required as a Bearer token",
                )
                    .into_response()
            })
    }
}

struct Retry(Option<IdempotencyKey>);

impl<S: Sync> FromRequestParts<S> for Retry {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        match parts.headers.get("idempotency-key") {
            None => Ok(Self(None)),
            Some(value) => {
                let key = value.to_str().map_err(|_| {
                    MachineError::Invalid("an idempotency key is visible ASCII".into())
                })?;
                Ok(Self(Some(IdempotencyKey::parse(key)?)))
            }
        }
    }
}

pub const REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

pub async fn report_to_control_plane(
    machines: Arc<Machines>,
    control: String,
    advertised: String,
    control_key: String,
) {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<http_body_util::Full<Bytes>>();
    let endpoint = format!("{}/hosts", control.trim_end_matches('/'));
    let mut reaching = true;
    let mut ticks = tokio::time::interval(REPORT_INTERVAL);
    loop {
        ticks.tick().await;
        let (Some(pool), Some(store)) = (machines.pool(), machines.store()) else {
            return;
        };
        let (generation, reported, executions) = machines.report().await;
        let report = HostReport {
            url: advertised.clone(),
            pool: pool.to_owned(),
            store: store.url().to_owned(),
            room: machines.room().await,
            egress_addresses: machines.egress_addresses(),
            generation,
            machines: reported,
            executions,
        };
        let sent = async {
            let body = serde_json::to_vec(&report).map_err(|error| error.to_string())?;
            let request = hyper::Request::post(endpoint.as_str())
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {control_key}"))
                .body(http_body_util::Full::new(Bytes::from(body)))
                .map_err(|error| error.to_string())?;
            let response = tokio::time::timeout(REPORT_INTERVAL, client.request(request))
                .await
                .map_err(|_| "no answer".to_owned())?
                .map_err(|error| error.to_string())?;
            if !response.status().is_success() {
                return Err(format!("control plane answered {}", response.status()));
            }
            let body = http_body_util::BodyExt::collect(response.into_body())
                .await
                .map_err(|error| error.to_string())?
                .to_bytes();
            let reply: HostReportReply =
                serde_json::from_slice(&body).map_err(|error| error.to_string())?;
            machines
                .forget_executions(&reply.recorded_executions)
                .await
                .map_err(|error| format!("forgetting recorded executions failed: {error}"))
        };
        match (sent.await, reaching) {
            (Ok(()), false) => {
                tracing::info!(%control, "reporting to the control plane");
                reaching = true;
            }
            (Err(error), true) => {
                tracing::warn!(%control, %error, "reporting to the control plane failed");
                reaching = false;
            }
            _ => {}
        }
    }
}

async fn usage(Caller(tenant): Caller) -> Json<TenantUsage> {
    Json(tenant.usage())
}

async fn metrics(State(host): State<Arc<Host>>) -> impl IntoResponse {
    let machines = &host.machines;
    let (active, reserved, budget) = machines.resource_usage();
    let (successes, failures) = machines.create_counts();
    let spares = machines.spares().await;
    let states = machines.states().await;
    let in_state =
        |wanted: fn(&MachineState) -> bool| states.iter().filter(|state| wanted(state)).count();
    let running = in_state(|state| matches!(state, MachineState::Running));
    let paused = in_state(|state| matches!(state, MachineState::Paused));
    let exited = in_state(|state| matches!(state, MachineState::Exited { .. }));
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        format!(
            "# TYPE machined_machines_active gauge\nmachined_machines_active {active}\n# TYPE machined_memory_reserved_mib gauge\nmachined_memory_reserved_mib {reserved}\n# TYPE machined_memory_budget_mib gauge\nmachined_memory_budget_mib {budget}\n# TYPE machined_spare_machines gauge\nmachined_spare_machines {spares}\n# TYPE machined_machines gauge\nmachined_machines{{state=\"running\"}} {running}\nmachined_machines{{state=\"paused\"}} {paused}\nmachined_machines{{state=\"exited\"}} {exited}\n# TYPE machined_creates_total counter\nmachined_creates_total{{result=\"success\"}} {successes}\nmachined_creates_total{{result=\"failure\"}} {failures}\n"
        ),
    )
}

async fn create(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Retry(key): Retry,
    body: Bytes,
) -> Result<(StatusCode, Json<MachineInfo>), ApiError> {
    let request: CreateMachine = json_body(if body.is_empty() { b"{}" } else { &body })?;
    let machine = host.machines.create(&tenant, request, key).await?;
    Ok((StatusCode::CREATED, Json(machine.info())))
}

async fn update(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<MachineInfo>, ApiError> {
    let request: UpdateMachine = json_body(&body)?;
    Ok(Json(host.machines.update(&tenant, &id, request).await?))
}

fn json_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|error| ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string(), None))
}

async fn list(State(host): State<Arc<Host>>, Caller(tenant): Caller) -> Json<Vec<MachineInfo>> {
    Json(host.machines.list(&tenant).await)
}

async fn info(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<Json<MachineInfo>, ApiError> {
    let machine = host.machines.get(&tenant, &id).await;
    Ok(Json(machine.ok_or(MachineError::UnknownMachine)?.info()))
}

async fn destroy(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    host.machines.destroy(&tenant, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn pause(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<Json<MachineInfo>, ApiError> {
    Ok(Json(host.machines.pause(&tenant, &id).await?))
}

async fn resume(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<Json<MachineInfo>, ApiError> {
    Ok(Json(host.machines.resume(&tenant, &id).await?))
}

async fn fork(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Retry(key): Retry,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<MachineInfo>), ApiError> {
    let machine = host.machines.fork(&tenant, &id, key).await?;
    Ok((StatusCode::CREATED, Json(machine.info())))
}

async fn create_snapshot(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Retry(key): Retry,
    body: Bytes,
) -> Result<(StatusCode, Json<SnapshotInfo>), ApiError> {
    let request: CreateSnapshot = json_body(&body)?;
    let snapshot = host.machines.create_snapshot(&tenant, request, key).await?;
    Ok((StatusCode::CREATED, Json(snapshot)))
}

async fn list_snapshots(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
) -> Result<Json<Vec<SnapshotInfo>>, ApiError> {
    Ok(Json(host.machines.snapshots(&tenant).await?))
}

async fn snapshot_info(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<Json<SnapshotInfo>, ApiError> {
    Ok(Json(host.machines.snapshot_info(&tenant, &id).await?))
}

async fn delete_snapshot(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    host.machines.delete_snapshot(&tenant, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_preview(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    body: Bytes,
) -> Result<(StatusCode, Json<PreviewInfo>), ApiError> {
    let request: CreatePreview = json_body(&body)?;
    let preview = host.machines.create_preview(&tenant, request).await?;
    Ok((StatusCode::CREATED, Json(preview)))
}

async fn list_previews(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
) -> Json<Vec<PreviewInfo>> {
    Json(host.machines.previews(&tenant).await)
}

async fn preview_info(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<Json<PreviewInfo>, ApiError> {
    let preview = host.machines.preview(&tenant, &id).await;
    Ok(Json(preview.ok_or(MachineError::UnknownPreview)?))
}

async fn delete_preview(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    host.machines.delete_preview(&tenant, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn proxy(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path((id, _)): Path<(String, String)>,
    mut request: Request,
) -> Result<Response, ApiError> {
    let machine = host
        .machines
        .begin_use(&tenant, &id)
        .await
        .ok_or(MachineError::UnknownMachine)?;
    if machine.state() == MachineState::Paused {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "machine is paused; resume it first".into(),
            None,
        ));
    }
    let machine = Arc::new(machine);
    let client_upgrade = hyper::upgrade::on(&mut request);
    let (mut parts, body) = request.into_parts();
    address_to_agentd(&mut parts, &id)?;

    let mut sender = agentd(&machine, machine.clone()).await?;
    let mut response = sender
        .send_request(Request::from_parts(parts, body))
        .await
        .map_err(unreachable_agentd)?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let agentd_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            let _machine_in_use_until_the_tunnel_ends = machine;
            relay_upgrades(client_upgrade, agentd_upgrade).await;
        });
    }
    Ok(response.map(Body::new))
}

async fn agentd(
    machine: &Machine,
    in_use: impl Send + 'static,
) -> Result<SendRequest<Body>, ApiError> {
    let stream = machine.connect().await.map_err(unreachable_agentd)?;
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(unreachable_agentd)?;
    tokio::spawn(async move {
        let _machine_in_use_until_agentd_disconnects = in_use;
        connection.with_upgrades().await
    });
    Ok(sender)
}

async fn relay_upgrades(client: OnUpgrade, upstream: OnUpgrade) {
    if let (Ok(client), Ok(upstream)) = tokio::join!(client, upstream) {
        slop_machines::relay_until_either_side_ends(TokioIo::new(client), TokioIo::new(upstream))
            .await;
    }
}

fn address_to_agentd(parts: &mut Parts, id: &str) -> Result<(), ApiError> {
    parts.uri = agentd_uri(&parts.uri, id)?;
    parts.headers.remove(header::AUTHORIZATION);
    parts
        .headers
        .insert(header::HOST, header::HeaderValue::from_static("agentd"));
    Ok(())
}

fn agentd_uri(uri: &Uri, id: &str) -> Result<Uri, ApiError> {
    let prefix = format!("/machines/{id}");
    uri.path_and_query()
        .and_then(|path| path.as_str().strip_prefix(&prefix))
        .and_then(|path| path.parse().ok())
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "malformed machine path".into(),
                None,
            )
        })
}

fn unreachable_agentd(error: impl std::fmt::Display) -> ApiError {
    ApiError(
        StatusCode::BAD_GATEWAY,
        format!("machine agent is unreachable: {error}"),
        None,
    )
}

struct ApiError(StatusCode, String, Option<u64>);

impl From<MachineError> for ApiError {
    fn from(error: MachineError) -> Self {
        let retry_after_secs = match &error {
            MachineError::RateLimited { retry_after } => {
                Some(retry_after.as_secs_f64().ceil().max(1.0) as u64)
            }
            _ => None,
        };
        let status = match error {
            MachineError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            MachineError::QuotaExceeded(_) | MachineError::RateLimited { .. } => {
                StatusCode::TOO_MANY_REQUESTS
            }
            MachineError::UnknownMachine
            | MachineError::UnknownSnapshot
            | MachineError::UnknownPreview => StatusCode::NOT_FOUND,
            MachineError::Conflict(_) => StatusCode::CONFLICT,
            MachineError::Capacity | MachineError::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            MachineError::Store(_) => StatusCode::BAD_GATEWAY,
            MachineError::Boot(_) | MachineError::Failed { .. } | MachineError::Io(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self(status, error.to_string(), retry_after_secs)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self.2 {
            Some(secs) => {
                (self.0, [(header::RETRY_AFTER, secs.to_string())], self.1).into_response()
            }
            None => (self.0, self.1).into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forwarded_request_leaves_the_api_key_outside_the_machine() {
        let (mut parts, ()) = Request::builder()
            .uri("/machines/m1/processes?cursor=3")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header(header::HOST, "machined")
            .header("x-kept", "yes")
            .body(())
            .expect("request")
            .into_parts();
        assert!(address_to_agentd(&mut parts, "m1").is_ok());
        assert_eq!(parts.uri, "/processes?cursor=3");
        assert!(parts.headers.get(header::AUTHORIZATION).is_none());
        assert_eq!(parts.headers[header::HOST], "agentd");
        assert_eq!(parts.headers["x-kept"], "yes");
    }
}
