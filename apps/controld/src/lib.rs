mod hosts;
mod ledger;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{FromRequestParts, Path, Query, Request, State},
    http::{Method, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use sha2::Digest;
use slop_machines::{
    ApiKeys, CreateMachine, CreatePreview, CreateSnapshot, DEFAULT_MEMORY_MIB, Generation,
    HostInfo, HostReport, HostReportReply, MachineInfo, MachineState, PreviewInfo, Quotas,
    ReportedMachine, Room, Settled, SnapshotInfo, Store, TenantConfig, TenantUsage,
    provisioned_disk_mib, stored_mib,
};

pub use hosts::REPORTING_WINDOW;
use hosts::{Call, Hosts, Reported, Sent, generation_of, is_capacity_refusal, unanswered};
use ledger::{
    Allocation, Holdings, KeyResult, Keyed, Ledger, Operation, SnapshotCharge, Work, key_of,
};

pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const STORE_RECONCILE_EVERY: u32 = 6;
const MAX_BODY_BYTES: usize = 1 << 20;

pub struct Config {
    pub store: Store,
    pub state_dir: PathBuf,
    pub tenants: Vec<TenantConfig>,
    pub keys: ApiKeys,
    pub control_key: String,
}

pub struct Control {
    store: Store,
    state_dir: PathBuf,
    quotas: HashMap<String, Quotas>,
    keys: ApiKeys,
    control_key: [u8; 32],
    hosts: Hosts,
    ledger: Mutex<Ledger>,
    writer: tokio::sync::Mutex<()>,
    reports: Mutex<HashMap<String, Reported>>,
    turns: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
    performing: Mutex<HashSet<String>>,
    _state_lock: std::fs::File,
}

struct Performing<'a> {
    control: &'a Control,
    operation: String,
}

impl Drop for Performing<'_> {
    fn drop(&mut self) {
        lock(&self.control.performing).remove(&self.operation);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

fn digest(text: &str) -> String {
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn gateway(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, message.into())
}

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("unknown {what}"))
}

fn ledger_failed(error: io::Error) -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("recording the request failed: {error}"),
    )
}

fn store_failed(error: io::Error) -> ApiError {
    gateway(format!("snapshot store: {error}"))
}

fn answered(response: axum::http::Response<Bytes>) -> Response {
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(hosts::GENERATION_HEADER);
    Response::from_parts(parts, Body::from(body))
}

async fn body_of(body: Body) -> Result<Bytes, ApiError> {
    axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))
}

fn json_of<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|error| ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))
}

