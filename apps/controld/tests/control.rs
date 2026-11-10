use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use sha2::Digest;
use slop_machines::{
    ExecutionUsage, Generation, HostReport, MachineState, ReportedMachine, Room, Store,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const POOL: &str = "test pool";
const CONTROL_KEY: &str = "control-key";
const TENANT_KEY: &str = "tenant-key";
const OTHER_KEY: &str = "other-key";

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("controld-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }

    fn store_url(&self) -> String {
        format!("file://{}", self.0.join("store").display())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Creates {
    Succeed,
    Full,
    WithoutSnapshots,
}

struct FakeMachine {
    tenant: String,
    key: Option<String>,
    running: bool,
}

#[derive(Default)]
struct FakeState {
    machines: HashMap<String, FakeMachine>,
    abandoned: HashSet<String>,
    made: usize,
    change: u64,
    seen: Vec<String>,
}

#[derive(Clone)]
struct FakeHost {
    name: &'static str,
    creates: Creates,
    state: Arc<Mutex<FakeState>>,
}

fn machine_json(id: &str, running: bool) -> Value {
    json!({
        "id": id,
        "state": { "state": if running { "running" } else { "paused" } },
        "vcpus": 2,
        "memory_mib": 512,
        "boot_ms": 1,
        "idle_timeout_secs": 600,
        "allowed_hosts": ["*"],
        "credential_hosts": [],
        "egress_address": null,
    })
}

fn tenant_of(headers: &HeaderMap) -> String {
    let presented = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    assert_eq!(
        presented,
        Some(format!("Bearer {CONTROL_KEY}").as_str()),
        "controld presents the control key to hosts"
    );
    headers
        .get("slop-tenant")
        .and_then(|value| value.to_str().ok())
        .expect("controld names the tenant")
        .to_owned()
}

impl FakeHost {
    fn new(name: &'static str, creates: Creates) -> Self {
        Self {
            name,
            creates,
            state: Arc::default(),
        }
    }

    fn seen(&self) -> Vec<String> {
        self.state.lock().expect("state").seen.clone()
    }

    fn machine_count(&self) -> usize {
        self.state.lock().expect("state").machines.len()
    }

    fn remove(&self, id: &str) {
        let mut state = self.state.lock().expect("state");
        state.machines.remove(id);
        state.change += 1;
    }

    fn stamped(&self, status: StatusCode, body: Value) -> Response {
        let change = self.state.lock().expect("state").change;
        (
            status,
            [("slop-generation", format!("1.{change}"))],
            Json(body),
        )
            .into_response()
    }

    fn router(self) -> Router {
        Router::new()
            .route("/machines", get(list).post(create))
            .route("/machines/{id}", get(info).delete(destroy))
            .route("/machines/{id}/pause", post(pause))
            .route("/machines/{id}/fork", post(fork))
            .route("/operations/{key}/settle", post(settle))
            .route("/snapshots", post(slow_snapshot))
            .with_state(self)
    }

    fn report(&self, url: &str, store: &str, executions: Vec<ExecutionUsage>) -> HostReport {
        let mut state = self.state.lock().expect("state");
        state.change += 1;
        HostReport {
            url: url.to_owned(),
            pool: POOL.into(),
            store: store.to_owned(),
            room: Room {
                slots: 8,
                memory_mib: if self.name == "small" { 1024 } else { 4096 },
            },
            egress_addresses: Vec::new(),
            generation: Generation {
                boot: 1,
                change: state.change,
            },
            machines: state
                .machines
                .iter()
                .map(|(id, machine)| ReportedMachine {
                    id: id.clone(),
                    tenant: machine.tenant.clone(),
                    memory_mib: 512,
                    state: if machine.running {
                        MachineState::Running
                    } else {
                        MachineState::Paused
                    },
                })
                .collect(),
            executions,
        }
    }
}

async fn list(State(host): State<FakeHost>, headers: HeaderMap) -> Json<Value> {
    let tenant = tenant_of(&headers);
    let state = host.state.lock().expect("state");
    Json(Value::Array(
        state
            .machines
            .iter()
            .filter(|(_, machine)| machine.tenant == tenant)
            .map(|(id, machine)| machine_json(id, machine.running))
            .collect(),
    ))
}

async fn create(State(host): State<FakeHost>, headers: HeaderMap, body: Bytes) -> Response {
    let tenant = tenant_of(&headers);
    let key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut state = host.state.lock().expect("state");
    state
        .seen
        .push(format!("POST /machines {}", String::from_utf8_lossy(&body)));
    if host.creates == Creates::Full {
        return (StatusCode::SERVICE_UNAVAILABLE, "machine capacity reached").into_response();
    }
    if let Some(key) = &key {
        if state.abandoned.contains(key) {
            return (StatusCode::CONFLICT, "settled as having made nothing").into_response();
        }
        let replayed = state
            .machines
            .iter()
            .find(|(_, machine)| machine.key.as_ref() == Some(key))
            .map(|(id, _)| id.clone());
        if let Some(id) = replayed {
            drop(state);
            return host.stamped(StatusCode::CREATED, machine_json(&id, true));
        }
    }
    let id = format!("{}-{}", host.name, state.made);
    state.made += 1;
    state.machines.insert(
        id.clone(),
        FakeMachine {
            tenant,
            key,
            running: true,
        },
    );
    state.change += 1;
    drop(state);
    host.stamped(StatusCode::CREATED, machine_json(&id, true))
}

async fn info(
    State(host): State<FakeHost>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let tenant = tenant_of(&headers);
    let running = {
        let mut state = host.state.lock().expect("state");
        state.seen.push(format!("GET /machines/{id}"));
        state
            .machines
            .get(&id)
            .filter(|machine| machine.tenant == tenant)
            .map(|machine| machine.running)
    };
    match running {
        Some(running) => host.stamped(StatusCode::OK, machine_json(&id, running)),
        None => (StatusCode::NOT_FOUND, "unknown machine").into_response(),
    }
}

async fn destroy(
    State(host): State<FakeHost>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    tenant_of(&headers);
    let removed = {
        let mut state = host.state.lock().expect("state");
        state.change += 1;
        state.machines.remove(&id).is_some()
    };
    if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "unknown machine").into_response()
    }
}

async fn pause(
    State(host): State<FakeHost>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    tenant_of(&headers);
    {
        let mut state = host.state.lock().expect("state");
        if let Some(machine) = state.machines.get_mut(&id) {
            machine.running = false;
        }
        state.change += 1;
    }
    host.stamped(StatusCode::OK, machine_json(&id, false))
}

async fn fork(
    State(host): State<FakeHost>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let tenant = tenant_of(&headers);
    let fork = format!("{id}-fork");
    {
        let mut state = host.state.lock().expect("state");
        state.seen.push(format!("POST /machines/{id}/fork"));
        state.machines.insert(
            fork.clone(),
            FakeMachine {
                tenant,
                key: None,
                running: true,
            },
        );
        state.change += 1;
    }
    host.stamped(StatusCode::CREATED, machine_json(&fork, true))
}

async fn settle(
    State(host): State<FakeHost>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    tenant_of(&headers);
    let made = {
        let mut state = host.state.lock().expect("state");
        state.seen.push(format!("settle {key}"));
        let made = state
            .machines
            .iter()
            .find(|(_, machine)| machine.key.as_ref() == Some(&key))
            .map(|(id, machine)| (id.clone(), machine.running));
        if made.is_none() {
            state.abandoned.insert(key);
        }
        made
    };
    match made {
        Some((id, running)) => host.stamped(StatusCode::OK, machine_json(&id, running)),
        None => (StatusCode::NOT_FOUND, "nothing exists").into_response(),
    }
}

async fn slow_snapshot(State(host): State<FakeHost>, headers: HeaderMap, body: Bytes) -> Response {
    tenant_of(&headers);
    if host.creates == Creates::WithoutSnapshots {
        return (StatusCode::BAD_GATEWAY, "snapshot store: unavailable").into_response();
    }
    let operation = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .expect("a snapshot carries its operation")
        .to_owned();
    let request: Value = serde_json::from_slice(&body).expect("snapshot request");
    tokio::time::sleep(controld::RECONCILE_INTERVAL + Duration::from_secs(2)).await;
    (
        StatusCode::CREATED,
        Json(json!({
            "id": operation,
            "machine_id": request["machine_id"],
            "vcpus": 2,
            "memory_mib": 512,
            "allowed_hosts": ["*"],
            "created_unix_ms": 1,
            "stored_bytes": 1 << 20,
            "pool": POOL,
        })),
    )
        .into_response()
}

async fn serve(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = stream.read(&mut buffer).await.unwrap_or(0);
        if read == 0 {
            return request;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            return request;
        }
    }
}

