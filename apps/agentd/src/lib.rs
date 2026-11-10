mod error;
mod files;
mod ports;
mod routes;

use std::sync::Arc;

use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::header,
    response::IntoResponse,
    routing::{get, post},
};
use slop_process::Processes;

pub fn app(processes: Arc<Processes>) -> Router {
    let process_routes = Router::new()
        .route("/metrics", get(metrics))
        .route("/processes", post(routes::start))
        .route("/processes/{id}", get(routes::info))
        .route("/processes/{id}/wait", get(routes::wait))
        .route("/processes/{id}/events", get(routes::events))
        .route("/processes/{id}/kill", post(routes::kill))
        .route("/processes/{id}/stdin", post(routes::write_stdin))
        .route("/processes/{id}/terminal", post(routes::resize_terminal))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(processes);
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/files/{*path}",
            get(files::read).put(files::write).delete(files::remove),
        )
        .route(
            "/archives/{*path}",
            get(files::read_archive).put(files::write_archive),
        )
        .route("/ports/{port}", get(ports::tunnel))
        .merge(process_routes)
}

async fn metrics(State(processes): State<Arc<Processes>>) -> impl IntoResponse {
    let (running, retained) = tokio::task::spawn_blocking(move || processes.counts())
        .await
        .expect("process metrics task panicked");
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        format!(
            "# TYPE agentd_processes_running gauge\nagentd_processes_running {running}\n# TYPE agentd_processes_retained gauge\nagentd_processes_retained {retained}\n"
        ),
    )
}
