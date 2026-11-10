use std::sync::Arc;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Request, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use slop_machines::{
    CreateMachine, CreateSnapshot, MachineError, MachineInfo, MachineState, Machines, SnapshotInfo,
    UpdateMachine,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub fn app(machines: Arc<Machines>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
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
        .with_state(machines)
}

async fn metrics(State(machines): State<Arc<Machines>>) -> impl IntoResponse {
    let (active, reserved, budget) = machines.resource_usage();
    let (successes, failures) = machines.create_counts();
    let spares = machines.spares().await;
    let listed = machines.list().await;
    let in_state = |wanted: fn(&MachineState) -> bool| {
        listed
            .iter()
            .filter(|machine| wanted(&machine.state))
            .count()
    };
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
    State(machines): State<Arc<Machines>>,
    body: Bytes,
) -> Result<(StatusCode, Json<MachineInfo>), ApiError> {
    let request: CreateMachine = json_body(if body.is_empty() { b"{}" } else { &body })?;
    let machine = machines.create(request).await?;
    Ok((StatusCode::CREATED, Json(machine.info())))
}

async fn update(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<MachineInfo>, ApiError> {
    let request: UpdateMachine = json_body(&body)?;
    Ok(Json(machines.update(&id, request).await?))
}

fn json_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|error| ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))
}

async fn list(State(machines): State<Arc<Machines>>) -> Json<Vec<MachineInfo>> {
    Json(machines.list().await)
}

async fn info(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
) -> Result<Json<MachineInfo>, ApiError> {
    let machine = machines.get(&id).await;
    Ok(Json(machine.ok_or(MachineError::UnknownMachine)?.info()))
}

async fn destroy(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    machines.destroy(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn pause(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
) -> Result<Json<MachineInfo>, ApiError> {
    Ok(Json(machines.pause(&id).await?))
}

async fn resume(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
) -> Result<Json<MachineInfo>, ApiError> {
    Ok(Json(machines.resume(&id).await?))
}

async fn fork(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<MachineInfo>), ApiError> {
    let machine = machines.fork(&id).await?;
    Ok((StatusCode::CREATED, Json(machine.info())))
}

async fn create_snapshot(
    State(machines): State<Arc<Machines>>,
    body: Bytes,
) -> Result<(StatusCode, Json<SnapshotInfo>), ApiError> {
    let request: CreateSnapshot = json_body(&body)?;
    let snapshot = machines.create_snapshot(request).await?;
    Ok((StatusCode::CREATED, Json(snapshot)))
}

async fn list_snapshots(State(machines): State<Arc<Machines>>) -> Json<Vec<SnapshotInfo>> {
    Json(machines.snapshots().await)
}

async fn snapshot_info(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
) -> Result<Json<SnapshotInfo>, ApiError> {
    let snapshot = machines.snapshot_info(&id).await;
    Ok(Json(snapshot.ok_or(MachineError::UnknownSnapshot)?))
}

async fn delete_snapshot(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    machines.delete_snapshot(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn proxy(
    State(machines): State<Arc<Machines>>,
    Path((id, _)): Path<(String, String)>,
    mut request: Request,
) -> Result<Response, ApiError> {
    let machine = machines
        .begin_use(&id)
        .await
        .ok_or(MachineError::UnknownMachine)?;
    if machine.state() == MachineState::Paused {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "machine is paused; resume it first".into(),
        ));
    }
    let machine = Arc::new(machine);
    let client_upgrade = hyper::upgrade::on(&mut request);
    let (mut parts, body) = request.into_parts();
    parts.uri = agentd_uri(&parts.uri, &id)?;
    parts
        .headers
        .insert(header::HOST, header::HeaderValue::from_static("agentd"));

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

fn agentd_uri(uri: &Uri, id: &str) -> Result<Uri, ApiError> {
    let prefix = format!("/machines/{id}");
    uri.path_and_query()
        .and_then(|path| path.as_str().strip_prefix(&prefix))
        .and_then(|path| path.parse().ok())
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "malformed machine path".into()))
}

fn unreachable_agentd(error: impl std::fmt::Display) -> ApiError {
    ApiError(
        StatusCode::BAD_GATEWAY,
        format!("machine agent is unreachable: {error}"),
    )
}

struct ApiError(StatusCode, String);

impl From<MachineError> for ApiError {
    fn from(error: MachineError) -> Self {
        let status = match error {
            MachineError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            MachineError::UnknownMachine | MachineError::UnknownSnapshot => StatusCode::NOT_FOUND,
            MachineError::Conflict(_) => StatusCode::CONFLICT,
            MachineError::Capacity | MachineError::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            MachineError::Boot(_) | MachineError::Failed { .. } | MachineError::Io(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self(status, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}