const CREATES: &str = "POST /machines HTTP";

#[derive(Clone)]
enum HangUp {
    AfterTheHostAnswers(String),
    BeforeTheHostHears(String),
}

impl HangUp {
    fn on(&self, request: &str) -> bool {
        match self {
            Self::AfterTheHostAnswers(start) | Self::BeforeTheHostHears(start) => {
                request.starts_with(start.as_str())
            }
        }
    }
}

fn after_answer(start: &str) -> Option<HangUp> {
    Some(HangUp::AfterTheHostAnswers(start.to_owned()))
}

async fn serve_hanging_up_on(router: Router, hang_up: Arc<Mutex<Option<HangUp>>>) -> String {
    let inner = serve(router).await;
    let inner = inner.trim_start_matches("http://").to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else {
                return;
            };
            let inner = inner.clone();
            let hang_up = hang_up.clone();
            tokio::spawn(async move {
                let request = read_request(&mut client).await;
                let request = String::from_utf8_lossy(&request).replacen(
                    "\r\n",
                    "\r\nConnection: close\r\n",
                    1,
                );
                let hang = hang_up
                    .lock()
                    .expect("flag")
                    .clone()
                    .filter(|hang| hang.on(&request));
                if matches!(hang, Some(HangUp::BeforeTheHostHears(_))) {
                    return;
                }
                let mut upstream = TcpStream::connect(&inner).await.expect("inner host");
                upstream
                    .write_all(request.as_bytes())
                    .await
                    .expect("forward");
                let mut response = Vec::new();
                let _ = upstream.read_to_end(&mut response).await;
                if hang.is_none() {
                    let _ = client.write_all(&response).await;
                }
            });
        }
    });
    format!("http://{address}")
}

