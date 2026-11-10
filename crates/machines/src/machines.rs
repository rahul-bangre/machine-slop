use std::{
    collections::HashMap,
    io,
    ops::ControlFlow,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore, watch};

use crate::{
    contract::{
        CreateMachine, CreateSnapshot, MachineInfo, MachineState, SnapshotInfo, UpdateMachine,
    },
    egress::{EgressPolicy, MAX_MACHINE_TUNNEL_FILES},
    files,
    firecracker::{Jail, SAVED_STATE, SCRATCH},
    machine::{
        Activity, BootError, Booted, Disk, Image, Machine, MachineSpec, MachineUse, Ownership,
        provisioned_disk_mib,
    },
    tenants::{
        Idempotency, IdempotencyKey, Storage, Tenant, TenantCharge, TenantConfig, Tenants, unix_ms,
    },
};

const DEFAULT_VCPUS: u8 = 2;
const DEFAULT_MEMORY_MIB: u32 = 512;
const MAX_VCPUS: u8 = 32;
const DEFAULT_IDLE_TIMEOUT_SECS: u32 = 600;
const MEMORY_MIB_RANGE: std::ops::RangeInclusive<u32> = 128..=32768;
const IDLE_TIMEOUT_SECS_RANGE: std::ops::RangeInclusive<u32> = 1..=86400;
const OPEN_FILES_OUTSIDE_EGRESS: u64 = 4096;
pub(crate) const MACHINE_OVERHEAD_MIB: u32 = 64;
const HOST_RESERVE_MIB: u32 = 512;
const SNAPSHOT_RECORD: &str = "snapshot.json";
const USAGE_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum MachineError {
    Invalid(String),
    UnknownMachine,
    UnknownSnapshot,
    Conflict(String),
    QuotaExceeded(String),
    RateLimited {
        retry_after: Duration,
    },
    Capacity,
    ShuttingDown,
    Boot(BootError),
    Failed {
        message: String,
        state: MachineState,
    },
    Io(io::Error),
}

impl std::error::Error for MachineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Boot(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl std::fmt::Display for MachineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Conflict(message) | Self::QuotaExceeded(message) => {
                write!(f, "{message}")
            }
            Self::RateLimited { retry_after } => write!(
                f,
                "the tenant's create rate is exceeded; retry in {} ms",
                retry_after.as_millis()
            ),
            Self::UnknownMachine => write!(f, "unknown machine"),
            Self::UnknownSnapshot => write!(f, "unknown snapshot"),
            Self::Capacity => write!(f, "machine capacity reached"),
            Self::ShuttingDown => write!(f, "host is shutting down"),
            Self::Boot(error) => write!(f, "{error}"),
            Self::Failed { message, state } => {
                let state = match state {
                    MachineState::Running => "still running",
                    MachineState::Paused => "still paused",
                    MachineState::Exited { .. } => "lost and now exited",
                };
                write!(f, "{message}; the machine is {state}")
            }
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

pub struct Limits {
    pub running_vms: usize,
    pub memory_mib: u32,
    pub spare_machines: usize,
    pub first_jail_uid: u32,
}

pub(crate) struct Admission {
    _slot: OwnedSemaphorePermit,
    _memory: OwnedSemaphorePermit,
    seat: Seat,
    tenant: Option<TenantCharge>,
}

impl Admission {
    pub(crate) fn uid(&self) -> u32 {
        self.seat.uid
    }

    pub(crate) fn charge(&mut self, charge: TenantCharge) {
        self.tenant = Some(charge);
    }

    pub(crate) fn start_metering(&mut self) {
        if let Some(charge) = &mut self.tenant {
            charge.start_metering();
        }
    }

    fn charged(mut self, charge: TenantCharge) -> Self {
        self.charge(charge);
        self
    }
}

struct Seat {
    uid: u32,
    free: Arc<std::sync::Mutex<Vec<u32>>>,
}

impl Drop for Seat {
    fn drop(&mut self) {
        lock(&self.free).push(self.uid);
    }
}

pub(crate) fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct Machines {
    _state_lock: std::fs::File,
    image: Image,
    jail: Jail,
    tenants: Tenants,
    seats: Arc<std::sync::Mutex<Vec<u32>>>,
    machines_dir: PathBuf,
    snapshots_dir: PathBuf,
    limits: Limits,
    slots: Arc<Semaphore>,
    memory: Arc<Semaphore>,
    spare_settled: Notify,
    admission_requests: AtomicUsize,
    create_successes: AtomicU64,
    create_failures: AtomicU64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    machines: HashMap<String, Arc<Machine>>,
    snapshots: HashMap<String, Arc<Snapshot>>,
    spares: Vec<Booted>,
    spares_booting: usize,
    shutting_down: bool,
}

struct AdmissionRequest<'a>(&'a AtomicUsize);

impl<'a> AdmissionRequest<'a> {
    fn new(requests: &'a AtomicUsize) -> Self {
        requests.fetch_add(1, Ordering::SeqCst);
        Self(requests)
    }
}

impl Drop for AdmissionRequest<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

enum Claim {
    Admitted(Admission),
    Spare(Booted),
    Retry,
}