impl Control {
    pub async fn open(config: Config) -> io::Result<Arc<Self>> {
        tokio::fs::create_dir_all(&config.state_dir).await?;
        tokio::fs::set_permissions(
            &config.state_dir,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .await?;
        let state_lock = std::fs::File::create(config.state_dir.join("controld.lock"))?;
        state_lock.try_lock().map_err(|_| {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another controld is using {}", config.state_dir.display()),
            )
        })?;
        let ledger = Ledger::load(&config.state_dir).await?;
        Ok(Arc::new(Self {
            store: config.store,
            state_dir: config.state_dir,
            quotas: config
                .tenants
                .into_iter()
                .map(|tenant| (tenant.id, tenant.quotas))
                .collect(),
            keys: config.keys,
            control_key: sha2::Sha256::digest(config.control_key.as_bytes()).into(),
            hosts: Hosts::new(config.control_key),
            ledger: Mutex::new(ledger),
            writer: tokio::sync::Mutex::new(()),
            reports: Mutex::default(),
            turns: Mutex::default(),
            buckets: Mutex::default(),
            performing: Mutex::default(),
            _state_lock: state_lock,
        }))
    }

    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        lock(&self.ledger)
    }

    async fn commit<R>(&self, change: impl FnOnce(&mut Ledger) -> R) -> io::Result<R> {
        let _one_writer = self.writer.lock().await;
        let mut next = self.ledger().clone();
        let result = change(&mut next);
        next.as_of_unix_ms = next.as_of_unix_ms.max(unix_ms());
        next.save(&self.state_dir).await?;
        *self.ledger() = next;
        Ok(result)
    }

    fn quotas(&self, tenant: &str) -> Quotas {
        self.quotas
            .get(tenant)
            .cloned()
            .unwrap_or_else(Quotas::none)
    }

    async fn turn(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let turn = {
            let mut turns = lock(&self.turns);
            turns.retain(|_, turn| turn.strong_count() > 0);
            match turns.get(key).and_then(Weak::upgrade) {
                Some(turn) => turn,
                None => {
                    let turn = Arc::new(tokio::sync::Mutex::new(()));
                    turns.insert(key.to_owned(), Arc::downgrade(&turn));
                    turn
                }
            }
        };
        turn.lock_owned().await
    }

    fn take_create(&self, tenant: &str) -> Result<(), Box<Response>> {
        let per_second = f64::from(self.quotas(tenant).creates_per_second);
        if per_second == 0.0 {
            return Err(Box::new(
                ApiError(
                    StatusCode::TOO_MANY_REQUESTS,
                    "the tenant's creates_per_second quota is 0".into(),
                )
                .into_response(),
            ));
        }
        let mut buckets = lock(&self.buckets);
        let now = Instant::now();
        let (tokens, refilled) = buckets
            .entry(tenant.to_owned())
            .or_insert((per_second, now));
        *tokens =
            (*tokens + now.duration_since(*refilled).as_secs_f64() * per_second).min(per_second);
        *refilled = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            return Ok(());
        }
        let retry_after = ((1.0 - *tokens) / per_second).ceil().max(1.0);
        Err(Box::new(
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, format!("{retry_after:.0}"))],
                "the tenant's create rate is exceeded",
            )
                .into_response(),
        ))
    }

    fn return_create(&self, tenant: &str) {
        let limit = f64::from(self.quotas(tenant).creates_per_second);
        if let Some((tokens, _)) = lock(&self.buckets).get_mut(tenant) {
            *tokens = (*tokens + 1.0).min(limit);
        }
    }

    fn reporting(&self, host: &str) -> bool {
        lock(&self.reports)
            .get(host)
            .is_some_and(Reported::is_fresh)
    }

    fn every_host(&self) -> Result<Vec<String>, ApiError> {
        let hosts: Vec<_> = self.ledger().hosts.keys().cloned().collect();
        let silent: Vec<_> = hosts
            .iter()
            .filter(|host| !self.reporting(host))
            .cloned()
            .collect();
        if silent.is_empty() {
            Ok(hosts)
        } else {
            Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "hosts {} have not reported in the last {} seconds, so the answer could be incomplete; wait for them or remove them with DELETE /hosts?url=",
                    silent.join(", "),
                    REPORTING_WINDOW.as_secs()
                ),
            ))
        }
    }

    fn candidates(
        &self,
        memory_mib: u32,
        pool: Option<&str>,
        egress_address: Option<&str>,
    ) -> Vec<String> {
        let reports = lock(&self.reports);
        let mut fitting: Vec<_> = reports
            .iter()
            .filter(|(_, reported)| reported.is_fresh())
            .map(|(url, reported)| (url, &reported.report))
            .filter(|(_, report)| pool.is_none_or(|pool| report.pool == pool))
            .filter(|(_, report)| {
                egress_address
                    .is_none_or(|address| report.egress_addresses.iter().any(|own| own == address))
            })
            .filter(|(_, report)| report.room.slots > 0 && report.room.memory_mib >= memory_mib)
            .map(|(url, report)| (report.room.memory_mib, url.clone()))
            .collect();
        fitting.sort_unstable_by(|a, b| b.cmp(a));
        fitting.into_iter().map(|(_, url)| url).collect()
    }

    fn placed(&self, host: &str, memory_mib: u32) {
        if let Some(reported) = lock(&self.reports).get_mut(host) {
            let room = &mut reported.report.room;
            *room = Room {
                slots: room.slots.saturating_sub(1),
                memory_mib: room.memory_mib.saturating_sub(memory_mib),
            };
        }
    }

    fn machine(&self, tenant: &str, id: &str) -> Result<Allocation, ApiError> {
        self.ledger()
            .machines
            .get(id)
            .filter(|machine| machine.tenant == tenant)
            .cloned()
            .ok_or_else(|| not_found("machine"))
    }

    async fn perform(
        self: &Arc<Self>,
        tenant: &str,
        key: Option<String>,
        work: Work,
        holds: Holdings,
        hosts: Vec<String>,
        request: Request<Bytes>,
    ) -> Result<Response, ApiError> {
        let ledger_key = key.as_ref().map(|key| key_of(tenant, key));
        let fingerprint = digest(&serde_json::to_string(&work).unwrap_or_default());
        let _turn = match &ledger_key {
            Some(ledger_key) => Some(self.turn(ledger_key).await),
            None => None,
        };
        if let Some(ledger_key) = &ledger_key
            && let Some(replayed) = self
                .replay(tenant, ledger_key, &fingerprint, key.as_deref())
                .await?
        {
            return Ok(replayed);
        }
        let Some(first) = hosts.first().cloned() else {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("no reporting host has room for {} MiB", holds.memory_mib),
            ));
        };
        if matches!(work, Work::Create { .. } | Work::Fork { .. })
            && let Err(refused) = self.take_create(tenant)
        {
            return Ok(*refused);
        }
        let operation_id = uuid::Uuid::new_v4().simple().to_string();
        lock(&self.performing).insert(operation_id.clone());
        let _performing = Performing {
            control: self,
            operation: operation_id.clone(),
        };
        let quotas = self.quotas(tenant);
        let recorded = self
            .commit(|ledger| {
                if let Some(machine) = work.machine()
                    && ledger.machine_is_busy(machine)
                {
                    return Err((
                        StatusCode::CONFLICT,
                        format!(
                            "an earlier request on machine {machine} is still in doubt; retry once it is settled"
                        ),
                    ));
                }
                ledger
                    .admits(tenant, &quotas, holds)
                    .map_err(|refusal| (StatusCode::TOO_MANY_REQUESTS, refusal))?;
                ledger.operations.insert(
                    operation_id.clone(),
                    Operation {
                        tenant: tenant.to_owned(),
                        work: work.clone(),
                        host: first.clone(),
                        holds,
                        key: ledger_key.clone(),
                    },
                );
                if let Some(ledger_key) = &ledger_key {
                    ledger.keys.insert(
                        ledger_key.clone(),
                        Keyed {
                            request: fingerprint.clone(),
                            result: KeyResult::Pending {
                                operation: operation_id.clone(),
                            },
                        },
                    );
                }
                Ok::<_, (StatusCode, String)>(())
            })
            .await
            .map_err(ledger_failed)?;
        if let Err((status, refusal)) = recorded {
            if matches!(work, Work::Create { .. } | Work::Fork { .. }) {
                self.return_create(tenant);
            }
            return Err(ApiError(status, refusal));
        }
        let (parts, body) = request.into_parts();
        let path = parts
            .uri
            .path_and_query()
            .map_or("/", |path| path.as_str())
            .to_owned();
        let mut refusals = Vec::new();
        for (index, host) in hosts.iter().enumerate() {
            let last = index + 1 == hosts.len();
            if index > 0 {
                let rebound = self
                    .commit(|ledger| {
                        if let Some(operation) = ledger.operations.get_mut(&operation_id) {
                            operation.host.clone_from(host);
                        }
                    })
                    .await;
                if let Err(error) = rebound {
                    tracing::error!(%error, operation = %operation_id, "the operation's next host was not recorded, so it is not tried");
                    break;
                }
            }
            let sent = self
                .hosts
                .call(Call {
                    host,
                    tenant,
                    method: parts.method.clone(),
                    path: &path,
                    key: Some(&operation_id),
                    body: body.clone(),
                })
                .await;
            let may_try_next = matches!(work, Work::Create { .. })
                && !last
                && match &sent {
                    Sent::NotDelivered(_) => true,
                    Sent::Answered(response) => is_capacity_refusal(response),
                    Sent::Unknown(_) => false,
                };
            match outcome(&work, sent, Settling::Sent) {
                Outcome::Made(response) => return self.finish(&operation_id, response).await,
                Outcome::Nothing { reason, .. } if may_try_next => {
                    refusals.push(format!("{host}: {reason}"));
                }
                Outcome::Nothing {
                    answer,
                    reason,
                    machine_gone,
                } => {
                    self.release(&operation_id, machine_gone).await?;
                    return match answer {
                        Some(answer) => Ok(answered(answer)),
                        None => {
                            refusals.push(reason);
                            Err(ApiError(
                                StatusCode::SERVICE_UNAVAILABLE,
                                refusals.join("; "),
                            ))
                        }
                    };
                }
                Outcome::Doubt(reason) => {
                    if matches!(work, Work::Snapshot { .. })
                        && let Some(settled) = self.settle_snapshot(&operation_id).await
                    {
                        return settled;
                    }
                    let advice = if key.is_some() {
                        "a retry with the same Idempotency-Key returns its result"
                    } else {
                        "list before retrying"
                    };
                    return Err(gateway(format!(
                        "{reason}; the operation stays reserved until it is settled, and {advice}"
                    )));
                }
            }
        }
        self.release(&operation_id, false).await?;
        Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            refusals.join("; "),
        ))
    }

    async fn replay(
        self: &Arc<Self>,
        tenant: &str,
        ledger_key: &str,
        fingerprint: &str,
        key: Option<&str>,
    ) -> Result<Option<Response>, ApiError> {
        loop {
            let known = self.ledger().keys.get(ledger_key).cloned();
            match known {
                None => return Ok(None),
                Some(keyed) if keyed.request != fingerprint => {
                    return Err(ApiError(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        format!(
                            "idempotency key {} was already used for a different request",
                            key.unwrap_or_default()
                        ),
                    ));
                }
                Some(Keyed {
                    result: KeyResult::Machine { id },
                    ..
                }) => return self.current_machine(tenant, &id).await.map(Some),
                Some(Keyed {
                    result: KeyResult::Snapshot { id },
                    ..
                }) => return self.current_snapshot(tenant, &id).await.map(Some),
                Some(Keyed {
                    result: KeyResult::Pending { operation },
                    ..
                }) => {
                    if !self.reconcile(&operation).await {
                        return Err(ApiError(
                            StatusCode::SERVICE_UNAVAILABLE,
                            format!(
                                "operation {operation} for this key is in doubt until its host answers; retry later"
                            ),
                        ));
                    }
                }
            }
        }
    }

    async fn release(&self, operation_id: &str, machine_gone: bool) -> Result<(), ApiError> {
        self.commit(|ledger| {
            let Some(operation) = ledger.operations.remove(operation_id) else {
                return;
            };
            if let Some(key) = &operation.key
                && ledger.keys.get(key).is_some_and(|keyed| {
                    keyed.result
                        == KeyResult::Pending {
                            operation: operation_id.to_owned(),
                        }
                })
            {
                ledger.keys.remove(key);
            }
            if machine_gone
                && let Work::Delete { machine }
                | Work::Resume { machine }
                | Work::Pause { machine }
                | Work::Fork { machine }
                | Work::Snapshot { machine } = &operation.work
            {
                ledger.forget_machine(machine);
            }
        })
        .await
        .map_err(ledger_failed)
    }

    async fn finish(
        &self,
        operation_id: &str,
        response: axum::http::Response<Bytes>,
    ) -> Result<Response, ApiError> {
        let generation = generation_of(&response);
        let Some(operation) = self.ledger().operations.get(operation_id).cloned() else {
            return Ok(answered(response));
        };
        let now = unix_ms();
        let machine_info = serde_json::from_slice::<MachineInfo>(response.body()).ok();
        let snapshot_info = serde_json::from_slice::<SnapshotInfo>(response.body()).ok();
        let published_unix_ms = match (&operation.work, &snapshot_info) {
            (Work::Snapshot { .. }, Some(info)) => match self.store.snapshot(&info.id).await {
                Ok(Some(stored)) => stored.published_unix_ms,
                _ => now,
            },
            _ => now,
        };
        let placed = self
            .commit(|ledger| {
                ledger.operations.remove(operation_id)?;
                if generation != Generation::default() {
                    ledger.saw(&operation.host, generation);
                }
                let mut placed = None;
                let result = match (&operation.work, &machine_info, &snapshot_info) {
                    (Work::Create { .. } | Work::Fork { .. }, Some(info), _) => {
                        ledger.machines.insert(
                            info.id.clone(),
                            Allocation {
                                tenant: operation.tenant.clone(),
                                host: operation.host.clone(),
                                memory_mib: info.memory_mib,
                                disk_mib: provisioned_disk_mib(info.memory_mib),
                                running: info.state == MachineState::Running,
                                key: operation.key.clone(),
                            },
                        );
                        placed = Some(info.memory_mib);
                        Some(KeyResult::Machine {
                            id: info.id.clone(),
                        })
                    }
                    (Work::Snapshot { .. }, _, Some(info)) => {
                        ledger.snapshots.insert(
                            info.id.clone(),
                            SnapshotCharge {
                                tenant: operation.tenant.clone(),
                                stored_mib: stored_mib(info.stored_bytes),
                                since_unix_ms: published_unix_ms,
                                key: operation.key.clone(),
                                deleting_since_unix_ms: None,
                            },
                        );
                        Some(KeyResult::Snapshot {
                            id: info.id.clone(),
                        })
                    }
                    (Work::Resume { machine } | Work::Pause { machine }, Some(info), _) => {
                        if let Some(allocation) = ledger.machines.get_mut(machine) {
                            allocation.running = info.state == MachineState::Running;
                        }
                        None
                    }
                    (Work::Delete { machine }, _, _) => {
                        ledger.forget_machine(machine);
                        None
                    }
                    _ => {
                        tracing::error!(operation = %operation_id, "a host's answer did not describe the operation's result; the next report adopts what it made");
                        None
                    }
                };
                if let Some(key) = &operation.key {
                    match result {
                        Some(result) => {
                            if let Some(keyed) = ledger.keys.get_mut(key) {
                                keyed.result = result;
                            }
                        }
                        None => {
                            ledger.keys.remove(key);
                        }
                    }
                }
                placed
            })
            .await
            .map_err(ledger_failed)?;
        if let Some(memory_mib) = placed {
            self.placed(&operation.host, memory_mib);
        }
        Ok(answered(response))
    }

    async fn settle_snapshot(&self, operation_id: &str) -> Option<Result<Response, ApiError>> {
        match self.store.settle_snapshot(operation_id).await {
            Ok(Settled::Published(stored)) => {
                let response = axum::http::Response::builder()
                    .status(StatusCode::CREATED)
                    .body(Bytes::from(
                        serde_json::to_vec(&stored.info).unwrap_or_default(),
                    ))
                    .unwrap_or_default();
                Some(self.finish(operation_id, response).await)
            }
            Ok(Settled::Abandoned) => Some(self.release(operation_id, false).await.map(|()| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the snapshot was not taken".into(),
                )
                .into_response()
            })),
            Err(error) => {
                tracing::warn!(operation = %operation_id, %error, "settling a snapshot failed");
                None
            }
        }
    }

    async fn reconcile(self: &Arc<Self>, operation_id: &str) -> bool {
        let Some(operation) = self.ledger().operations.get(operation_id).cloned() else {
            return true;
        };
        let (method, path) = match &operation.work {
            Work::Snapshot { .. } => {
                return self
                    .settle_snapshot(operation_id)
                    .await
                    .is_some_and(|settled| settled.is_ok());
            }
            Work::Create { .. } | Work::Fork { .. } => {
                (Method::POST, format!("/operations/{operation_id}/settle"))
            }
            Work::Resume { machine } => (Method::POST, format!("/machines/{machine}/resume")),
            Work::Pause { machine } => (Method::POST, format!("/machines/{machine}/pause")),
            Work::Delete { machine } => (Method::DELETE, format!("/machines/{machine}")),
        };
        let sent = self
            .hosts
            .call(Call {
                host: &operation.host,
                tenant: &operation.tenant,
                method,
                path: &path,
                key: None,
                body: Bytes::new(),
            })
            .await;
        match outcome(&operation.work, sent, Settling::Reconciled) {
            Outcome::Made(response) => self.finish(operation_id, response).await.is_ok(),
            Outcome::Nothing { machine_gone, .. } => {
                self.release(operation_id, machine_gone).await.is_ok()
            }
            Outcome::Doubt(_) => false,
        }
    }

    async fn current_machine(&self, tenant: &str, id: &str) -> Result<Response, ApiError> {
        let allocation = self.machine(tenant, id)?;
        let sent = self
            .hosts
            .call(Call {
                host: &allocation.host,
                tenant,
                method: Method::GET,
                path: &format!("/machines/{id}"),
                key: None,
                body: Bytes::new(),
            })
            .await;
        match sent {
            Sent::Answered(response) if response.status() == StatusCode::OK => {
                let (mut parts, body) = answered(response).into_parts();
                parts.status = StatusCode::CREATED;
                Ok(Response::from_parts(parts, body))
            }
            Sent::Answered(response) => Ok(answered(response)),
            Sent::NotDelivered(error) | Sent::Unknown(error) => Err(gateway(error)),
        }
    }

    async fn current_snapshot(&self, tenant: &str, id: &str) -> Result<Response, ApiError> {
        match self.store.snapshot(id).await.map_err(store_failed)? {
            Some(stored) if stored.tenant == tenant => {
                Ok((StatusCode::CREATED, Json(stored.info)).into_response())
            }
            _ => Err(not_found("snapshot")),
        }
    }

    async fn handle_report(&self, report: HostReport) -> Result<HostReportReply, ApiError> {
        if report.store != self.store.url() {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "host uses store {:?} but this control plane uses {:?}",
                    report.store,
                    self.store.url()
                ),
            ));
        }
        let seen_boot = self
            .ledger()
            .hosts
            .get(&report.url)
            .map_or(0, |seen| seen.boot);
        if report.generation.boot < seen_boot {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "{} reports start {} but start {seen_boot} was already seen; it has lost its state, so remove it with DELETE /hosts?url= before it rejoins",
                    report.url, report.generation.boot
                ),
            ));
        }
        let applied = self
            .commit(|ledger| apply_report(ledger, &report))
            .await
            .map_err(ledger_failed)?;
        if !applied {
            return Ok(HostReportReply::default());
        }
        let recorded_executions = report
            .executions
            .iter()
            .filter(|execution| execution.ended)
            .map(|execution| execution.id.clone())
            .collect();
        lock(&self.reports).insert(
            report.url.clone(),
            Reported {
                report,
                at: Instant::now(),
            },
        );
        Ok(HostReportReply {
            recorded_executions,
        })
    }

    pub async fn reconcile_periodically(self: Arc<Self>) {
        let mut rounds = 0u32;
        loop {
            tokio::time::sleep(RECONCILE_INTERVAL).await;
            let pending: Vec<_> = self
                .ledger()
                .operations
                .iter()
                .filter(|(id, operation)| {
                    !lock(&self.performing).contains(*id)
                        && (matches!(operation.work, Work::Snapshot { .. })
                            || self.reporting(&operation.host))
                })
                .map(|(id, _)| id.clone())
                .collect();
            for operation in pending {
                self.reconcile(&operation).await;
            }
            rounds = rounds.wrapping_add(1);
            if rounds.is_multiple_of(STORE_RECONCILE_EVERY) {
                self.reconcile_snapshots().await;
            }
            if let Err(error) = self.commit(|_| ()).await {
                tracing::error!(%error, "usage was not recorded");
            }
        }
    }

    pub async fn reconcile_snapshots(&self) {
        let (charged_before, known_before, deleting_before) = {
            let ledger = self.ledger();
            let charged: BTreeSet<String> = ledger.snapshots.keys().cloned().collect();
            let deleting: BTreeMap<String, Option<u64>> = ledger
                .snapshots
                .iter()
                .map(|(id, charge)| (id.clone(), charge.deleting_since_unix_ms))
                .collect();
            let known = charged
                .iter()
                .cloned()
                .chain(ledger.operations.keys().cloned())
                .collect::<BTreeSet<String>>();
            (charged, known, deleting)
        };
        let stored = match self.store.snapshots().await {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(%error, "listing the store's snapshots failed");
                return;
            }
        };
        let now = unix_ms();
        let committed = self
            .commit(|ledger| {
                let in_store: BTreeMap<_, _> = stored
                    .iter()
                    .map(|stored| (stored.info.id.clone(), stored))
                    .collect();
                let gone: Vec<_> = ledger
                    .snapshots
                    .keys()
                    .filter(|id| charged_before.contains(*id) && !in_store.contains_key(*id))
                    .cloned()
                    .collect();
                for id in gone {
                    ledger.close_snapshot(&id, now);
                }
                for (id, stored) in in_store {
                    match ledger.snapshots.get_mut(&id) {
                        Some(charge)
                            if deleting_before.get(&id)
                                == Some(&charge.deleting_since_unix_ms) =>
                        {
                            charge.deleting_since_unix_ms = None;
                        }
                        Some(_) => {}
                        None if !known_before.contains(&id)
                            && !ledger.operations.contains_key(&id) =>
                        {
                            tracing::warn!(snapshot_id = %id, tenant = %stored.tenant, "adopting a stored snapshot no operation recorded");
                            ledger.snapshots.insert(
                                id,
                                SnapshotCharge {
                                    tenant: stored.tenant.clone(),
                                    stored_mib: stored_mib(stored.info.stored_bytes),
                                    since_unix_ms: stored.published_unix_ms,
                                    key: None,
                                    deleting_since_unix_ms: None,
                                },
                            );
                        }
                        None => {}
                    }
                }
            })
            .await;
        if let Err(error) = committed {
            tracing::error!(%error, "reconciling snapshots was not recorded");
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Settling {
    Sent,
    Reconciled,
}

enum Outcome {
    Made(axum::http::Response<Bytes>),
    Nothing {
        answer: Option<axum::http::Response<Bytes>>,
        reason: String,
        machine_gone: bool,
    },
    Doubt(String),
}

fn outcome(work: &Work, sent: Sent, settling: Settling) -> Outcome {
    let response = match sent {
        Sent::Unknown(reason) => return Outcome::Doubt(reason),
        Sent::NotDelivered(reason) => {
            return Outcome::Nothing {
                answer: None,
                reason,
                machine_gone: false,
            };
        }
        Sent::Answered(response) if response.status().is_success() => {
            return Outcome::Made(response);
        }
        Sent::Answered(response) => response,
    };
    let reason = format!(
        "answered {}: {}",
        response.status(),
        String::from_utf8_lossy(response.body())
    );
    let made_nothing = match work {
        Work::Create { .. } | Work::Fork { .. } => {
            settling == Settling::Sent || response.status() == StatusCode::NOT_FOUND
        }
        Work::Snapshot { .. } => response.status().is_client_error(),
        Work::Resume { .. } | Work::Pause { .. } | Work::Delete { .. } => true,
    };
    let names_a_machine = match work {
        Work::Create { .. } => false,
        Work::Fork { .. } | Work::Snapshot { .. } => settling == Settling::Sent,
        Work::Resume { .. } | Work::Pause { .. } | Work::Delete { .. } => true,
    };
    if made_nothing {
        Outcome::Nothing {
            machine_gone: names_a_machine && response.status() == StatusCode::NOT_FOUND,
            answer: Some(response),
            reason,
        }
    } else {
        Outcome::Doubt(reason)
    }
}

fn apply_report(ledger: &mut Ledger, report: &HostReport) -> bool {
    let host = report.url.as_str();
    if !ledger.saw(host, report.generation) {
        return false;
    }
    let listed: HashMap<&str, &ReportedMachine> = report
        .machines
        .iter()
        .map(|machine| (machine.id.as_str(), machine))
        .collect();
    let settled: Vec<String> = ledger
        .machines
        .iter()
        .filter(|(id, machine)| machine.host == host && !ledger.machine_is_busy(id))
        .map(|(id, _)| id.clone())
        .collect();
    for id in settled {
        match listed.get(id.as_str()) {
            None => ledger.forget_machine(&id),
            Some(reported) => {
                if let Some(machine) = ledger.machines.get_mut(&id) {
                    machine.running = reported.state == MachineState::Running;
                }
            }
        }
    }
    for reported in &report.machines {
        if !ledger.machines.contains_key(&reported.id) {
            ledger.machines.insert(
                reported.id.clone(),
                Allocation {
                    tenant: reported.tenant.clone(),
                    host: host.to_owned(),
                    memory_mib: reported.memory_mib,
                    disk_mib: provisioned_disk_mib(reported.memory_mib),
                    running: reported.state == MachineState::Running,
                    key: None,
                },
            );
        }
    }
    let reported_ids: BTreeSet<&str> = report
        .executions
        .iter()
        .map(|execution| execution.id.as_str())
        .collect();
    let forgotten: Vec<String> = ledger
        .executions
        .iter()
        .filter(|(id, record)| {
            record.host == host && record.acknowledged && !reported_ids.contains(id.as_str())
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in forgotten {
        ledger.fold_execution(&id);
    }
    for execution in &report.executions {
        ledger.record_execution(host, execution);
        if execution.ended
            && let Some(record) = ledger.executions.get_mut(&execution.id)
        {
            record.acknowledged = true;
        }
    }
    true
}

pub fn app(control: Arc<Control>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/hosts", get(hosts).post(report).delete(remove_host))
        .route("/usage", get(usage))
        .route("/machines", get(machines).post(create))
        .route("/machines/{id}", any(machine))
        .route("/machines/{id}/pause", post(pause))
        .route("/machines/{id}/resume", post(resume))
        .route("/machines/{id}/fork", post(fork))
        .route("/machines/{id}/{*path}", any(machine_path))
        .route("/snapshots", get(snapshots).post(create_snapshot))
        .route("/snapshots/{id}", get(snapshot).delete(delete_snapshot))
        .route("/previews", get(previews).post(create_preview))
        .route("/previews/{id}", any(preview))
        .with_state(control)
}

fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

struct Caller(String);

impl FromRequestParts<Arc<Control>> for Caller {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        control: &Arc<Control>,
    ) -> Result<Self, Response> {
        bearer(parts)
            .and_then(|key| control.keys.tenant_of(key))
            .map(|tenant| Self(tenant.to_owned()))
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Bearer")],
                    "a valid API key is required as a Bearer token",
                )
                    .into_response()
            })
    }
}