fn digest(key: &str) -> String {
    sha2::Sha256::digest(key.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Control {
    url: String,
    store_url: String,
    _scratch: Scratch,
}

impl Control {
    async fn start(name: &str, running_machines: u32) -> Self {
        let scratch = Scratch::new(name);
        let quotas = json!({
            "running_machines": running_machines,
            "memory_mib": 1 << 20,
            "disk_gib": 1 << 20,
            "creates_per_second": 1000,
        });
        let tenants = json!({ "tenants": [
            { "id": "tenant", "api_key_sha256": [digest(TENANT_KEY)], "quotas": quotas },
            { "id": "other", "api_key_sha256": [digest(OTHER_KEY)], "quotas": quotas },
        ]});
        let (tenants, keys) = slop_machines::parse_tenants(&tenants.to_string()).expect("tenants");
        let store_url = scratch.store_url();
        let control = controld::Control::open(controld::Config {
            store: Store::open(&store_url).expect("store"),
            state_dir: scratch.0.join("state"),
            tenants,
            keys,
            control_key: CONTROL_KEY.into(),
        })
        .await
        .expect("open control");
        tokio::spawn(control.clone().reconcile_periodically());
        Self {
            url: serve(controld::app(control)).await,
            store_url,
            _scratch: scratch,
        }
    }

    async fn report(&self, host: &FakeHost, url: &str) -> Value {
        self.report_with(host.report(url, &self.store_url, Vec::new()))
            .await
    }

    async fn report_with(&self, report: HostReport) -> Value {
        let (status, reply) = self
            .send("POST", "/hosts", Some(json!(report)), CONTROL_KEY, None)
            .await;
        assert_eq!(status, 200, "report: {reply}");
        reply
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        self.send(method, path, body, TENANT_KEY, None).await
    }

    async fn keyed(&self, path: &str, key: &str) -> (u16, Value) {
        self.send("POST", path, None, TENANT_KEY, Some(key)).await
    }

    async fn usage(&self) -> u64 {
        let (status, usage) = self.request("GET", "/usage", None).await;
        assert_eq!(status, 200, "{usage}");
        usage["vcpu_ms"].as_u64().expect("vcpu_ms")
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        api_key: &str,
        idempotency_key: Option<&str>,
    ) -> (u16, Value) {
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        let address = self.url.trim_start_matches("http://");
        let idempotency = idempotency_key
            .map(|key| format!("Idempotency-Key: {key}\r\n"))
            .unwrap_or_default();
        let exchange = async {
            let mut stream = TcpStream::connect(address).await.expect("connect");
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: controld\r\nAuthorization: Bearer {api_key}\r\n{idempotency}Connection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(request.as_bytes()).await.expect("write");
            let mut response = String::new();
            stream.read_to_string(&mut response).await.expect("read");
            response
        };
        let response = tokio::time::timeout(Duration::from_secs(20), exchange)
            .await
            .expect("controld answers within 20 seconds");
        let (head, body) = response.split_once("\r\n\r\n").expect("http response");
        let status = head[9..12].parse().expect("status");
        let body = serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_owned()));
        (status, body)
    }
}