struct Snapshot {
    info: SnapshotInfo,
    tenant: Arc<Tenant>,
    idempotency: Option<Idempotency>,
    dir: PathBuf,
    deleted: RwLock<bool>,
    storage: std::sync::Mutex<Option<Storage>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct SnapshotRecord {
    #[serde(flatten)]
    info: SnapshotInfo,
    tenant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idempotency: Option<Idempotency>,
}

impl Machines {
    pub async fn open(
        image_dir: PathBuf,
        state_dir: PathBuf,
        limits: Limits,
        tenants: Vec<TenantConfig>,
    ) -> io::Result<Arc<Self>> {
        let image = Image::in_dir(&image_dir)?;
        ensure_open_file_limit(
            limits.running_vms as u64 * MAX_MACHINE_TUNNEL_FILES + OPEN_FILES_OUTSIDE_EGRESS,
        )?;
        tokio::fs::create_dir_all(&state_dir).await?;
        let state_lock = std::fs::File::create(state_dir.join("machined.lock"))?;
        state_lock.try_lock().map_err(|_| {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another machined is using {}", state_dir.display()),
            )
        })?;
        let machines_dir = state_dir.join("machines");
        let snapshots_dir = state_dir.join("snapshots");
        for dir in [&machines_dir, &snapshots_dir] {
            tokio::fs::create_dir_all(dir).await?;
            tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let machines_dir = tokio::fs::canonicalize(machines_dir).await?;
        let snapshots_dir = tokio::fs::canonicalize(snapshots_dir).await?;
        let jail = Jail::new(image.jailer(), &state_dir)?;
        jail.kill_orphans()?;
        let tenants = Tenants::open(&state_dir, tenants).await?;
        let mut state = State::default();
        for (id, dir) in subdirectories(&machines_dir).await? {
            match Machine::recover(id.clone(), dir.clone(), &tenants).await {
                Ok(Some(machine)) => {
                    state.machines.insert(id, Arc::new(machine));
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(path = %dir.display(), %error, "ignoring unreadable machine");
                }
            }
        }
        for (id, dir) in subdirectories(&snapshots_dir).await? {
            if !tokio::fs::try_exists(dir.join(SNAPSHOT_RECORD)).await? {
                tokio::fs::remove_dir_all(&dir).await?;
                continue;
            }
            match files::read_record::<SnapshotRecord>(&dir, SNAPSHOT_RECORD).await {
                Ok(record) => {
                    let tenant = tenants.owner(&record.tenant);
                    let mib = stored_mib(record.info.stored_bytes);
                    let metered_since = record.info.created_unix_ms.max(tenants.recorded_as_of());
                    let storage = tenant.store(tenant.keep_disk(mib), mib, metered_since);
                    let snapshot = Snapshot {
                        info: record.info,
                        tenant,
                        idempotency: record.idempotency,
                        dir,
                        deleted: RwLock::new(false),
                        storage: std::sync::Mutex::new(Some(storage)),
                    };
                    state.snapshots.insert(id, Arc::new(snapshot));
                }
                Err(error) => {
                    tracing::error!(path = %dir.display(), %error, "ignoring unreadable snapshot");
                }
            }
        }
        let paused = state
            .machines
            .values()
            .filter(|machine| machine.state() == MachineState::Paused)
            .count();
        tracing::info!(
            machines = state.machines.len(),
            paused,
            snapshots = state.snapshots.len(),
            "state recovered"
        );
        let seats = (0..limits.running_vms)
            .rev()
            .map(|seat| u32::try_from(seat).map(|seat| limits.first_jail_uid.saturating_add(seat)))
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
        let machines = Arc::new(Self {
            _state_lock: state_lock,
            image,
            jail,
            tenants,
            seats: Arc::new(std::sync::Mutex::new(seats)),
            machines_dir,
            snapshots_dir,
            slots: Arc::new(Semaphore::new(limits.running_vms)),
            memory: Arc::new(Semaphore::new(limits.memory_mib as usize)),
            limits,
            spare_settled: Notify::new(),
            admission_requests: AtomicUsize::new(0),
            create_successes: AtomicU64::new(0),
            create_failures: AtomicU64::new(0),
            state: Mutex::new(state),
        });
        for machine in machines.state.lock().await.machines.values() {
            machines.destroy_when_idle(machine);
        }
        machines.boot_spares();
        tokio::spawn(flush_usage_periodically(Arc::downgrade(&machines)));
        Ok(machines)
    }

    pub fn tenant(&self, id: &str) -> Option<Arc<Tenant>> {
        self.tenants.get(id)
    }

    async fn replayed_machine(
        &self,
        tenant: &Tenant,
        attempt: Option<&Idempotency>,
    ) -> Result<Option<Arc<Machine>>, MachineError> {
        let Some(attempt) = attempt else {
            return Ok(None);
        };
        let state = self.state.lock().await;
        for snapshot in state.snapshots.values() {
            if snapshot.tenant.id() == tenant.id() {
                Idempotency::replays(snapshot.idempotency.as_ref(), attempt)?;
            }
        }
        for machine in state.machines.values() {
            if machine.belongs_to(tenant) && Idempotency::replays(machine.idempotency(), attempt)? {
                return Ok(Some(machine.clone()));
            }
        }
        Ok(None)
    }

