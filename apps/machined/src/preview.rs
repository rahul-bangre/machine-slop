use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use slop_machines::{Machine, MachineState, Machines, PreviewUse};

use crate::{ApiError, agentd, relay_until_either_side_ends, unreachable_agentd};

const TCP: &str = "tcp";
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

pub fn preview_app(machines: Arc<Machines>) -> Router {
    Router::new().fallback(serve).with_state(machines)
}

async fn serve(State(machines): State<Arc<Machines>>, request: Request) -> Response {
    forward(&machines, request)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn forward(machines: &Machines, mut request: Request) -> Result<Response, ApiError> {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .or_else(|| request.uri().host())
        .unwrap_or_default();
    let preview = machines.open_preview(host).await.ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "no such preview, or it has expired".into(),
        )
    })?;
    match preview.machine.state() {
        MachineState::Running => {}
        MachineState::Paused => {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the machine is paused".into(),
            ));
        }
        MachineState::Exited { .. } => {
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "the machine has exited".into(),
            ));
        }
    }
    let preview = Arc::new(preview);
    let tunnel = port_tunnel(&preview.machine, preview.port, preview.clone()).await?;
    if requests_tcp_upgrade(request.headers()) {
        let client = hyper::upgrade::on(&mut request);
        spawn_until_ended(preview, async move {
            if let Ok(client) = client.await {
                relay_until_either_side_ends(TokioIo::new(client), TokioIo::new(tunnel)).await;
            }
        });
        return Ok(Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, TCP)
            .body(Body::empty())
            .expect("static response is valid"));
    }
    let client_upgrade = hyper::upgrade::on(&mut request);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(tunnel)
        .await
        .map_err(unreachable_port)?;
    spawn_until_ended(preview.clone(), async move {
        let _ = connection.with_upgrades().await;
    });
    let origin_form = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .parse()
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "malformed request target".into()))?;
    *request.uri_mut() = origin_form;
    let mut response = sender
        .send_request(request)
        .await
        .map_err(unreachable_port)?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let app_upgrade = hyper::upgrade::on(&mut response);
        spawn_until_ended(preview, async move {
            if let (Ok(client), Ok(app)) = tokio::join!(client_upgrade, app_upgrade) {
                relay_until_either_side_ends(TokioIo::new(client), TokioIo::new(app)).await;
            }
        });
    }
    Ok(response.map(Body::new))
}

fn spawn_until_ended(preview: Arc<PreviewUse>, work: impl Future<Output = ()> + Send + 'static) {
    let ended = preview.ended();
    tokio::spawn(async move {
        let _machine_in_use_until_the_work_ends = preview;
        tokio::select! {
            () = ended => {}
            () = work => {}
        }
    });
}

async fn port_tunnel(
    machine: &Machine,
    port: u16,
    in_use: impl Send + 'static,
) -> Result<Upgraded, ApiError> {
    let mut agentd = agentd(machine, in_use).await?;
    let request = Request::get(format!("/ports/{port}"))
        .header(header::HOST, "agentd")
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, TCP)
        .body(Body::empty())
        .expect("static request is valid");
    let mut response = agentd
        .send_request(request)
        .await
        .map_err(unreachable_agentd)?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status();
        let body = axum::body::to_bytes(Body::new(response.into_body()), MAX_ERROR_BODY_BYTES)
            .await
            .unwrap_or_default();
        return Err(ApiError(
            status,
            String::from_utf8_lossy(&body).into_owned(),
        ));
    }
    hyper::upgrade::on(&mut response)
        .await
        .map_err(unreachable_agentd)
}

fn unreachable_port(error: impl std::fmt::Display) -> ApiError {
    ApiError(
        StatusCode::BAD_GATEWAY,
        format!("the machine's port did not answer: {error}"),
    )
}

fn requests_tcp_upgrade(headers: &HeaderMap) -> bool {
    let has_token = |name, token: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|value: &HeaderValue| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|item| item.trim().eq_ignore_ascii_case(token))
    };
    has_token(header::CONNECTION, "upgrade") && has_token(header::UPGRADE, TCP)
}
