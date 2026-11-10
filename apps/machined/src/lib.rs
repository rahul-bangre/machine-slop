mod tenants;

use std::sync::Arc;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{FromRequestParts, Path, Request, State},
    http::{StatusCode, Uri, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use slop_machines::{
    CreateMachine, CreateSnapshot, IdempotencyKey, MachineError, MachineInfo, MachineState,
    Machines, SnapshotInfo, Tenant, TenantUsage, UpdateMachine,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub use tenants::{ApiKeys, parse as parse_tenants};

pub struct Host {
    pub machines: Arc<Machines>,
    pub keys: ApiKeys,
}

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
        .with_state(host)
}

struct Caller(Arc<Tenant>);

impl FromRequestParts<Arc<Host>> for Caller {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, host: &Arc<Host>) -> Result<Self, Response> {
        parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|key| host.keys.tenant_of(key))
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
) -> Json<Vec<SnapshotInfo>> {
    Json(host.machines.snapshots(&tenant).await)
}

async fn snapshot_info(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<Json<SnapshotInfo>, ApiError> {
    let snapshot = host.machines.snapshot_info(&tenant, &id).await;
    Ok(Json(snapshot.ok_or(MachineError::UnknownSnapshot)?))
}

async fn delete_snapshot(
    State(host): State<Arc<Host>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    host.machines.delete_snapshot(&tenant, &id).await?;
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

    let stream = machine.connect().await.map_err(unreachable_agentd)?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(unreachable_agentd)?;
    let connection_use = machine.clone();
    tokio::spawn(async move {
        let _machine_in_use_until_agentd_disconnects = connection_use;
        connection.with_upgrades().await
    });
    let mut response = sender
        .send_request(Request::from_parts(parts, body))
        .await
        .map_err(unreachable_agentd)?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let agentd_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            let _machine_in_use_until_the_tunnel_ends = machine;
            if let (Ok(client), Ok(agentd)) = tokio::join!(client_upgrade, agentd_upgrade) {
                relay_until_either_side_ends(TokioIo::new(client), TokioIo::new(agentd)).await;
            }
        });
    }
    Ok(response.map(Body::new))
}

async fn relay_until_either_side_ends(
    client: impl AsyncRead + AsyncWrite + Unpin,
    agentd: impl AsyncRead + AsyncWrite + Unpin,
) {
    let (mut from_client, mut to_client) = tokio::io::split(client);
    let (mut from_agentd, mut to_agentd) = tokio::io::split(agentd);
    tokio::select! {
        _ = tokio::io::copy(&mut from_client, &mut to_agentd) => {}
        _ = async {
            let _ = tokio::io::copy(&mut from_agentd, &mut to_client).await;
            let _ = to_client.shutdown().await;
        } => {}
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
            MachineError::UnknownMachine | MachineError::UnknownSnapshot => StatusCode::NOT_FOUND,
            MachineError::Conflict(_) => StatusCode::CONFLICT,
            MachineError::Capacity | MachineError::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
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