    async fn replayed_snapshot(
        &self,
        tenant: &Tenant,
        attempt: Option<&Idempotency>,
    ) -> Result<Option<SnapshotInfo>, MachineError> {
        let Some(attempt) = attempt else {
            return Ok(None);
        };
        let state = self.state.lock().await;
        for machine in state.machines.values() {
            if machine.belongs_to(tenant) {
                Idempotency::replays(machine.idempotency(), attempt)?;
            }
        }
        for snapshot in state.snapshots.values() {
            if snapshot.tenant.id() == tenant.id()
                && Idempotency::replays(snapshot.idempotency.as_ref(), attempt)?
            {
                return Ok(Some(snapshot.info.clone()));
            }
        }
        Ok(None)
    }

    fn destroy_when_idle(self: &Arc<Self>, machine: &Machine) {
        tokio::spawn(destroy_when_idle(
            Arc::downgrade(self),
            machine.id().to_owned(),
            machine.activity(),
            machine.idle_timeout(),
        ));
    }

    pub async fn create(
        self: &Arc<Self>,
        tenant: &Arc<Tenant>,
        request: CreateMachine,
        key: Option<IdempotencyKey>,
    ) -> Result<Arc<Machine>, MachineError> {
        let attempt = Idempotency::of(
            key,
            format!(
                "create {}",
                serde_json::to_string(&request)
                    .map_err(io::Error::other)
                    .map_err(MachineError::Io)?
            ),
        );
        let turn = tenant.take_turn(attempt.as_ref()).await;
        if let Some(machine) = self.replayed_machine(tenant, attempt.as_ref()).await? {
            return Ok(machine);
        }
        let snapshot = match &request.snapshot_id {
            None => None,
            Some(_) if request.vcpus.is_some() || request.memory_mib.is_some() => {
                return Err(MachineError::Invalid(
                    "vcpus and memory_mib come from the snapshot".into(),
                ));
            }
            Some(id) => Some(
                self.snapshot(tenant, id)
                    .await
                    .ok_or(MachineError::UnknownSnapshot)?,
            ),
        };
        let (vcpus, memory_mib, inherited_hosts) = match &snapshot {
            Some(snapshot) => (
                snapshot.info.vcpus,
                snapshot.info.memory_mib,
                snapshot.info.allowed_hosts.clone(),
            ),
            None => (
                request.vcpus.unwrap_or(DEFAULT_VCPUS),
                request.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
                vec!["*".to_owned()],
            ),
        };
        let idle_timeout_secs = request
            .idle_timeout_secs
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
        if !(1..=MAX_VCPUS).contains(&vcpus) {
            return Err(MachineError::Invalid(format!(
                "vcpus must be between 1 and {MAX_VCPUS}"
            )));
        }
        if !MEMORY_MIB_RANGE.contains(&memory_mib) {
            return Err(MachineError::Invalid(format!(
                "memory_mib must be between {} and {}",
                MEMORY_MIB_RANGE.start(),
                MEMORY_MIB_RANGE.end()
            )));
        }
        if !IDLE_TIMEOUT_SECS_RANGE.contains(&idle_timeout_secs) {
            return Err(MachineError::Invalid(format!(
                "idle_timeout_secs must be between {} and {}",
                IDLE_TIMEOUT_SECS_RANGE.start(),
                IDLE_TIMEOUT_SECS_RANGE.end()
            )));
        }
        let allowed_hosts = request.allowed_hosts.unwrap_or(inherited_hosts);
        let spec = MachineSpec {
            vcpus,
            memory_mib,
            idle_timeout_secs,
            egress: EgressPolicy::parse(&allowed_hosts).map_err(MachineError::Invalid)?,
        };
        let ownership = Ownership {
            tenant: tenant.clone(),
            idempotency: attempt,
            disk: tenant.reserve_disk(provisioned_disk_mib(memory_mib))?,
        };
        let charge = tenant.charge(vcpus, memory_mib)?;
        tenant.take_create()?;
        let machines = self.clone();
        let snapshot_id = request.snapshot_id;
        run_to_completion(async move {
            let _turn = turn;
            let request_started = Instant::now();
            let booted = match snapshot {
                None => machines.boot(&spec, charge).await,
                Some(snapshot) => {
                    let deleted = snapshot.deleted.read().await;
                    if *deleted {
                        Err(MachineError::UnknownSnapshot)
                    } else {
                        machines
                            .restore(&snapshot.dir, Disk::Copy, &spec, charge)
                            .await
                    }
                }
            };
            let created = machines
                .admit(
                    booted.map(|booted| (booted, ownership)),
                    request_started,
                    spec,
                )
                .await;
            match &created {
                Ok(machine) => {
                    machines.create_successes.fetch_add(1, Ordering::Relaxed);
                    let info = machine.info();
                    tracing::info!(machine_id = %info.id, tenant = %machine.tenant().id(), vcpus = info.vcpus, memory_mib = info.memory_mib, boot_ms = info.boot_ms, from_snapshot = ?snapshot_id, "machine created");
                }
                Err(error) => {
                    machines.create_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(%error, "machine create failed");
                }
            }
            created
        })
        .await
    }

