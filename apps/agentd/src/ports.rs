use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::{
    body::Body,
    extract::{Path, Request},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

use crate::error::ApiError;

const PROTOCOL: &str = "tcp";

pub async fn tunnel(Path(port): Path<u16>, mut request: Request) -> Result<Response, ApiError> {
    if !requests_tcp_upgrade(request.headers()) {
        return Ok(Response::builder()
            .status(StatusCode::UPGRADE_REQUIRED)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, PROTOCOL)
            .body(Body::from("send Connection: upgrade and Upgrade: tcp"))
            .expect("static response is valid"));
    }
    if port == 0 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "port must be between 1 and 65535".into(),
        ));
    }
    let target = TcpStream::connect(&loopback(port)[..])
        .await
        .map_err(|error| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                format!("cannot connect to port {port} on loopback: {error}"),
            )
        })?;
    let upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        if let Ok(upgraded) = upgrade.await {
            relay_until_either_side_ends(TokioIo::new(upgraded), target).await;
        }
    });
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, PROTOCOL)
        .body(Body::empty())
        .expect("static response is valid"))
}

async fn relay_until_either_side_ends(
    client: impl AsyncRead + AsyncWrite + Unpin,
    mut target: TcpStream,
) {
    let (mut from_client, mut to_client) = tokio::io::split(client);
    let (mut from_target, mut to_target) = target.split();
    tokio::select! {
        _ = tokio::io::copy(&mut from_client, &mut to_target) => {}
        _ = async {
            let _ = tokio::io::copy(&mut from_target, &mut to_client).await;
            let _ = to_client.shutdown().await;
        } => {}
    }
}

fn loopback(port: u16) -> [SocketAddr; 2] {
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
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
    has_token(header::CONNECTION, "upgrade") && has_token(header::UPGRADE, PROTOCOL)
}
