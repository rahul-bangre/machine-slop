use std::{convert::Infallible, sync::Arc};

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use slop_process::{
    Process, ProcessError, ProcessEvent, ProcessInfo, ProcessResult, Processes, StartProcess,
    TerminalSize,
};

use crate::error::ApiError;

pub async fn start(
    State(processes): State<Arc<Processes>>,
    Json(request): Json<StartProcess>,
) -> Result<(StatusCode, Json<ProcessInfo>), ApiError> {
    let process = tokio::task::spawn_blocking(move || processes.start(request))
        .await
        .expect("process start task panicked")?;
    Ok((StatusCode::CREATED, Json(process.info())))
}

pub async fn info(
    State(processes): State<Arc<Processes>>,
    Path(id): Path<String>,
) -> Result<Json<ProcessInfo>, ApiError> {
    Ok(Json(find(&processes, &id).await?.info()))
}

pub async fn wait(
    State(processes): State<Arc<Processes>>,
    Path(id): Path<String>,
) -> Result<Json<ProcessResult>, ApiError> {
    Ok(Json(find(&processes, &id).await?.wait().await))
}

pub async fn kill(
    State(processes): State<Arc<Processes>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    find(&processes, &id).await?.kill();
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdinPosition {
    offset: u64,
    #[serde(default)]
    close: bool,
}

pub async fn write_stdin(
    State(processes): State<Arc<Processes>>,
    Path(id): Path<String>,
    Query(StdinPosition { offset, close }): Query<StdinPosition>,
    data: Bytes,
) -> Result<StatusCode, ApiError> {
    find(&processes, &id)
        .await?
        .write_stdin(offset, data, close)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn resize_terminal(
    State(processes): State<Arc<Processes>>,
    Path(id): Path<String>,
    Json(size): Json<TerminalSize>,
) -> Result<StatusCode, ApiError> {
    find(&processes, &id).await?.resize_terminal(size)?;
    Ok(StatusCode::NO_CONTENT)
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
    let process = find(&processes, &id).await?;
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

async fn find(processes: &Arc<Processes>, id: &str) -> Result<Arc<Process>, ApiError> {
    let processes = processes.clone();
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || processes.get(&id))
        .await
        .expect("process lookup task panicked")
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "unknown or expired process".into(),
        ))
}