    pub async fn fork(
        self: &Arc<Self>,
        tenant: &Arc<Tenant>,
        id: &str,
        key: Option<IdempotencyKey>,
    ) -> Result<Arc<Machine>, MachineError> {
        let attempt = Idempotency::of(key, format!("fork {id}"));
        let turn = tenant.take_turn(attempt.as_ref()).await;
        if let Some(machine) = self.replayed_machine(tenant, attempt.as_ref()).await? {
            return Ok(machine);
        }
        let source = self
            .begin_use(tenant, id)
            .await
            .ok_or(MachineError::UnknownMachine)?;
        let machines = self.clone();
        let tenant = tenant.clone();
        let source_id = id.to_owned();
        run_to_completion(async move {
            let _turn = turn;
            let request_started = Instant::now();
            let unrecorded_snapshot = machines.snapshots_dir.join(new_id());
            let source = source.exclusive().await;
            let spec = source.spec();
            let restored = async {
                source.is_paused()?;
                let ownership = Ownership {
                    tenant: tenant.clone(),
                    idempotency: attempt,
                    disk: tenant.reserve_disk(provisioned_disk_mib(spec.memory_mib))?,
                };
                let charge = tenant.charge(spec.vcpus, spec.memory_mib)?;
                tenant.take_create()?;
                let admission = machines.admission(spec.memory_mib).await?.charged(charge);
                tokio::fs::create_dir(&unrecorded_snapshot)
                    .await
                    .map_err(MachineError::Io)?;
                source.save_into(&unrecorded_snapshot).await?;
                drop(source);
                let restored = machines
                    .restore_in(admission, &unrecorded_snapshot, Disk::Take, &spec)
                    .await?;
                Ok((restored, ownership))
            };
            let restored = restored.await;
            files::remove_dir_logged(&unrecorded_snapshot).await;
            let forked = machines.admit(restored, request_started, spec).await;
            match &forked {
                Ok(fork) => tracing::info!(machine_id = %fork.id(), source_id = %source_id, "machine forked"),
                Err(error) => tracing::warn!(source_id = %source_id, %error, "machine fork failed"),
            }
            forked
        })
        .await
    }

    fn new_machine_dir(&self) -> (String, PathBuf) {
        let id = new_id();
        let dir = self.machines_dir.join(&id);
        (id, dir)
    }

    async fn boot(
        &self,
        spec: &MachineSpec,
        mut charge: TenantCharge,
    ) -> Result<Booted, MachineError> {
        let is_default = (spec.vcpus, spec.memory_mib) == (DEFAULT_VCPUS, DEFAULT_MEMORY_MIB);
        let request = AdmissionRequest::new(&self.admission_requests);
        let admission = loop {
            match self.claim(is_default, spec.memory_mib).await? {
                Claim::Admitted(admission) => break admission,
                Claim::Spare(spare) if is_default && spare.is_alive() => {
                    match spare.charge(charge) {
                        Ok(()) => return Ok(spare),
                        Err(returned) => {
                            charge = returned;
                            spare.discard().await;
                        }
                    }
                }
                Claim::Spare(spare) => spare.discard().await,
                Claim::Retry => {}
            }
        };
        drop(request);
        self.boot_in(
            admission.charged(charge),
            spec.vcpus,
            spec.memory_mib,
            spec.egress.clone(),
        )
        .await
    }

    async fn boot_in(
        &self,
        admission: Admission,
        vcpus: u8,
        memory_mib: u32,
        egress: EgressPolicy,
    ) -> Result<Booted, MachineError> {
        let (id, dir) = self.new_machine_dir();
        Booted::from_image(
            id,
            dir,
            &self.image,
            &self.jail,
            vcpus,
            memory_mib,
            egress,
            admission,
        )
        .await
        .map_err(MachineError::Boot)
    }

    async fn restore(
        &self,
        saved: &Path,
        disk: Disk,
        spec: &MachineSpec,
        charge: TenantCharge,
    ) -> Result<Booted, MachineError> {
        let admission = self.admission(spec.memory_mib).await?.charged(charge);
        self.restore_in(admission, saved, disk, spec).await
    }

    async fn restore_in(
        &self,
        admission: Admission,
        saved: &Path,
        disk: Disk,
        spec: &MachineSpec,
    ) -> Result<Booted, MachineError> {
        let (id, dir) = self.new_machine_dir();
        let (vcpus, memory_mib, egress) = (spec.vcpus, spec.memory_mib, spec.egress.clone());
        Booted::from_saved(
            id, dir, saved, disk, &self.jail, vcpus, memory_mib, egress, admission,
        )
        .await
        .map_err(MachineError::Boot)
    }

    async fn admission(&self, memory_mib: u32) -> Result<Admission, MachineError> {
        let _request = AdmissionRequest::new(&self.admission_requests);
        loop {
            match self.claim(false, memory_mib).await? {
                Claim::Admitted(admission) => return Ok(admission),
                Claim::Spare(spare) => spare.discard().await,
                Claim::Retry => {}
            }
        }
    }

    async fn claim(&self, spare_first: bool, memory_mib: u32) -> Result<Claim, MachineError> {
        if memory_mib + MACHINE_OVERHEAD_MIB > self.limits.memory_mib {
            return Err(MachineError::Capacity);
        }
        let settled = self.spare_settled.notified();
        tokio::pin!(settled);
        settled.as_mut().enable();
        {
            let mut state = self.state.lock().await;
            if state.shutting_down {
                return Err(MachineError::ShuttingDown);
            }
            if spare_first && let Some(spare) = state.spares.pop() {
                return Ok(Claim::Spare(spare));
            }
            if let Some(admission) = self.try_admit(memory_mib).map_err(MachineError::Io)? {
                return Ok(Claim::Admitted(admission));
            }
            if let Some(spare) = state.spares.pop() {
                return Ok(Claim::Spare(spare));
            }
            if state.spares_booting == 0 {
                return Err(MachineError::Capacity);
            }
        }
        settled.await;
        Ok(Claim::Retry)
    }