struct Operator;

impl FromRequestParts<Arc<Control>> for Operator {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        control: &Arc<Control>,
    ) -> Result<Self, Response> {
        let admitted = bearer(parts).is_some_and(|key| {
            <[u8; 32]>::from(sha2::Sha256::digest(key.as_bytes())) == control.control_key
        });
        if admitted {
            Ok(Self)
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "the control key is required",
            )
                .into_response())
        }
    }
}

struct Retry(Option<String>);

impl<S: Sync> FromRequestParts<S> for Retry {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        let Some(value) = parts.headers.get("idempotency-key") else {
            return Ok(Self(None));
        };
        value
            .to_str()
            .ok()
            .filter(|key| {
                !key.is_empty()
                    && key.len() <= 255
                    && key.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .map(|key| Self(Some(key.to_owned())))
            .ok_or_else(|| {
                ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "an idempotency key is 1 to 255 visible ASCII characters".into(),
                )
            })
    }
}

async fn report(
    State(control): State<Arc<Control>>,
    _: Operator,
    Json(report): Json<HostReport>,
) -> Result<Json<HostReportReply>, ApiError> {
    Ok(Json(control.handle_report(report).await?))
}

async fn hosts(State(control): State<Arc<Control>>, _: Operator) -> Json<Vec<HostInfo>> {
    let known: Vec<_> = control.ledger().hosts.keys().cloned().collect();
    let reports = lock(&control.reports);
    Json(
        known
            .into_iter()
            .map(|url| {
                let reported = reports.get(&url);
                HostInfo {
                    pool: reported.map(|reported| reported.report.pool.clone()),
                    room: reported
                        .filter(|reported| reported.is_fresh())
                        .map_or_else(Room::default, |reported| reported.report.room),
                    reported_ms_ago: reported.map(|reported| {
                        reported
                            .at
                            .elapsed()
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX)
                    }),
                    reporting: reported.is_some_and(Reported::is_fresh),
                    url,
                }
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct HostQuery {
    url: String,
}

async fn remove_host(
    State(control): State<Arc<Control>>,
    _: Operator,
    Query(query): Query<HostQuery>,
) -> Result<StatusCode, ApiError> {
    let url = query.url;
    let removed = control
        .commit(|ledger| {
            if ledger.hosts.remove(&url).is_none() {
                return false;
            }
            let machines: Vec<_> = ledger
                .machines
                .iter()
                .filter(|(_, machine)| machine.host == url)
                .map(|(id, _)| id.clone())
                .collect();
            for id in machines {
                ledger.forget_machine(&id);
            }
            let executions: Vec<_> = ledger
                .executions
                .iter()
                .filter(|(_, record)| record.host == url)
                .map(|(id, _)| id.clone())
                .collect();
            for id in executions {
                ledger.fold_execution(&id);
            }
            let operations: Vec<_> = ledger
                .operations
                .iter()
                .filter(|(_, operation)| {
                    operation.host == url && !matches!(operation.work, Work::Snapshot { .. })
                })
                .map(|(id, operation)| (id.clone(), operation.key.clone()))
                .collect();
            for (id, key) in operations {
                ledger.operations.remove(&id);
                ledger.free_key(key.as_deref());
            }
            true
        })
        .await
        .map_err(ledger_failed)?;
    lock(&control.reports).remove(&url);
    if removed {
        tracing::info!(host = %url, "host removed; its machines and reservations are released");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found("host"))
    }
}

async fn usage(State(control): State<Arc<Control>>, Caller(tenant): Caller) -> Json<TenantUsage> {
    let ledger = control.ledger();
    let usage = ledger.usage(&tenant);
    Json(TenantUsage {
        quotas: control.quotas(&tenant),
        vcpu_ms: usage.vcpu_ms,
        memory_mib_ms: usage.memory_mib_ms,
        snapshot_mib_ms: usage.snapshot_mib_ms,
        as_of_unix_ms: ledger.as_of_unix_ms,
        tenant,
    })
}

async fn fan_out<T: serde::de::DeserializeOwned>(
    control: &Control,
    tenant: &str,
    path: &str,
) -> Result<Vec<(String, T)>, ApiError> {
    let hosts = control.every_host()?;
    let answers = futures_util::future::join_all(hosts.iter().map(|host| {
        control.hosts.call(Call {
            host,
            tenant,
            method: Method::GET,
            path,
            key: None,
            body: Bytes::new(),
        })
    }))
    .await;
    let mut found = Vec::new();
    for (host, answer) in hosts.into_iter().zip(answers) {
        match answer {
            Sent::Answered(response) if response.status() == StatusCode::OK => {
                let value = serde_json::from_slice(response.body())
                    .map_err(|error| gateway(format!("{host}: {error}")))?;
                found.push((host, value));
            }
            Sent::Answered(response) if response.status() == StatusCode::NOT_FOUND => {}
            Sent::Answered(response) => {
                return Err(gateway(format!("{host} answered {}", response.status())));
            }
            Sent::NotDelivered(error) | Sent::Unknown(error) => return Err(gateway(error)),
        }
    }
    Ok(found)
}

async fn machines(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
) -> Result<Json<Vec<MachineInfo>>, ApiError> {
    let listed: Vec<(String, Vec<MachineInfo>)> = fan_out(&control, &tenant, "/machines").await?;
    let mut all: Vec<_> = listed
        .into_iter()
        .flat_map(|(_, machines)| machines)
        .collect();
    all.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(all))
}

async fn create(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Retry(key): Retry,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let body = body_of(body).await?;
    let body = if body.is_empty() {
        Bytes::from_static(b"{}")
    } else {
        body
    };
    let create: CreateMachine = json_of(&body)?;
    let (pool, memory_mib) = match &create.snapshot_id {
        Some(id) => match control.store.snapshot(id).await.map_err(store_failed)? {
            Some(stored) if stored.tenant == tenant => (stored.info.pool, stored.info.memory_mib),
            _ => return Err(not_found("snapshot")),
        },
        None => (None, create.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB)),
    };
    let hosts = control.candidates(
        memory_mib,
        pool.as_deref(),
        create.egress_address.as_deref(),
    );
    let holds =
        Holdings::running(memory_mib).plus(Holdings::disk(provisioned_disk_mib(memory_mib)));
    let work = Work::Create {
        request: digest(&serde_json::to_string(&create).unwrap_or_default()),
    };
    control
        .perform(
            &tenant,
            key,
            work,
            holds,
            hosts,
            Request::from_parts(parts, body),
        )
        .await
}