#[tokio::test]
async fn quotas_hold_across_the_fleet_and_concurrent_creates_share_them() {
    let control = Control::start("fleet-quota", 2).await;
    let (a, b) = (
        FakeHost::new("small", Creates::Succeed),
        FakeHost::new("large", Creates::Succeed),
    );
    let a_url = serve(a.clone().router()).await;
    let b_url = serve(b.clone().router()).await;
    control.report(&a, &a_url).await;
    control.report(&b, &b_url).await;

    let creates =
        futures_util::future::join_all((0..4).map(|_| control.request("POST", "/machines", None)))
            .await;
    let made = creates.iter().filter(|(status, _)| *status == 201).count();
    let refused = creates.iter().filter(|(status, _)| *status == 429).count();
    assert_eq!((made, refused), (2, 2), "{creates:?}");
    assert_eq!(a.machine_count() + b.machine_count(), 2);

    let (status, other) = control
        .send("POST", "/machines", None, OTHER_KEY, None)
        .await;
    assert_eq!(status, 201, "another tenant has its own budget: {other}");

    let (status, listed) = control.request("GET", "/machines", None).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed.as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn a_full_host_is_skipped_and_a_deleted_machine_returns_its_quota() {
    let control = Control::start("placement", 1).await;
    let (full, small) = (
        FakeHost::new("large", Creates::Full),
        FakeHost::new("small", Creates::Succeed),
    );
    let full_url = serve(full.clone().router()).await;
    let small_url = serve(small.clone().router()).await;
    control.report(&full, &full_url).await;
    control.report(&small, &small_url).await;
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["id"], "small-0");
    assert_eq!(full.seen().len(), 1);
    let (status, _) = control.request("DELETE", "/machines/small-0", None).await;
    assert_eq!(status, 204);
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "the quota came back: {created}");
}

#[tokio::test]
async fn a_lost_answer_keeps_its_reservation_and_a_keyed_retry_settles_it_on_the_same_host() {
    let control = Control::start("lost-answer", 1).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let spare = FakeHost::new("small", Creates::Succeed);
    let hang_up = Arc::new(Mutex::new(after_answer(CREATES)));
    let host_url = serve_hanging_up_on(host.clone().router(), hang_up.clone()).await;
    let spare_url = serve(spare.clone().router()).await;
    control.report(&host, &host_url).await;
    control.report(&spare, &spare_url).await;

    let (status, error) = control.keyed("/machines", "retry-me").await;
    assert_eq!(status, 502, "{error}");
    assert_eq!(host.machine_count(), 1, "the host made the machine");
    let (status, _) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 429, "the uncertain create still holds the quota");

    *hang_up.lock().expect("flag") = None;
    let (status, created) = control.keyed("/machines", "retry-me").await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["id"], "large-0");
    assert_eq!(host.machine_count(), 1);
    assert!(spare.seen().is_empty(), "the retry never went elsewhere");
    let (status, again) = control.keyed("/machines", "retry-me").await;
    assert_eq!((status, &again["id"]), (201, &json!("large-0")));
    let (status, _) = control
        .send(
            "POST",
            "/machines",
            Some(json!({ "memory_mib": 1024 })),
            TENANT_KEY,
            Some("retry-me"),
        )
        .await;
    assert_eq!(status, 422, "a key belongs to one request");
}