    fn try_admit(&self, memory_mib: u32) -> io::Result<Option<Admission>> {
        let required_mib = memory_mib + MACHINE_OVERHEAD_MIB;
        if available_memory_mib()?.saturating_sub(HOST_RESERVE_MIB) < required_mib {
            return Ok(None);
        }
        Ok(reserve(&self.slots, &self.memory, &self.seats, required_mib).ok())
    }

    async fn admit(
        self: &Arc<Self>,
        booted: Result<(Booted, Ownership), MachineError>,
        request_started: Instant,
        spec: MachineSpec,
    ) -> Result<Arc<Machine>, MachineError> {
        let admitted = async {
            let (booted, ownership) = booted?;
            let machine = booted
                .record(
                    request_started,
                    ownership,
                    spec.idle_timeout_secs,
                    spec.egress,
                )
                .await
                .map_err(MachineError::Boot)?;
            let mut state = self.state.lock().await;
            if state.shutting_down {
                drop(state);
                let _ = machine.exclusive().await.destroy().await;
                return Err(MachineError::ShuttingDown);
            }
            let machine = Arc::new(machine);
            let id = machine.id().to_owned();
            state.machines.insert(id, machine.clone());
            drop(state);
            self.destroy_when_idle(&machine);
            Ok(machine)
        };
        let admitted = admitted.await;
        self.boot_spares();
        admitted
    }

    fn boot_spares(self: &Arc<Self>) {
        let machines = self.clone();
        tokio::spawn(async move {
            loop {
                let admission = {
                    let mut state = machines.state.lock().await;
                    let spares = state.spares.len() + state.spares_booting;
                    if state.shutting_down
                        || machines.admission_requests.load(Ordering::SeqCst) > 0
                        || spares >= machines.limits.spare_machines
                    {
                        return;
                    }
                    let Ok(Some(admission)) = machines.try_admit(DEFAULT_MEMORY_MIB) else {
                        return;
                    };
                    state.spares_booting += 1;
                    admission
                };
                let no_egress_until_taken = EgressPolicy::default();
                let spare = machines
                    .boot_in(
                        admission,
                        DEFAULT_VCPUS,
                        DEFAULT_MEMORY_MIB,
                        no_egress_until_taken,
                    )
                    .await;
                let mut state = machines.state.lock().await;
                state.spares_booting -= 1;
                machines.spare_settled.notify_waiters();
                match spare {
                    Ok(spare) if state.shutting_down => {
                        drop(state);
                        spare.discard().await;
                        return;
                    }
                    Ok(spare) => {
                        tracing::info!(spares = state.spares.len() + 1, "spare machine booted");
                        state.spares.push(spare);
                    }
                    Err(error) => {
                        tracing::error!(%error, "spare machine failed to boot");
                        return;
                    }
                }
            }
        });
    }

    pub async fn spares(&self) -> usize {
        self.state.lock().await.spares.len()
    }

    pub async fn update(
        &self,
        tenant: &Tenant,
        id: &str,
        request: UpdateMachine,
    ) -> Result<MachineInfo, MachineError> {
        let egress = EgressPolicy::parse(&request.allowed_hosts).map_err(MachineError::Invalid)?;
        let machine = self
            .get(tenant, id)
            .await
            .ok_or(MachineError::UnknownMachine)?;
        run_to_completion(async move {
            let machine = machine.exclusive().await;
            machine.set_egress(egress).await?;
            Ok(machine.info())
        })
        .await
    }

    pub async fn get(&self, tenant: &Tenant, id: &str) -> Option<Arc<Machine>> {
        self.state
            .lock()
            .await
            .machines
            .get(id)
            .filter(|machine| machine.belongs_to(tenant))
            .cloned()
    }

    pub async fn begin_use(&self, tenant: &Tenant, id: &str) -> Option<MachineUse> {
        self.state
            .lock()
            .await
            .machines
            .get(id)
            .filter(|machine| machine.belongs_to(tenant))
            .map(Machine::begin_use)
    }

    async fn destroy_if_expired(self: &Arc<Self>, id: &str) -> ControlFlow<()> {
        let Some(machine) = self.state.lock().await.machines.get(id).cloned() else {
            return ControlFlow::Break(());
        };
        let _turn = machine.tenant().take_turn(machine.idempotency()).await;
        let expired = {
            let mut state = self.state.lock().await;
            match state.machines.get(id) {
                Some(listed) if Arc::ptr_eq(listed, &machine) && !machine.is_expired() => {
                    return ControlFlow::Continue(());
                }
                Some(listed) if Arc::ptr_eq(listed, &machine) => state.machines.remove(id),
                _ => None,
            }
        };
        if let Some(machine) = expired
            && let Err(error) = self.destroy_unlisted(machine).await
        {
            tracing::error!(machine_id = %id, %error, "expired machine was not destroyed");
        }
        ControlFlow::Break(())
    }

