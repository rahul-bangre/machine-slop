mod routes;

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};
use slop_process::Processes;

pub fn app(processes: Arc<Processes>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/processes", post(routes::start))
        .route("/processes/{id}", get(routes::info))
        .route("/processes/{id}/wait", get(routes::wait))
        .route("/processes/{id}/events", get(routes::events))
        .route("/processes/{id}/kill", post(routes::kill))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(processes)
}