#[tokio::test]
async fn a_lost_create_that_made_nothing_is_fenced_and_its_key_tries_again() {
    let control = Control::start("lost-nothing", 1).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let hang_up = Arc::new(Mutex::new(after_answer(CREATES)));
    let host_url = serve_hanging_up_on(host.clone().router(), hang_up.clone()).await;
    control.report(&host, &host_url).await;
    let (status, _) = control.keyed("/machines", "fenced").await;
    assert_eq!(status, 502);
    host.remove("large-0");
    *hang_up.lock().expect("flag") = None;
    let (status, created) = control.keyed("/machines", "fenced").await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["id"], "large-1");
    let settles = host
        .seen()
        .iter()
        .filter(|seen| seen.starts_with("settle "))
        .count();
    assert_eq!(settles, 1);
    assert_eq!(host.state.lock().expect("state").abandoned.len(), 1);
}

#[tokio::test]
async fn a_host_that_stops_answering_keeps_its_reservations_until_it_reports_again() {
    let control = Control::start("unreachable", 1).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    let router = host.clone().router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    control.report(&host, &url).await;
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "{created}");
    server.abort();
    let _ = server.await;
    let (status, _) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 429, "a silent host's machines keep their quota");
    let (status, _) = control.request("GET", "/machines", None).await;
    assert_eq!(status, 502);

    host.remove("large-0");
    let mut restarted = host.report(&url, &control.store_url, Vec::new());
    restarted.generation.boot = 2;
    control.report_with(restarted).await;
    let (status, refused) = control.request("POST", "/machines", None).await;
    assert_eq!(
        status, 503,
        "the quota is free once the host reports the machine gone; only the host is down: {refused}"
    );
}

#[tokio::test]
async fn reports_release_only_what_they_have_seen_and_never_count_usage_twice() {
    let control = Control::start("reports", 1).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let url = serve(host.clone().router()).await;
    let stale = host.report(&url, &control.store_url, Vec::new());
    control.report(&host, &url).await;
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "{created}");
    control.report_with(stale).await;
    let (status, _) = control.request("POST", "/machines", None).await;
    assert_eq!(
        status, 429,
        "a report older than the create does not release it"
    );

    let execution = |vcpu_ms: u64, ended: bool| ExecutionUsage {
        id: "execution".into(),
        tenant: "tenant".into(),
        machine_id: "large-0".into(),
        vcpu_ms,
        memory_mib_ms: vcpu_ms * 256,
        ended,
    };
    control
        .report_with(host.report(&url, &control.store_url, vec![execution(100, false)]))
        .await;
    assert_eq!(control.usage().await, 100);
    let reply = control
        .report_with(host.report(&url, &control.store_url, vec![execution(300, true)]))
        .await;
    assert_eq!(reply["recorded_executions"], json!(["execution"]));
    control
        .report_with(host.report(&url, &control.store_url, vec![execution(300, true)]))
        .await;
    assert_eq!(control.usage().await, 300);
    host.remove("large-0");
    control
        .report_with(host.report(&url, &control.store_url, Vec::new()))
        .await;
    assert_eq!(
        control.usage().await,
        300,
        "a forgotten execution stays counted once"
    );
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(
        status, 201,
        "a newer report without the machine releases it: {created}"
    );
}

#[tokio::test]
async fn machines_are_found_on_their_host_and_forks_stay_with_it() {
    let control = Control::start("routes", 4).await;
    let (first, second) = (
        FakeHost::new("small", Creates::Succeed),
        FakeHost::new("large", Creates::Succeed),
    );
    let first_url = serve(first.clone().router()).await;
    let second_url = serve(second.clone().router()).await;
    control.report(&first, &first_url).await;
    control.report(&second, &second_url).await;
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "{created}");
    let id = created["id"].as_str().expect("id").to_owned();
    let (status, fork) = control
        .request("POST", &format!("/machines/{id}/fork"), None)
        .await;
    assert_eq!(status, 201, "{fork}");
    let fork_request = format!("POST /machines/{id}/fork");
    assert_eq!(
        second.seen().last().map(String::as_str),
        Some(fork_request.as_str())
    );
    let (status, _) = control
        .send("GET", &format!("/machines/{id}"), None, OTHER_KEY, None)
        .await;
    assert_eq!(status, 404, "another tenant's machine is unknown");
    let (status, paused) = control
        .request("POST", &format!("/machines/{id}/pause"), None)
        .await;
    assert_eq!((status, &paused["state"]["state"]), (200, &json!("paused")));
}