    pub async fn list(&self, tenant: &Tenant) -> Vec<MachineInfo> {
        let mut infos: Vec<_> = self
            .state
            .lock()
            .await
            .machines
            .values()
            .filter(|machine| machine.belongs_to(tenant))
            .map(|machine| machine.info())
            .collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }

    pub async fn states(&self) -> Vec<MachineState> {
        self.state
            .lock()
            .await
            .machines
            .values()
            .map(|machine| machine.state())
            .collect()
    }

    pub async fn pause(
        self: &Arc<Self>,
        tenant: &Tenant,
        id: &str,
    ) -> Result<MachineInfo, MachineError> {
        let machine = self
            .begin_use(tenant, id)
            .await
            .ok_or(MachineError::UnknownMachine)?;
        let machines = self.clone();
        run_to_completion(async move {
            let paused = machine.exclusive().await.pause().await;
            machines.boot_spares();
            log_outcome(
                &paused,
                machine.id(),
                "machine paused",
                "machine pause failed",
            );
            paused?;
            Ok(machine.info())
        })
        .await
    }

    pub async fn resume(
        self: &Arc<Self>,
        tenant: &Tenant,
        id: &str,
    ) -> Result<MachineInfo, MachineError> {
        let machine = self
            .begin_use(tenant, id)
            .await
            .ok_or(MachineError::UnknownMachine)?;
        let machines = self.clone();
        run_to_completion(async move {
            let machine = machine.exclusive().await;
            let resumed = async {
                if machine.is_paused()? {
                    let spec = machine.spec();
                    let charge = machine.tenant().charge(spec.vcpus, spec.memory_mib)?;
                    let admission = machines.admission(spec.memory_mib).await?.charged(charge);
                    machine.resume(&machines.jail, admission).await?;
                }
                Ok(machine.info())
            };
            let resumed = resumed.await;
            machines.boot_spares();
            log_outcome(
                &resumed,
                machine.id(),
                "machine resumed",
                "machine resume failed",
            );
            resumed
        })
        .await
    }

    pub async fn destroy(self: &Arc<Self>, tenant: &Tenant, id: &str) -> Result<(), MachineError> {
        let machine = self
            .get(tenant, id)
            .await
            .ok_or(MachineError::UnknownMachine)?;
        let turn = tenant.take_turn(machine.idempotency()).await;
        let machine = {
            let mut state = self.state.lock().await;
            match state.machines.get(id) {
                Some(listed) if Arc::ptr_eq(listed, &machine) => state.machines.remove(id),
                _ => None,
            }
        }
        .ok_or(MachineError::UnknownMachine)?;
        let machines = self.clone();
        run_to_completion(async move {
            let _turn = turn;
            machines
                .destroy_unlisted(machine)
                .await
                .map_err(MachineError::Io)
        })
        .await
    }

    async fn destroy_unlisted(self: &Arc<Self>, machine: Arc<Machine>) -> io::Result<()> {
        let destroyed = machine.exclusive().await.destroy().await;
        log_outcome(
            &destroyed,
            machine.id(),
            "machine destroyed",
            "machine destroy failed",
        );
        if destroyed.is_err() {
            drop(machine.begin_use());
            let mut state = self.state.lock().await;
            state
                .machines
                .insert(machine.id().to_owned(), machine.clone());
            drop(state);
            self.destroy_when_idle(&machine);
        }
        self.boot_spares();
        destroyed
    }

    pub async fn create_snapshot(
        self: &Arc<Self>,
        tenant: &Arc<Tenant>,
        request: CreateSnapshot,
        key: Option<IdempotencyKey>,
    ) -> Result<SnapshotInfo, MachineError> {
        let attempt = Idempotency::of(key, format!("snapshot {}", request.machine_id));
        let turn = tenant.take_turn(attempt.as_ref()).await;
        if let Some(info) = self.replayed_snapshot(tenant, attempt.as_ref()).await? {
            return Ok(info);
        }
        let machine = self
            .begin_use(tenant, &request.machine_id)
            .await
            .ok_or(MachineError::UnknownMachine)?;
        let machines = self.clone();
        let tenant = tenant.clone();
        run_to_completion(async move {
            let _turn = turn;
            let taken = async {
                let machine = machine.exclusive().await;
                let spec = machine.spec();
                let provisional = tenant.reserve_disk(provisioned_disk_mib(spec.memory_mib))?;
                let mut info = SnapshotInfo {
                    id: new_id(),
                    machine_id: machine.id().to_owned(),
                    vcpus: spec.vcpus,
                    memory_mib: spec.memory_mib,
                    allowed_hosts: spec.egress.patterns(),
                    created_unix_ms: unix_ms(),
                    stored_bytes: 0,
                };
                let dir = machines.snapshots_dir.join(&info.id);
                tokio::fs::create_dir(&dir)
                    .await
                    .map_err(MachineError::Io)?;
                let saved = machine.save_into(&dir).await;
                drop(machine);
                let recorded = match saved {
                    Ok(()) => match stored_bytes(&dir).await {
                        Ok(stored) => {
                            info.stored_bytes = stored;
                            let record = SnapshotRecord {
                                info: info.clone(),
                                tenant: tenant.id().to_owned(),
                                idempotency: attempt.clone(),
                            };
                            files::commit(&dir, SNAPSHOT_RECORD, &record)
                                .await
                                .map_err(MachineError::Io)
                        }
                        Err(error) => Err(MachineError::Io(error)),
                    },
                    Err(error) => Err(error),
                };
                if let Err(error) = recorded {
                    if let Err(error) = files::discard(&dir, SNAPSHOT_RECORD).await {
                        tracing::error!(path = %dir.display(), %error, "snapshot cleanup failed");
                    }
                    return Err(error);
                }
                let mib = stored_mib(info.stored_bytes);
                let kept = tenant.keep_disk(mib);
                drop(provisional);
                let snapshot = Snapshot {
                    info: info.clone(),
                    storage: std::sync::Mutex::new(Some(tenant.store(
                        kept,
                        mib,
                        info.created_unix_ms,
                    ))),
                    tenant: tenant.clone(),
                    idempotency: attempt,
                    dir,
                    deleted: RwLock::new(false),
                };
                let mut state = machines.state.lock().await;
                state.snapshots.insert(info.id.clone(), Arc::new(snapshot));
                Ok(info)
            }
            .await;
            machines.boot_spares();
            match &taken {
                Ok(info) => tracing::info!(snapshot_id = %info.id, machine_id = %info.machine_id, "snapshot created"),
                Err(error) => tracing::warn!(machine_id = %request.machine_id, %error, "snapshot failed"),
            }
            taken
        })
        .await
    }

