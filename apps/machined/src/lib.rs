use std::sync::Arc;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Request, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use hyper_util::rt::TokioIo;
use slop_machines::{CreateMachine, Machine, MachineError, MachineInfo, Machines};

pub fn app(machines: Arc<Machines>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/machines", get(list).post(create))
        .route("/machines/{id}", get(info).delete(destroy))
        .route("/machines/{id}/{*path}", any(proxy))
        .with_state(machines)
}

async fn create(
    State(machines): State<Arc<Machines>>,
    body: Bytes,
) -> Result<(StatusCode, Json<MachineInfo>), ApiError> {
    let request = if body.is_empty() {
        CreateMachine::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|error| ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?
    };
    let machine = machines.create(request).await?;
    Ok((StatusCode::CREATED, Json(machine.info())))
}

async fn list(State(machines): State<Arc<Machines>>) -> Json<Vec<MachineInfo>> {
    Json(machines.list().await)
}

async fn info(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
) -> Result<Json<MachineInfo>, ApiError> {
    Ok(Json(find(&machines, &id).await?.info()))
}

async fn destroy(
    State(machines): State<Arc<Machines>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if machines.destroy(&id).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(unknown_machine())
    }
}

async fn proxy(
    State(machines): State<Arc<Machines>>,
    Path((id, _)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let machine = find(&machines, &id).await?;
    let (mut parts, body) = request.into_parts();
    parts.uri = agentd_uri(&parts.uri, &id)?;
    parts
        .headers
        .insert(header::HOST, header::HeaderValue::from_static("agentd"));

    let stream = machine.connect().await.map_err(unreachable_agentd)?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(unreachable_agentd)?;
    tokio::spawn(connection);
    let response = sender
        .send_request(Request::from_parts(parts, body))
        .await
        .map_err(unreachable_agentd)?;
    Ok(response.map(Body::new))
}

fn agentd_uri(uri: &Uri, id: &str) -> Result<Uri, ApiError> {
    let prefix = format!("/machines/{id}");
    uri.path_and_query()
        .and_then(|path| path.as_str().strip_prefix(&prefix))
        .and_then(|path| path.parse().ok())
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "malformed machine path".into()))
}

async fn find(machines: &Machines, id: &str) -> Result<Arc<Machine>, ApiError> {
    machines.get(id).await.ok_or_else(unknown_machine)
}

fn unknown_machine() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "unknown machine".into())
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
            MachineError::Capacity | MachineError::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            MachineError::Boot(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}