#[tokio::test]
async fn removing_a_host_releases_what_it_held_and_hosts_are_for_the_operator() {
    let control = Control::start("removal", 1).await;
    let host = FakeHost::new("small", Creates::Succeed);
    let url = serve(host.clone().router()).await;
    control.report(&host, &url).await;
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "{created}");
    let (status, _) = control
        .send(
            "DELETE",
            &format!("/hosts?url={url}"),
            None,
            CONTROL_KEY,
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (status, _) = control.request("GET", "/machines/small-0", None).await;
    assert_eq!(status, 404);
    let (status, hosts) = control.send("GET", "/hosts", None, CONTROL_KEY, None).await;
    assert_eq!((status, hosts), (200, json!([])));
    let (status, _) = control.send("GET", "/hosts", None, TENANT_KEY, None).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_host_on_another_store_cannot_join() {
    let control = Control::start("membership", 1).await;
    let host = FakeHost::new("small", Creates::Succeed);
    let report = host.report("http://127.0.0.1:1", "file:///elsewhere", Vec::new());
    let (status, error) = control
        .send("POST", "/hosts", Some(json!(report)), CONTROL_KEY, None)
        .await;
    assert_eq!(status, 409, "{error}");
    let report = host.report("http://127.0.0.1:1", &control.store_url, Vec::new());
    let (status, _) = control
        .send("POST", "/hosts", Some(json!(report)), TENANT_KEY, None)
        .await;
    assert_eq!(status, 401, "only hosts holding the control key report");
}

#[tokio::test]
async fn an_operation_still_in_flight_is_never_settled_behind_its_back() {
    let control = Control::start("in-flight", 4).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let url = serve(host.clone().router()).await;
    control.report(&host, &url).await;
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "{created}");
    let (status, snapshot) = control
        .request(
            "POST",
            "/snapshots",
            Some(json!({ "machine_id": created["id"] })),
        )
        .await;
    assert_eq!(status, 201, "{snapshot}");
    let records = control._scratch.0.join("store/snapshots");
    assert!(
        std::fs::read_dir(&records).map_or(0, Iterator::count) == 0,
        "no snapshot was abandoned while its host was still taking it"
    );
}

#[tokio::test]
async fn a_machine_with_a_request_in_doubt_takes_no_other_request() {
    let control = Control::start("busy", 4).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let hang_up = Arc::new(Mutex::new(None));
    let url = serve_hanging_up_on(host.clone().router(), hang_up.clone()).await;
    control.report(&host, &url).await;
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "{created}");
    *hang_up.lock().expect("flag") = after_answer("POST /machines/large-0/pause");
    let (status, _) = control
        .request("POST", "/machines/large-0/pause", None)
        .await;
    assert_eq!(status, 502);
    let (status, refused) = control
        .request("POST", "/machines/large-0/resume", None)
        .await;
    assert_eq!(status, 409, "{refused}");
    let (status, refused) = control.request("DELETE", "/machines/large-0", None).await;
    assert_eq!(status, 409, "{refused}");
}

#[tokio::test]
async fn a_report_older_than_one_already_applied_changes_nothing() {
    let control = Control::start("stale-report", 4).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let url = serve(host.clone().router()).await;
    control.report(&host, &url).await;
    let running = ExecutionUsage {
        id: "execution".into(),
        tenant: "tenant".into(),
        machine_id: "gone".into(),
        vcpu_ms: 100,
        memory_mib_ms: 100,
        ended: false,
    };
    let ended = ExecutionUsage {
        vcpu_ms: 300,
        ended: true,
        ..running.clone()
    };
    let delayed = host.report(&url, &control.store_url, vec![running]);
    let reply = control
        .report_with(host.report(&url, &control.store_url, vec![ended]))
        .await;
    assert_eq!(reply["recorded_executions"], json!(["execution"]));
    control.report(&host, &url).await;
    let reply = control.report_with(delayed).await;
    assert_eq!(reply["recorded_executions"], json!([]));
    assert_eq!(control.usage().await, 300);
}