    pub async fn snapshots(&self, tenant: &Tenant) -> Vec<SnapshotInfo> {
        let mut infos: Vec<_> = self
            .state
            .lock()
            .await
            .snapshots
            .values()
            .filter(|snapshot| snapshot.tenant.id() == tenant.id())
            .map(|snapshot| snapshot.info.clone())
            .collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }

    pub async fn snapshot_info(&self, tenant: &Tenant, id: &str) -> Option<SnapshotInfo> {
        Some(self.snapshot(tenant, id).await?.info.clone())
    }

    pub async fn delete_snapshot(
        self: &Arc<Self>,
        tenant: &Tenant,
        id: &str,
    ) -> Result<(), MachineError> {
        let snapshot = self
            .snapshot(tenant, id)
            .await
            .ok_or(MachineError::UnknownSnapshot)?;
        let turn = tenant.take_turn(snapshot.idempotency.as_ref()).await;
        let snapshot = {
            let mut state = self.state.lock().await;
            match state.snapshots.get(id) {
                Some(listed) if Arc::ptr_eq(listed, &snapshot) => state.snapshots.remove(id),
                _ => None,
            }
        }
        .ok_or(MachineError::UnknownSnapshot)?;
        let machines = self.clone();
        let snapshot_id = snapshot.info.id.clone();
        run_to_completion(async move {
            let _turn = turn;
            let mut deleted = snapshot.deleted.write().await;
            let discarded = files::discard(&snapshot.dir, SNAPSHOT_RECORD).await;
            *deleted = discarded.is_ok();
            drop(deleted);
            if discarded.is_ok() {
                drop(lock(&snapshot.storage).take());
            }
            if discarded.is_err() {
                let mut state = machines.state.lock().await;
                state.snapshots.insert(snapshot.info.id.clone(), snapshot);
            }
            match &discarded {
                Ok(()) => tracing::info!(%snapshot_id, "snapshot deleted"),
                Err(error) => tracing::warn!(%snapshot_id, %error, "snapshot delete failed"),
            }
            discarded.map_err(MachineError::Io)
        })
        .await
    }

    pub async fn shutdown(&self) {
        let (machines, spares) = {
            let mut state = self.state.lock().await;
            state.shutting_down = true;
            let machines: Vec<_> = state.machines.values().cloned().collect();
            (machines, std::mem::take(&mut state.spares))
        };
        let spares = spares
            .into_iter()
            .map(|spare| tokio::spawn(spare.discard()));
        let parks = machines
            .into_iter()
            .map(|machine| tokio::spawn(async move { machine.exclusive().await.park().await }));
        let stopping: Vec<_> = spares.chain(parks).collect();
        for stopped in stopping {
            if let Err(error) = stopped.await {
                std::panic::resume_unwind(error.into_panic());
            }
        }
        let every_vm_has_exited = self
            .slots
            .clone()
            .acquire_many_owned(u32::try_from(self.limits.running_vms).unwrap_or(u32::MAX))
            .await;
        drop(every_vm_has_exited);
        self.jail.release_cgroup();
        if let Err(error) = self.tenants.flush().await {
            tracing::error!(%error, "usage was not recorded at shutdown");
        }
    }

    pub fn create_counts(&self) -> (u64, u64) {
        (
            self.create_successes.load(Ordering::Relaxed),
            self.create_failures.load(Ordering::Relaxed),
        )
    }

    pub fn resource_usage(&self) -> (usize, u32, u32) {
        let reserved = self.limits.memory_mib as usize - self.memory.available_permits();
        (
            self.limits.running_vms - self.slots.available_permits(),
            u32::try_from(reserved).unwrap_or(u32::MAX),
            self.limits.memory_mib,
        )
    }

