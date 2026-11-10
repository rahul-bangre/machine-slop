use std::time::{Duration, Instant};

use axum::{
    body::{Body, Bytes},
    http::{Method, Response, StatusCode, header},
};
use http_body_util::BodyExt;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use slop_machines::{Generation, HostReport};

pub const REPORTING_WINDOW: Duration = Duration::from_secs(10);
const ANSWER_LIMIT: Duration = Duration::from_secs(65 * 60);
pub const TENANT_HEADER: &str = "slop-tenant";
pub const GENERATION_HEADER: &str = "slop-generation";

pub struct Reported {
    pub report: HostReport,
    pub at: Instant,
}

impl Reported {
    pub fn is_fresh(&self) -> bool {
        self.at.elapsed() <= REPORTING_WINDOW
    }
}

pub enum Sent {
    NotDelivered(String),
    Unknown(String),
    Answered(Response<Bytes>),
}

pub struct Hosts {
    client: Client<HttpConnector, Body>,
    control_key: String,
}

pub struct Call<'a> {
    pub host: &'a str,
    pub tenant: &'a str,
    pub method: Method,
    pub path: &'a str,
    pub key: Option<&'a str>,
    pub body: Bytes,
}

impl Hosts {
    pub fn new(control_key: String) -> Self {
        Self {
            client: Client::builder(hyper_util::rt::TokioExecutor::new()).build_http(),
            control_key,
        }
    }

    pub fn client(&self) -> &Client<HttpConnector, Body> {
        &self.client
    }

    pub fn authorization(&self) -> String {
        format!("Bearer {}", self.control_key)
    }

    pub async fn call(&self, call: Call<'_>) -> Sent {
        let mut request = hyper::Request::builder()
            .method(call.method)
            .uri(format!("{}{}", call.host, call.path))
            .header(header::AUTHORIZATION, self.authorization())
            .header(TENANT_HEADER, call.tenant)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = call.key {
            request = request.header("idempotency-key", key);
        }
        let request = match request.body(Body::from(call.body)) {
            Ok(request) => request,
            Err(error) => return Sent::NotDelivered(error.to_string()),
        };
        let host = call.host;
        let answered = async {
            let response = match self.client.request(request).await {
                Ok(response) => response,
                Err(error) => return unanswered(host, &error),
            };
            let (parts, body) = response.into_parts();
            match body.collect().await {
                Ok(body) => Sent::Answered(Response::from_parts(parts, body.to_bytes())),
                Err(error) => Sent::Unknown(format!(
                    "{host} did not finish answering, so the request may or may not have taken effect: {error}"
                )),
            }
        };
        tokio::time::timeout(ANSWER_LIMIT, answered)
            .await
            .unwrap_or_else(|_| {
                Sent::Unknown(format!(
                    "{host} did not answer within {} minutes, so the request may or may not have taken effect",
                    ANSWER_LIMIT.as_secs() / 60
                ))
            })
    }
}

pub fn unanswered(host: &str, error: &hyper_util::client::legacy::Error) -> Sent {
    if error.is_connect() {
        Sent::NotDelivered(format!("{host} is unreachable: {error}"))
    } else {
        Sent::Unknown(format!(
            "{host} did not answer, so the request may or may not have taken effect: {error}"
        ))
    }
}

pub fn generation_of(response: &Response<Bytes>) -> Generation {
    response
        .headers()
        .get(GENERATION_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once('.'))
        .and_then(|(boot, change)| {
            Some(Generation {
                boot: boot.parse().ok()?,
                change: change.parse().ok()?,
            })
        })
        .unwrap_or_default()
}

pub fn is_capacity_refusal(response: &Response<Bytes>) -> bool {
    response.status() == StatusCode::SERVICE_UNAVAILABLE
}