async fn fork(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Retry(key): Retry,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let source = control.machine(&tenant, &id)?;
    let holds = Holdings::running(source.memory_mib)
        .plus(Holdings::disk(provisioned_disk_mib(source.memory_mib)));
    let (parts, _) = request.into_parts();
    control
        .perform(
            &tenant,
            key,
            Work::Fork { machine: id },
            holds,
            vec![source.host],
            Request::from_parts(parts, Bytes::new()),
        )
        .await
}

async fn resume(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let machine = control.machine(&tenant, &id)?;
    let holds = if machine.running {
        Holdings::default()
    } else {
        Holdings::running(machine.memory_mib)
    };
    let (parts, _) = request.into_parts();
    control
        .perform(
            &tenant,
            None,
            Work::Resume { machine: id },
            holds,
            vec![machine.host],
            Request::from_parts(parts, Bytes::new()),
        )
        .await
}

async fn pause(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let machine = control.machine(&tenant, &id)?;
    let (parts, _) = request.into_parts();
    control
        .perform(
            &tenant,
            None,
            Work::Pause { machine: id },
            Holdings::default(),
            vec![machine.host],
            Request::from_parts(parts, Bytes::new()),
        )
        .await
}

async fn machine(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let allocation = control.machine(&tenant, &id)?;
    if request.method() == Method::DELETE {
        let (parts, _) = request.into_parts();
        return control
            .perform(
                &tenant,
                None,
                Work::Delete { machine: id },
                Holdings::default(),
                vec![allocation.host],
                Request::from_parts(parts, Bytes::new()),
            )
            .await;
    }
    forward(&control, &allocation.host, &tenant, request).await
}

