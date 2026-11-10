use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    contract::{Quotas, TenantUsage},
    files,
    machines::MachineError,
};

const LEDGER: &str = "usage.json";
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 255;
const MAX_TENANT_ID_BYTES: usize = 64;

pub struct TenantConfig {
    pub id: String,
    pub quotas: Quotas,
}

#[derive(Clone, Debug)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn parse(key: &str) -> Result<Self, MachineError> {
        if key.is_empty()
            || key.len() > MAX_IDEMPOTENCY_KEY_BYTES
            || !key.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(MachineError::Invalid(format!(
                "an idempotency key is 1 to {MAX_IDEMPOTENCY_KEY_BYTES} visible ASCII characters"
            )));
        }
        Ok(Self(key.to_owned()))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Idempotency {
    key: String,
    request: String,
}

impl Idempotency {
    pub(crate) fn of(key: Option<IdempotencyKey>, request: String) -> Option<Self> {
        key.map(|IdempotencyKey(key)| Self { key, request })
    }

    pub(crate) fn replays(recorded: Option<&Self>, attempt: &Self) -> Result<bool, MachineError> {
        match recorded {
            Some(recorded) if recorded.key != attempt.key => Ok(false),
            Some(recorded) if recorded.request == attempt.request => Ok(true),
            Some(_) => Err(MachineError::Invalid(format!(
                "idempotency key {} was already used for a different request",
                attempt.key
            ))),
            None => Ok(false),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Usage {
    vcpu_ms: u64,
    memory_mib_ms: u64,
    snapshot_mib_ms: u64,
}

#[derive(Clone, Copy)]
pub(crate) enum Rate {
    Vm { vcpus: u8, memory_mib: u32 },
    Storage { mib: u64 },
}

impl Usage {
    fn accrue(&mut self, rate: Rate, elapsed_ms: u64) {
        match rate {
            Rate::Vm { vcpus, memory_mib } => {
                self.vcpu_ms = self
                    .vcpu_ms
                    .saturating_add(elapsed_ms.saturating_mul(vcpus.into()));
                self.memory_mib_ms = self
                    .memory_mib_ms
                    .saturating_add(elapsed_ms.saturating_mul(memory_mib.into()));
            }
            Rate::Storage { mib } => {
                self.snapshot_mib_ms = self
                    .snapshot_mib_ms
                    .saturating_add(elapsed_ms.saturating_mul(mib));
            }
        }
    }
}

struct Allowance {
    resource: &'static str,
    limit: u64,
    used: Mutex<u64>,
}

impl Allowance {
    fn new(resource: &'static str, limit: u64) -> Arc<Self> {
        Arc::new(Self {
            resource,
            limit,
            used: Mutex::new(0),
        })
    }

    fn hold(self: &Arc<Self>, amount: u64) -> Result<Held, MachineError> {
        let mut used = lock(&self.used);
        if used.saturating_add(amount) > self.limit {
            return Err(MachineError::QuotaExceeded(format!(
                "the tenant's {} quota is {}; {} is in use and this needs {amount}",
                self.resource, self.limit, *used
            )));
        }
        *used += amount;
        Ok(Held {
            allowance: self.clone(),
            amount,
        })
    }

    fn hold_regardless(self: &Arc<Self>, amount: u64) -> Held {
        *lock(&self.used) += amount;
        Held {
            allowance: self.clone(),
            amount,
        }
    }
}

pub(crate) struct Held {
    allowance: Arc<Allowance>,
    amount: u64,
}

impl Drop for Held {
    fn drop(&mut self) {
        *lock(&self.allowance.used) -= self.amount;
    }
}

struct Bucket {
    tokens: f64,
    refilled: Instant,
}

#[derive(Default)]
struct Meter {
    closed: Usage,
    open: HashMap<u64, (u64, Rate)>,
    next: u64,
}

pub struct Tenant {
    id: String,
    quotas: Quotas,
    running_machines: Arc<Allowance>,
    memory_mib: Arc<Allowance>,
    disk_mib: Arc<Allowance>,
    creates: Mutex<Bucket>,
    meter: Mutex<Meter>,
    reported: Mutex<(Usage, u64)>,
    attempts: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

pub(crate) struct Metered {
    tenant: Arc<Tenant>,
    span: u64,
}

impl Drop for Metered {
    fn drop(&mut self) {
        let now = unix_ms();
        let mut meter = lock(&self.tenant.meter);
        if let Some((since, rate)) = meter.open.remove(&self.span) {
            meter.closed.accrue(rate, now.saturating_sub(since));
        }
    }
}

pub(crate) struct TenantCharge {
    tenant: Arc<Tenant>,
    rate: Rate,
    _running: Held,
    _memory: Held,
    metered: Option<Metered>,
}

impl TenantCharge {
    pub(crate) fn start_metering(&mut self) {
        if self.metered.is_none() {
            self.metered = Some(self.tenant.meter(self.rate, unix_ms()));
        }
    }
}

pub(crate) struct Storage {
    _disk: Held,
    _metered: Option<Metered>,
}

impl Tenant {
    fn new(id: String, quotas: Quotas, recorded: Usage, as_of: u64) -> Arc<Self> {
        Arc::new(Self {
            running_machines: Allowance::new("running machines", quotas.running_machines.into()),
            memory_mib: Allowance::new("running memory MiB", quotas.memory_mib.into()),
            disk_mib: Allowance::new("disk MiB", u64::from(quotas.disk_gib) << 10),
            creates: Mutex::new(Bucket {
                tokens: creates_burst(&quotas),
                refilled: Instant::now(),
            }),
            meter: Mutex::new(Meter {
                closed: recorded,
                ..Meter::default()
            }),
            reported: Mutex::new((recorded, as_of)),
            attempts: Mutex::default(),
            id,
            quotas,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn charge(
        self: &Arc<Self>,
        vcpus: u8,
        memory_mib: u32,
    ) -> Result<TenantCharge, MachineError> {
        Ok(TenantCharge {
            _running: self.running_machines.hold(1)?,
            _memory: self.memory_mib.hold(memory_mib.into())?,
            tenant: self.clone(),
            rate: Rate::Vm { vcpus, memory_mib },
            metered: None,
        })
    }

    pub(crate) fn reserve_disk(&self, mib: u64) -> Result<Held, MachineError> {
        self.disk_mib.hold(mib)
    }

    pub(crate) fn keep_disk(&self, mib: u64) -> Held {
        self.disk_mib.hold_regardless(mib)
    }

    pub(crate) fn store(self: &Arc<Self>, disk: Held, mib: u64, since_unix_ms: u64) -> Storage {
        Storage {
            _disk: disk,
            _metered: Some(self.meter(Rate::Storage { mib }, since_unix_ms)),
        }
    }

    pub(crate) fn store_unmetered(disk: Held) -> Storage {
        Storage {
            _disk: disk,
            _metered: None,
        }
    }

    fn meter(self: &Arc<Self>, rate: Rate, since_unix_ms: u64) -> Metered {
        let mut meter = lock(&self.meter);
        let span = meter.next;
        meter.next += 1;
        meter.open.insert(span, (since_unix_ms, rate));
        Metered {
            tenant: self.clone(),
            span,
        }
    }

    pub(crate) fn take_create(&self) -> Result<(), MachineError> {
        if self.quotas.creates_per_second == 0 {
            return Err(MachineError::QuotaExceeded(
                "the tenant's creates_per_second quota is 0".into(),
            ));
        }
        let per_second = f64::from(self.quotas.creates_per_second);
        let mut bucket = lock(&self.creates);
        let now = Instant::now();
        let refill = now.duration_since(bucket.refilled).as_secs_f64() * per_second;
        bucket.tokens = (bucket.tokens + refill).min(creates_burst(&self.quotas));
        bucket.refilled = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        Err(MachineError::RateLimited {
            retry_after: Duration::from_secs_f64((1.0 - bucket.tokens) / per_second),
        })
    }

    pub(crate) async fn take_turn(
        &self,
        key: Option<&Idempotency>,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        let key = &key?.key;
        let turn = {
            let mut attempts = lock(&self.attempts);
            attempts.retain(|_, turn| turn.strong_count() > 0);
            match attempts.get(key).and_then(Weak::upgrade) {
                Some(turn) => turn,
                None => {
                    let turn = Arc::new(tokio::sync::Mutex::new(()));
                    attempts.insert(key.clone(), Arc::downgrade(&turn));
                    turn
                }
            }
        };
        Some(turn.lock_owned().await)
    }

    fn usage_at(&self, now: u64) -> Usage {
        let meter = lock(&self.meter);
        let mut usage = meter.closed;
        for (since, rate) in meter.open.values() {
            usage.accrue(*rate, now.saturating_sub(*since));
        }
        usage
    }

    pub fn usage(&self) -> TenantUsage {
        let (usage, as_of_unix_ms) = *lock(&self.reported);
        TenantUsage {
            tenant: self.id.clone(),
            quotas: self.quotas.clone(),
            vcpu_ms: usage.vcpu_ms,
            memory_mib_ms: usage.memory_mib_ms,
            snapshot_mib_ms: usage.snapshot_mib_ms,
            as_of_unix_ms,
        }
    }
}

fn creates_burst(quotas: &Quotas) -> f64 {
    f64::from(quotas.creates_per_second)
}

#[derive(Default, Deserialize, Serialize)]
struct Ledger {
    as_of_unix_ms: u64,
    tenants: BTreeMap<String, Usage>,
}

pub(crate) struct Tenants {
    state_dir: PathBuf,
    recorded_as_of: u64,
    by_id: Mutex<HashMap<String, Arc<Tenant>>>,
    flushing: tokio::sync::Mutex<()>,
}

impl Tenants {
    pub(crate) async fn open(state_dir: &Path, configs: Vec<TenantConfig>) -> io::Result<Self> {
        let ledger: Ledger = match files::read_record(state_dir, LEDGER).await {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ledger::default(),
            ledger => ledger?,
        };
        let mut by_id = HashMap::new();
        for TenantConfig { id, quotas } in configs {
            validate_tenant_id(&id)?;
            let recorded = ledger.tenants.get(&id).copied().unwrap_or_default();
            let tenant = Tenant::new(id.clone(), quotas, recorded, ledger.as_of_unix_ms);
            if by_id.insert(id.clone(), tenant).is_some() {
                return Err(io::Error::other(format!("tenant {id} is configured twice")));
            }
        }
        for (id, recorded) in &ledger.tenants {
            by_id.entry(id.clone()).or_insert_with(|| {
                Tenant::new(id.clone(), Quotas::none(), *recorded, ledger.as_of_unix_ms)
            });
        }
        Ok(Self {
            state_dir: state_dir.to_owned(),
            recorded_as_of: ledger.as_of_unix_ms,
            by_id: Mutex::new(by_id),
            flushing: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<Tenant>> {
        lock(&self.by_id).get(id).cloned()
    }

    pub(crate) fn owner(&self, id: &str) -> Arc<Tenant> {
        lock(&self.by_id)
            .entry(id.to_owned())
            .or_insert_with(|| {
                tracing::warn!(tenant = %id, "state belongs to a tenant that is not configured");
                Tenant::new(id.to_owned(), Quotas::none(), Usage::default(), 0)
            })
            .clone()
    }

    pub(crate) fn recorded_as_of(&self) -> u64 {
        self.recorded_as_of
    }

    pub(crate) async fn flush(&self) -> io::Result<()> {
        let _one_flush_at_a_time = self.flushing.lock().await;
        let now = unix_ms();
        let tenants: Vec<_> = lock(&self.by_id).values().cloned().collect();
        let usages: Vec<_> = tenants
            .iter()
            .map(|tenant| (tenant.clone(), tenant.usage_at(now)))
            .collect();
        let ledger = Ledger {
            as_of_unix_ms: now,
            tenants: usages
                .iter()
                .map(|(tenant, usage)| (tenant.id.clone(), *usage))
                .collect(),
        };
        files::replace_record(&self.state_dir, LEDGER, &ledger).await?;
        for (tenant, usage) in usages {
            *lock(&tenant.reported) = (usage, now);
        }
        Ok(())
    }
}

fn validate_tenant_id(id: &str) -> io::Result<()> {
    let valid = !id.is_empty()
        && id.len() <= MAX_TENANT_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "tenant id {id:?} must be 1 to {MAX_TENANT_ID_BYTES} letters, digits, '-' or '_'"
        )))
    }
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quotas(creates_per_second: u32) -> Quotas {
        Quotas {
            running_machines: 2,
            memory_mib: 1024,
            disk_gib: 1,
            creates_per_second,
        }
    }

    #[test]
    fn a_charge_meters_only_once_started() {
        let tenant = Tenant::new("t".into(), quotas(1), Usage::default(), 0);
        let mut charge = tenant.charge(2, 512).expect("charge");
        assert!(lock(&tenant.meter).open.is_empty());
        charge.start_metering();
        charge.start_metering();
        assert_eq!(lock(&tenant.meter).open.len(), 1);
        drop(charge);
        assert!(lock(&tenant.meter).open.is_empty());
    }

    #[test]
    fn holds_are_refused_beyond_the_quota_and_returned_when_dropped() {
        let tenant = Tenant::new("t".into(), quotas(1), Usage::default(), 0);
        let first = tenant.charge(1, 512).expect("first");
        let second = tenant.charge(1, 512).expect("second");
        assert!(matches!(
            tenant.charge(1, 128),
            Err(MachineError::QuotaExceeded(_))
        ));
        drop(first);
        assert!(matches!(
            tenant.charge(1, 1024),
            Err(MachineError::QuotaExceeded(_))
        ));
        let _third = tenant.charge(1, 512).expect("third");
        drop(second);
        let kept = tenant.keep_disk(2048);
        assert!(tenant.reserve_disk(1).is_err());
        drop(kept);
        assert!(tenant.reserve_disk(1024).is_ok());
    }

    #[test]
    fn creates_refill_at_the_quota_rate() {
        let tenant = Tenant::new("t".into(), quotas(2), Usage::default(), 0);
        tenant.take_create().expect("first of the burst");
        tenant.take_create().expect("second of the burst");
        let Err(MachineError::RateLimited { retry_after }) = tenant.take_create() else {
            panic!("a third create in the same instant is limited");
        };
        assert!(retry_after <= Duration::from_millis(500));
        let closed = Tenant::new("t".into(), quotas(0), Usage::default(), 0);
        assert!(matches!(
            closed.take_create(),
            Err(MachineError::QuotaExceeded(_))
        ));
    }

    #[test]
    fn usage_accrues_closed_and_open_spans() {
        let tenant = Tenant::new("t".into(), quotas(1), Usage::default(), 0);
        let now = unix_ms();
        let vm = tenant.meter(
            Rate::Vm {
                vcpus: 2,
                memory_mib: 512,
            },
            now - 1000,
        );
        let snapshot = tenant.meter(Rate::Storage { mib: 100 }, now - 500);
        let open = tenant.usage_at(now);
        assert_eq!(
            open,
            Usage {
                vcpu_ms: 2000,
                memory_mib_ms: 512_000,
                snapshot_mib_ms: 50_000
            }
        );
        drop((vm, snapshot));
        let closed = tenant.usage_at(now + 10_000);
        assert!(closed.vcpu_ms >= 2000 && closed.vcpu_ms < 2000 + 2 * 1000);
        assert_eq!(tenant.usage_at(now + 20_000), closed);
    }

    #[test]
    fn idempotency_keys_are_visible_ascii_and_bound_to_one_request() {
        assert!(IdempotencyKey::parse("").is_err());
        assert!(IdempotencyKey::parse("has space").is_err());
        assert!(IdempotencyKey::parse(&"k".repeat(256)).is_err());
        let attempt =
            Idempotency::of(IdempotencyKey::parse("k-1").ok(), "create {}".into()).expect("keyed");
        let other_key = Idempotency::of(IdempotencyKey::parse("k-2").ok(), "create {}".into());
        let other_request = Idempotency::of(IdempotencyKey::parse("k-1").ok(), "fork m".into());
        assert!(Idempotency::replays(Some(&attempt), &attempt).expect("same"));
        assert!(!Idempotency::replays(other_key.as_ref(), &attempt).expect("other key"));
        assert!(Idempotency::replays(other_request.as_ref(), &attempt).is_err());
        assert!(!Idempotency::replays(None, &attempt).expect("unkeyed"));
    }
}