    async fn snapshot(&self, tenant: &Tenant, id: &str) -> Option<Arc<Snapshot>> {
        self.state
            .lock()
            .await
            .snapshots
            .get(id)
            .filter(|snapshot| snapshot.tenant.id() == tenant.id())
            .cloned()
    }
}

async fn flush_usage_periodically(machines: Weak<Machines>) {
    loop {
        tokio::time::sleep(USAGE_FLUSH_INTERVAL).await;
        let Some(machines) = machines.upgrade() else {
            return;
        };
        if let Err(error) = machines.tenants.flush().await {
            tracing::error!(%error, "usage was not recorded");
        }
    }
}

async fn stored_bytes(dir: &Path) -> io::Result<u64> {
    let mut total = 0;
    for name in SAVED_STATE.into_iter().chain([SCRATCH]) {
        total +=
            std::os::unix::fs::MetadataExt::blocks(&tokio::fs::metadata(dir.join(name)).await?)
                * 512;
    }
    Ok(total)
}

fn stored_mib(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 20)
}

async fn destroy_when_idle(
    machines: Weak<Machines>,
    id: String,
    mut activity: watch::Receiver<Activity>,
    idle_timeout: Duration,
) {
    while idle_deadline_passes(&mut activity, idle_timeout).await {
        let Some(machines) = machines.upgrade() else {
            return;
        };
        if machines.destroy_if_expired(&id).await.is_break() {
            return;
        }
    }
}

async fn idle_deadline_passes(
    activity: &mut watch::Receiver<Activity>,
    idle_timeout: Duration,
) -> bool {
    loop {
        let deadline = activity.borrow_and_update().idle_deadline(idle_timeout);
        let changed = match deadline {
            None => activity.changed().await,
            Some(deadline) => tokio::select! {
                changed = activity.changed() => changed,
                () = tokio::time::sleep_until(deadline) => return true,
            },
        };
        if changed.is_err() {
            return false;
        }
    }
}

fn ensure_open_file_limit(required: u64) -> io::Result<()> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let Rlimit { current, maximum } = getrlimit(Resource::Nofile);
    if current.is_none_or(|current| current >= required) {
        return Ok(());
    }
    if maximum.is_some_and(|maximum| maximum < required) {
        return Err(io::Error::other(format!(
            "machined needs {required} open files but the hard limit is {}",
            maximum.unwrap_or_default()
        )));
    }
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(required),
            maximum,
        },
    )?;
    Ok(())
}

fn log_outcome<T, E: std::fmt::Display>(
    outcome: &Result<T, E>,
    machine_id: &str,
    done: &str,
    failed: &str,
) {
    match outcome {
        Ok(_) => tracing::info!(%machine_id, "{done}"),
        Err(error) => tracing::warn!(%machine_id, %error, "{failed}"),
    }
}

fn reserve(
    slots: &Arc<Semaphore>,
    memory: &Arc<Semaphore>,
    seats: &Arc<std::sync::Mutex<Vec<u32>>>,
    required_mib: u32,
) -> Result<Admission, MachineError> {
    let slot = slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| MachineError::Capacity)?;
    let memory = memory
        .clone()
        .try_acquire_many_owned(required_mib)
        .map_err(|_| MachineError::Capacity)?;
    let uid = lock(seats)
        .pop()
        .ok_or_else(|| MachineError::Io(io::Error::other("every jail uid is in use")))?;
    Ok(Admission {
        _slot: slot,
        _memory: memory,
        seat: Seat {
            uid,
            free: seats.clone(),
        },
        tenant: None,
    })
}

pub fn default_memory_budget_mib() -> io::Result<u32> {
    let available = available_memory_mib()?;
    Ok(available.saturating_sub(HOST_RESERVE_MIB.max(available / 10)))
}

fn available_memory_mib() -> io::Result<u32> {
    let meminfo = std::fs::read_to_string("/proc/meminfo")?;
    parse_available_memory(&meminfo)
}

fn parse_available_memory(meminfo: &str) -> io::Result<u32> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|kib| u32::try_from(kib / 1024).ok())
        .ok_or_else(|| io::Error::other("/proc/meminfo has no valid MemAvailable"))
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

async fn run_to_completion<T: Send + 'static>(work: impl Future<Output = T> + Send + 'static) -> T {
    match tokio::spawn(work).await {
        Ok(output) => output,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}

async fn subdirectories(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut found = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            found.push((name.to_owned(), entry.path()));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_reserves_memory_atomically_and_releases_failed_attempts() {
        let slots = Arc::new(Semaphore::new(2));
        let memory = Arc::new(Semaphore::new(1024));
        let seats = Arc::new(std::sync::Mutex::new(vec![7, 8]));
        let first = reserve(&slots, &memory, &seats, 768).unwrap();
        assert!(matches!(
            reserve(&slots, &memory, &seats, 512),
            Err(MachineError::Capacity)
        ));
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(memory.available_permits(), 256);
        assert_eq!(*lock(&seats), vec![7]);
        drop(first);
        let second = reserve(&slots, &memory, &seats, 1024).unwrap();
        assert_eq!(memory.available_permits(), 0);
        assert_eq!(second.uid(), 8);
    }

    #[test]
    fn available_memory_requires_the_available_field_and_converts_kib() {
        assert_eq!(
            parse_available_memory("MemTotal: 999999 kB\nMemAvailable: 2097152 kB\n").unwrap(),
            2048
        );
        assert!(parse_available_memory("MemFree: 100 kB").is_err());
        assert!(parse_available_memory("MemAvailable: invalid kB").is_err());
    }
}