async fn machine_path(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Path((id, _)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let allocation = control.machine(&tenant, &id)?;
    forward(&control, &allocation.host, &tenant, request).await
}

async fn forward(
    control: &Control,
    host: &str,
    tenant: &str,
    mut request: Request,
) -> Result<Response, ApiError> {
    let client_upgrade = hyper::upgrade::on(&mut request);
    let (mut parts, body) = request.into_parts();
    let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());
    parts.uri = format!("{host}{path}")
        .parse()
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "malformed path".into()))?;
    parts.headers.remove(header::HOST);
    parts.headers.insert(
        header::AUTHORIZATION,
        control
            .hosts
            .authorization()
            .parse()
            .map_err(|_| gateway("the control key is not a valid header"))?,
    );
    parts.headers.insert(
        hosts::TENANT_HEADER,
        tenant
            .parse()
            .map_err(|_| gateway("the tenant id is not a valid header"))?,
    );
    let mut response = control
        .hosts
        .client()
        .request(Request::from_parts(parts, body))
        .await
        .map_err(|error| match unanswered(host, &error) {
            Sent::NotDelivered(message) | Sent::Unknown(message) => gateway(message),
            Sent::Answered(_) => gateway(error.to_string()),
        })?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let host_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            if let (Ok(client), Ok(upstream)) = tokio::join!(client_upgrade, host_upgrade) {
                slop_machines::relay_until_either_side_ends(
                    TokioIo::new(client),
                    TokioIo::new(upstream),
                )
                .await;
            }
        });
    }
    response.headers_mut().remove(hosts::GENERATION_HEADER);
    Ok(response.map(Body::new))
}

