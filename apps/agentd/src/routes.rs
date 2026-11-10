use std::{convert::Infallible, sync::Arc};

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use slop_process::{
    Process, ProcessError, ProcessEvent, ProcessInfo, ProcessResult, Processes, StartProcess,
};

pub async fn start(
    State(processes): State<Arc<Processes>>,
    Json(request): Json<StartProcess>,
) -> Result<(StatusCode, Json<ProcessInfo>), ApiError> {
    Ok((StatusCode::CREATED, Json(processes.start(request)?.info())))
}

pub async fn info(
    State(processes): State<Arc<Processes>>,
    Path(id): Path<String>,
) -> Result<Json<ProcessInfo>, ApiError> {
    Ok(Json(find(&processes, &id)?.info()))
}

pub async fn wait(
    State(processes): State<Arc<Processes>>,
    Path(id): Path<String>,
) -> Result<Json<ProcessResult>, ApiError> {
    Ok(Json(find(&processes, &id)?.wait().await))
}

pub async fn kill(
    State(processes): State<Arc<Processes>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    find(&processes, &id)?.kill();
    Ok(StatusCode::ACCEPTED)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    #[serde(default)]
    cursor: u64,
}

pub async fn events(
    State(processes): State<Arc<Processes>>,
    Path(id): Path<String>,
    Query(Cursor { cursor }): Query<Cursor>,
) -> Result<Response, ApiError> {
    let process = find(&processes, &id)?;
    let mut changed = process.subscribe();
    let initial = process.events(cursor)?;
    if initial.finished && initial.chunks.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let stream = async_stream::stream! {
        let mut batch = initial;
        loop {
            for chunk in batch.chunks {
                yield Ok::<_, Infallible>(chunk);
            }
            let cursor = batch.next_seq;
            if batch.finished { break; }
            if changed.changed().await.is_err() { break; }
            match process.events(cursor) {
                Ok(next) => batch = next,
                Err(ProcessError::Gap { first_seq }) => {
                    let mut gap = serde_json::to_vec(&ProcessEvent::Gap { first_seq }).expect("gap is serializable");
                    gap.push(b'\n');
                    yield Ok(gap.into());
                    break;
                }
                Err(_) => break,
            }
        }
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

fn find(processes: &Processes, id: &str) -> Result<Arc<Process>, ApiError> {
    processes.get(id).ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "unknown or expired process".into(),
    ))
}

pub struct ApiError(StatusCode, String);

impl From<ProcessError> for ApiError {
    fn from(error: ProcessError) -> Self {
        match error {
            ProcessError::ShuttingDown => Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon is shutting down".into(),
            ),
            ProcessError::Capacity => Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "process capacity reached".into(),
            ),
            ProcessError::Spawn(error) => Self(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
            ProcessError::InvalidCursor => Self(
                StatusCode::BAD_REQUEST,
                "cursor is ahead of process output".into(),
            ),
            ProcessError::Gap { first_seq } => Self(
                StatusCode::GONE,
                format!("output expired; first available cursor is {first_seq}"),
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}