#[tokio::test]
async fn a_snapshot_answered_with_a_store_failure_is_settled_in_the_store() {
    let control = Control::start("snapshot-doubt", 4).await;
    let host = FakeHost::new("large", Creates::WithoutSnapshots);
    let url = serve(host.clone().router()).await;
    control.report(&host, &url).await;
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "{created}");
    let (status, refused) = control
        .send(
            "POST",
            "/snapshots",
            Some(json!({ "machine_id": created["id"] })),
            TENANT_KEY,
            Some("snapshot-key"),
        )
        .await;
    assert_eq!(status, 500, "{refused}");
    let records: Vec<_> = std::fs::read_dir(control._scratch.0.join("store/snapshots"))
        .expect("the settle wrote to the store")
        .map(|entry| entry.expect("entry").path().join("record.json"))
        .collect();
    assert_eq!(records.len(), 1);
    let record = std::fs::read_to_string(&records[0]).expect("record");
    assert!(
        record.contains("abandoned"),
        "a late upload is fenced: {record}"
    );
}

#[tokio::test]
async fn a_host_that_lost_its_state_is_refused_until_it_is_removed() {
    let control = Control::start("lost-state", 4).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let url = serve(host.clone().router()).await;
    let mut report = host.report(&url, &control.store_url, Vec::new());
    report.generation.boot = 2;
    control.report_with(report).await;
    let reborn = host.report(&url, &control.store_url, Vec::new());
    let (status, refused) = control
        .send("POST", "/hosts", Some(json!(reborn)), CONTROL_KEY, None)
        .await;
    assert_eq!(status, 409, "{refused}");
    tokio::time::sleep(controld::REPORTING_WINDOW + Duration::from_secs(1)).await;
    let (status, _) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 503, "a refused host gets no placements");
}

#[tokio::test]
async fn a_request_in_doubt_that_gets_any_answer_frees_its_machine() {
    let control = Control::start("doubt-answered", 4).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let hang_up = Arc::new(Mutex::new(None));
    let url = serve_hanging_up_on(host.clone().router(), hang_up.clone()).await;
    control.report(&host, &url).await;
    let (status, created) = control.request("POST", "/machines", None).await;
    assert_eq!(status, 201, "{created}");
    *hang_up.lock().expect("flag") = after_answer("POST /machines/large-0/resume");
    let (status, _) = control
        .request("POST", "/machines/large-0/resume", None)
        .await;
    assert_eq!(status, 502);
    *hang_up.lock().expect("flag") = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        control.report(&host, &url).await;
        let (status, paused) = control
            .request("POST", "/machines/large-0/pause", None)
            .await;
        if status == 200 {
            break;
        }
        assert_eq!(status, 409, "{paused}");
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resume in doubt was never settled"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
async fn a_fork_that_never_reached_its_host_settles_without_touching_its_source() {
    let control = Control::start("fork-unsent", 2).await;
    let host = FakeHost::new("large", Creates::Succeed);
    let hang_up = Arc::new(Mutex::new(None));
    let url = serve_hanging_up_on(host.clone().router(), hang_up.clone()).await;
    control.report(&host, &url).await;
    let (status, created) = control.keyed("/machines", "source-key").await;
    assert_eq!(status, 201, "{created}");
    *hang_up.lock().expect("flag") = Some(HangUp::BeforeTheHostHears(
        "POST /machines/large-0/fork".to_owned(),
    ));
    let (status, _) = control.keyed("/machines/large-0/fork", "fork-key").await;
    assert_eq!(status, 502);
    *hang_up.lock().expect("flag") = None;
    let (status, _) = control.keyed("/machines/large-0/fork", "fork-key").await;
    assert_eq!(
        status, 201,
        "the settled fork made nothing, so its key tries again"
    );
    let (status, refused) = control.request("POST", "/machines", None).await;
    assert_eq!(
        status, 429,
        "the source and its fork both still hold quota: {refused}"
    );
    let (status, again) = control.keyed("/machines", "source-key").await;
    assert_eq!(
        (status, &again["id"]),
        (201, &json!("large-0")),
        "the source keeps its key"
    );
}