async fn create_snapshot(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Retry(key): Retry,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let body = body_of(body).await?;
    let snapshot: CreateSnapshot = json_of(&body)?;
    let machine = control.machine(&tenant, &snapshot.machine_id)?;
    let holds = Holdings::disk(provisioned_disk_mib(machine.memory_mib));
    control
        .perform(
            &tenant,
            key,
            Work::Snapshot {
                machine: snapshot.machine_id,
            },
            holds,
            vec![machine.host],
            Request::from_parts(parts, body),
        )
        .await
}

async fn snapshots(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
) -> Result<Json<Vec<SnapshotInfo>>, ApiError> {
    let stored = control.store.snapshots().await.map_err(store_failed)?;
    Ok(Json(
        stored
            .into_iter()
            .filter(|stored| stored.tenant == tenant)
            .map(|stored| stored.info)
            .collect(),
    ))
}

async fn snapshot(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<Json<SnapshotInfo>, ApiError> {
    match control.store.snapshot(&id).await.map_err(store_failed)? {
        Some(stored) if stored.tenant == tenant => Ok(Json(stored.info)),
        _ => Err(not_found("snapshot")),
    }
}

async fn delete_snapshot(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let in_doubt = control.ledger().operations.contains_key(&id);
    let still_running = lock(&control.performing).contains(&id);
    if in_doubt && (still_running || !control.reconcile(&id).await) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "the snapshot's creation is still in doubt; retry once it is settled".into(),
        ));
    }
    match control.store.snapshot(&id).await.map_err(store_failed)? {
        Some(stored) if stored.tenant == tenant => {}
        _ => return Err(not_found("snapshot")),
    }
    let deleting_since = unix_ms();
    control
        .commit(|ledger| {
            if let Some(charge) = ledger.snapshots.get_mut(&id) {
                charge.deleting_since_unix_ms = Some(deleting_since);
            }
        })
        .await
        .map_err(ledger_failed)?;
    let deleted = control.store.delete_snapshot(&id).await;
    if let Ok(true) = deleted {
        let now = unix_ms();
        control
            .commit(|ledger| ledger.close_snapshot(&id, now))
            .await
            .map_err(ledger_failed)?;
    }
    match deleted {
        Ok(true) => {
            tracing::info!(snapshot_id = %id, "snapshot deleted");
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err(not_found("snapshot")),
        Err(error) => Err(store_failed(error)),
    }
}

async fn create_preview(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let body = body_of(body).await?;
    let preview: CreatePreview = json_of(&body)?;
    let machine = control.machine(&tenant, &preview.machine_id)?;
    forward(
        &control,
        &machine.host,
        &tenant,
        Request::from_parts(parts, Body::from(body)),
    )
    .await
}

async fn previews(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
) -> Result<Json<Vec<PreviewInfo>>, ApiError> {
    let listed: Vec<(String, Vec<PreviewInfo>)> = fan_out(&control, &tenant, "/previews").await?;
    let mut all: Vec<_> = listed
        .into_iter()
        .flat_map(|(_, previews)| previews)
        .collect();
    all.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(all))
}

async fn preview(
    State(control): State<Arc<Control>>,
    Caller(tenant): Caller,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let found: Vec<(String, PreviewInfo)> =
        fan_out(&control, &tenant, &format!("/previews/{id}")).await?;
    let Some((host, _)) = found.into_iter().next() else {
        return Err(not_found("preview"));
    };
    forward(&control, &host, &tenant, request).await
}
