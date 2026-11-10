use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use hyper::{body::Incoming, service::service_fn, upgrade::Upgraded};
use hyper_util::rt::{TokioIo, TokioTimer};
use slop_machines::{MAX_PREVIEW_CONNECTIONS, Machine, MachineState, Machines, PreviewUse};
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tower::ServiceExt;

use crate::{ApiError, agentd, relay_until_either_side_ends, relay_upgrades, unreachable_agentd};

const TCP: &str = "tcp";
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct ConnectionSlot {
    _permit: Arc<OwnedSemaphorePermit>,
}

struct Lease {
    preview: PreviewUse,
    _slot: Option<ConnectionSlot>,
}

pub async fn serve_previews(listener: TcpListener, machines: Arc<Machines>) {
    let app = Router::new().fallback(serve).with_state(machines);
    let slots = Arc::new(Semaphore::new(MAX_PREVIEW_CONNECTIONS));
    loop {
        let Ok(slot) = slots.clone().acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::error!(%error, "preview accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let slot = ConnectionSlot {
            _permit: Arc::new(slot),
        };
        let app = app.clone();
        let service = service_fn(move |mut request: hyper::Request<Incoming>| {
            request.extensions_mut().insert(slot.clone());
            app.clone().oneshot(request.map(Body::new))
        });
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
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
            None,
        )
    })?;
    match preview.machine.state() {
        MachineState::Running => {}
        MachineState::Paused => {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the machine is paused".into(),
                None,
            ));
        }
        MachineState::Exited { .. } => {
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "the machine has exited".into(),
                None,
            ));
        }
    }
    let preview = Arc::new(Lease {
        preview,
        _slot: request.extensions_mut().remove::<ConnectionSlot>(),
    });
    let tunnel = port_tunnel(
        &preview.preview.machine,
        preview.preview.port,
        preview.clone(),
    )
    .await?;
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
        .map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "malformed request target".into(),
                None,
            )
        })?;
    *request.uri_mut() = origin_form;
    let client_version = request.version();
    let mut response = sender
        .send_request(request)
        .await
        .map_err(unreachable_port)?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let app_upgrade = hyper::upgrade::on(&mut response);
        spawn_until_ended(preview, relay_upgrades(client_upgrade, app_upgrade));
    }
    *response.version_mut() = client_version;
    Ok(response.map(Body::new))
}

fn spawn_until_ended(preview: Arc<Lease>, work: impl Future<Output = ()> + Send + 'static) {
    let ended = preview.preview.ended();
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
            None,
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
        None,
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
