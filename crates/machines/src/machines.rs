use std::{
    collections::HashMap,
    io,
    net::IpAddr,
    ops::ControlFlow,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::{
    Mutex, Notify, OwnedSemaphorePermit, RwLock, RwLockWriteGuard, Semaphore, watch,
};

use crate::{
    contract::{
        CreateMachine, CreatePreview, CreateSnapshot, ExecutionUsage, Generation, MachineInfo,
        MachineState, PreviewInfo, ReportedMachine, Room, SnapshotInfo, UpdateMachine,
    },
    egress::{Egress, EgressPolicy, MAX_MACHINE_EGRESS_FILES, can_send_from},
    files,
    firecracker::{Jail, SAVED_STATE, SCRATCH},
    gateway::Gateway,
    machine::{
        Activity, BootError, Booted, Disk, Image, Machine, MachineSpec, MachineUse, Ownership,
        SAVED_FILES, provisioned_disk_mib,
    },
    previews::{
        DEFAULT_PREVIEW_TTL_SECS, MAX_PREVIEW_CONNECTIONS, MAX_PREVIEW_TTL_SECS,
        MAX_PREVIEWS_PER_MACHINE, PREVIEW_FILES_PER_CONNECTION, Preview, PreviewOrigin, PreviewUse,
    },
    store::{Digests, Store, StoredSnapshot, seed_image_cache},
    tenants::{
        Idempotency, IdempotencyKey, Storage, Tenant, TenantCharge, TenantConfig, Tenants, unix_ms,
    },
};

pub enum Tenancy {
    Local(Vec<TenantConfig>),
    Fleet { pool: String, store: Arc<Store> },
}

struct Fleet {
    pool: String,
    store: Arc<Store>,
    images_dir: PathBuf,
    abandoned_dir: PathBuf,
    digests: Digests,
    fetching: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    boot: u64,
    changes: AtomicU64,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct CopyRecord {
    store: String,
    info: SnapshotInfo,
    tenant: String,
}

#[derive(serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Standalone,
    Fleet,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ModeRecord {
    mode: Mode,
    #[serde(default)]
    boots: u64,
}

pub enum Settlement {
    Made(MachineInfo),
    Nothing,
}

#[derive(Default)]
pub struct Network {
    pub egress_addresses: Vec<IpAddr>,
    pub previews: Option<PreviewOrigin>,
}

const DEFAULT_VCPUS: u8 = 2;
pub const DEFAULT_MEMORY_MIB: u32 = 512;
const MAX_VCPUS: u8 = 32;
const DEFAULT_IDLE_TIMEOUT_SECS: u32 = 600;
const MEMORY_MIB_RANGE: std::ops::RangeInclusive<u32> = 128..=32768;
const IDLE_TIMEOUT_SECS_RANGE: std::ops::RangeInclusive<u32> = 1..=86400;
const OPEN_FILES_OUTSIDE_EGRESS: u64 = 4096;
pub(crate) const MACHINE_OVERHEAD_MIB: u32 = 64;
const HOST_RESERVE_MIB: u32 = 512;
const SNAPSHOT_RECORD: &str = "snapshot.json";
const COPY_RECORD: &str = "copy.json";
const MODE_RECORD: &str = "mode.json";
const ABANDONED_KEY_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const USAGE_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum MachineError {
    Invalid(String),
    UnknownMachine,
    UnknownSnapshot,
    UnknownPreview,
    Conflict(String),
    QuotaExceeded(String),
    RateLimited {
        retry_after: Duration,
    },
    Capacity,
    ShuttingDown,
    Store(io::Error),
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
            Self::Io(error) | Self::Store(error) => Some(error),
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
            Self::UnknownPreview => write!(f, "unknown preview"),
            Self::Capacity => write!(f, "machine capacity reached"),
            Self::ShuttingDown => write!(f, "host is shutting down"),
            Self::Store(error) => write!(f, "snapshot store: {error}"),
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

    pub(crate) fn start_metering(&mut self, machine_id: &str) {
        if let Some(charge) = &mut self.tenant {
            charge.start_metering(machine_id);
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
    gateway: Arc<Gateway>,
    network: Network,
    tenants: Tenants,
    fleet: Option<Fleet>,
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
    previews: HashMap<String, Preview>,
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

enum Restorable {
    Local(Arc<Snapshot>),
    Stored(StoredSnapshot),
}

impl Restorable {
    fn info(&self) -> &SnapshotInfo {
        match self {
            Self::Local(snapshot) => &snapshot.info,
            Self::Stored(stored) => &stored.info,
        }
    }
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
        tenancy: Tenancy,
        network: Network,
    ) -> io::Result<Arc<Self>> {
        let image = Image::in_dir(&image_dir)?;
        let preview_files = match network.previews {
            Some(_) => MAX_PREVIEW_CONNECTIONS as u64 * PREVIEW_FILES_PER_CONNECTION,
            None => 0,
        };
        ensure_open_file_limit(
            limits.running_vms as u64 * MAX_MACHINE_EGRESS_FILES
                + preview_files
                + OPEN_FILES_OUTSIDE_EGRESS,
        )?;
        for address in &network.egress_addresses {
            can_send_from(*address).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("egress address {address} cannot be used on this host: {error}"),
                )
            })?;
        }
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
        let boots = record_mode(&state_dir, &tenancy).await?;
        let (tenants, fleet) = match tenancy {
            Tenancy::Local(configs) => (Tenants::open(&state_dir, configs).await?, None),
            Tenancy::Fleet { pool, store } => {
                let images_dir = state_dir.join("images");
                let abandoned_dir = state_dir.join("abandoned");
                for dir in [&images_dir, &abandoned_dir] {
                    tokio::fs::create_dir_all(dir).await?;
                    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
                }
                remove_unused_images(&images_dir).await?;
                remove_old_files(&abandoned_dir, ABANDONED_KEY_RETENTION).await?;
                let fleet = Fleet {
                    pool,
                    store,
                    images_dir: tokio::fs::canonicalize(images_dir).await?,
                    abandoned_dir: tokio::fs::canonicalize(abandoned_dir).await?,
                    digests: Digests::new(),
                    fetching: std::sync::Mutex::default(),
                    boot: boots,
                    changes: AtomicU64::new(0),
                };
                (Tenants::open_for_fleet(&state_dir).await?, Some(fleet))
            }
        };
        let gateway = Gateway::open(&state_dir).await?;
        let mut state = State::default();
        for (id, dir) in subdirectories(&machines_dir).await? {
            match Machine::recover(id.clone(), dir.clone(), &tenants, gateway.clone()).await {
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
            let recovered = match &fleet {
                None => recover_snapshot(&dir, &tenants).await,
                Some(fleet) => recover_copy(&dir, fleet, &tenants).await,
            };
            match recovered {
                Ok(Some(snapshot)) => {
                    state.snapshots.insert(id, Arc::new(snapshot));
                }
                Ok(None) => tokio::fs::remove_dir_all(&dir).await?,
                Err(error) => {
                    tracing::error!(path = %dir.display(), %error, "ignoring unreadable snapshot");
                }
            }
        }
        if let Some(fleet) = &fleet {
            drop_deleted_copies(&fleet.store, &mut state.snapshots).await;
        }
        for machine in state.machines.values() {
            if let Some(source) = machine.egress_source()
                && !network.egress_addresses.contains(&source)
            {
                tracing::error!(machine_id = %machine.id(), egress_address = %source, "machine's egress address is no longer in SLOP_EGRESS_ADDRESSES; its egress will fail");
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
            gateway,
            network,
            tenants,
            fleet,
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
        if machines.fleet.is_some() {
            let seeding = machines.clone();
            tokio::spawn(async move {
                let Some(fleet) = &seeding.fleet else {
                    return;
                };
                let files = seeding.image.saved_with();
                if let Err(error) =
                    seed_image_cache(&fleet.digests, &files, &fleet.images_dir).await
                {
                    tracing::warn!(%error, "this host's image was not offered to snapshot downloads");
                }
                fleet.store.sweep_abandoned_uploads().await;
            });
        }
        Ok(machines)
    }

    pub fn tenant(&self, id: &str) -> Option<Arc<Tenant>> {
        match &self.fleet {
            Some(_) if is_tenant_id(id) => Some(self.tenants.owner(id)),
            Some(_) => None,
            None => self.tenants.get(id),
        }
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
        self.refuse_abandoned(tenant, attempt.as_ref()).await?;
        let snapshot = match &request.snapshot_id {
            None => None,
            Some(_) if request.vcpus.is_some() || request.memory_mib.is_some() => {
                return Err(MachineError::Invalid(
                    "vcpus and memory_mib come from the snapshot".into(),
                ));
            }
            Some(id) => Some(self.restorable_snapshot(tenant, id).await?),
        };
        let (vcpus, memory_mib, inherited_hosts) = match &snapshot {
            Some(snapshot) => (
                snapshot.info().vcpus,
                snapshot.info().memory_mib,
                snapshot.info().allowed_hosts.clone(),
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
        let source = request
            .egress_address
            .as_deref()
            .map(|address| self.egress_address(address))
            .transpose()?;
        let egress = EgressPolicy::new(
            &allowed_hosts,
            &request.egress_credentials.unwrap_or_default(),
            source,
        )
        .map_err(MachineError::Invalid)?;
        let spec = MachineSpec {
            vcpus,
            memory_mib,
            idle_timeout_secs,
            egress,
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
                Some(snapshot) => match machines.local_copy(snapshot).await {
                    Err(error) => Err(error),
                    Ok(snapshot) => {
                        let deleted = snapshot.deleted.read().await;
                        if *deleted {
                            Err(MachineError::UnknownSnapshot)
                        } else {
                            machines
                                .restore(&snapshot.dir, Disk::Copy, &spec, charge)
                                .await
                        }
                    }
                },
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
        self.refuse_abandoned(tenant, attempt.as_ref()).await?;
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

    fn egress_address(&self, address: &str) -> Result<IpAddr, MachineError> {
        address
            .parse()
            .ok()
            .filter(|address| self.network.egress_addresses.contains(address))
            .ok_or_else(|| {
                MachineError::Invalid(format!(
                    "egress_address {address:?} is not one of this host's egress addresses"
                ))
            })
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
        let egress = Egress::new(egress, self.gateway.clone());
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
        let (vcpus, memory_mib) = (spec.vcpus, spec.memory_mib);
        let egress = Egress::new(spec.egress.clone(), self.gateway.clone());
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
            self.changed();
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
        if request.allowed_hosts.is_none() && request.egress_credentials.is_none() {
            return Err(MachineError::Invalid(
                "an update sets allowed_hosts, egress_credentials or both".into(),
            ));
        }
        let machine = self
            .get(tenant, id)
            .await
            .ok_or(MachineError::UnknownMachine)?;
        run_to_completion(async move {
            let machine = machine.exclusive().await;
            let mut egress = machine.spec().egress;
            if let Some(allowed_hosts) = &request.allowed_hosts {
                egress = egress
                    .with_allowed_hosts(allowed_hosts)
                    .map_err(MachineError::Invalid)?;
            }
            if let Some(credentials) = &request.egress_credentials {
                egress = egress
                    .with_credentials(credentials)
                    .map_err(MachineError::Invalid)?;
            }
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
                Some(listed) if Arc::ptr_eq(listed, &machine) => state.unlist(id),
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
            machines.changed();
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
            machines.changed();
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
                Some(listed) if Arc::ptr_eq(listed, &machine) => state.unlist(id),
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
        self.changed();
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
        let fleet_snapshot_id = match (&self.fleet, &attempt) {
            (None, _) => None,
            (Some(_), None) => {
                return Err(MachineError::Invalid(
                    "a fleet host takes snapshots only for operations with a key".into(),
                ));
            }
            (Some(fleet), Some(attempt)) => match fleet.store.snapshot(attempt.key()).await {
                Ok(Some(stored)) if stored.tenant == tenant.id() => return Ok(stored.info),
                Ok(Some(_)) => {
                    return Err(MachineError::Conflict(format!(
                        "operation {} belongs to another tenant",
                        attempt.key()
                    )));
                }
                Ok(None) => Some(attempt.key().to_owned()),
                Err(error) => return Err(MachineError::Store(error)),
            },
        };
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
                    id: fleet_snapshot_id.unwrap_or_else(new_id),
                    machine_id: machine.id().to_owned(),
                    vcpus: spec.vcpus,
                    memory_mib: spec.memory_mib,
                    allowed_hosts: spec.egress.allowed_hosts(),
                    created_unix_ms: unix_ms(),
                    stored_bytes: 0,
                    pool: machines.fleet.as_ref().map(|fleet| fleet.pool.clone()),
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
                            machines
                                .record_snapshot(&tenant, attempt.clone(), &info, &dir)
                                .await
                        }
                        Err(error) => Err(MachineError::Io(error)),
                    },
                    Err(error) => Err(error),
                };
                let kept_locally = match recorded {
                    Ok(kept_locally) => kept_locally,
                    Err(error) => {
                        if let Err(error) = files::discard(&dir, SNAPSHOT_RECORD).await {
                            tracing::error!(path = %dir.display(), %error, "snapshot cleanup failed");
                        }
                        return Err(error);
                    }
                };
                if machines.fleet.is_some() {
                    machines.changed();
                    if kept_locally {
                        let copy = Snapshot {
                            info: info.clone(),
                            storage: std::sync::Mutex::new(None),
                            tenant: tenant.clone(),
                            idempotency: None,
                            dir,
                            deleted: RwLock::new(false),
                        };
                        let mut state = machines.state.lock().await;
                        state.snapshots.insert(info.id.clone(), Arc::new(copy));
                    }
                    return Ok(info);
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

    async fn record_snapshot(
        &self,
        tenant: &Tenant,
        idempotency: Option<Idempotency>,
        info: &SnapshotInfo,
        dir: &Path,
    ) -> Result<bool, MachineError> {
        let Some(fleet) = &self.fleet else {
            let record = SnapshotRecord {
                info: info.clone(),
                tenant: tenant.id().to_owned(),
                idempotency,
            };
            files::commit(dir, SNAPSHOT_RECORD, &record)
                .await
                .map_err(MachineError::Io)?;
            return Ok(true);
        };
        let stored = fleet
            .store
            .publish(tenant.id(), info, dir, &SAVED_FILES, &fleet.digests)
            .await
            .map_err(|error| match error.kind() {
                io::ErrorKind::AlreadyExists => MachineError::Conflict(error.to_string()),
                _ => MachineError::Store(error),
            })?;
        let kept = async {
            stored.link_images_into(dir, &fleet.images_dir).await?;
            files::commit(dir, COPY_RECORD, &fleet.copy_record(&stored)).await
        };
        if let Err(error) = kept.await {
            tracing::warn!(snapshot_id = %info.id, %error, "the published snapshot was not kept on this host");
            if let Err(error) = files::discard(dir, COPY_RECORD).await {
                tracing::error!(path = %dir.display(), %error, "snapshot cleanup failed");
            }
            return Ok(false);
        }
        Ok(true)
    }

    pub async fn snapshots(&self, tenant: &Tenant) -> Result<Vec<SnapshotInfo>, MachineError> {
        if let Some(fleet) = &self.fleet {
            let stored = fleet.store.snapshots().await.map_err(MachineError::Store)?;
            return Ok(stored
                .into_iter()
                .filter(|stored| stored.tenant == tenant.id())
                .map(|stored| stored.info)
                .collect());
        }
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
        Ok(infos)
    }

    pub async fn snapshot_info(
        &self,
        tenant: &Tenant,
        id: &str,
    ) -> Result<SnapshotInfo, MachineError> {
        let found = match &self.fleet {
            Some(fleet) => fleet
                .store
                .snapshot(id)
                .await
                .map_err(MachineError::Store)?
                .filter(|stored| stored.tenant == tenant.id())
                .map(|stored| stored.info),
            None => self
                .snapshot(tenant, id)
                .await
                .map(|snapshot| snapshot.info.clone()),
        };
        found.ok_or(MachineError::UnknownSnapshot)
    }

    pub async fn delete_snapshot(
        self: &Arc<Self>,
        tenant: &Tenant,
        id: &str,
    ) -> Result<(), MachineError> {
        if let Some(fleet) = &self.fleet {
            match fleet.store.snapshot(id).await {
                Ok(Some(stored)) if stored.tenant == tenant.id() => {}
                Ok(_) => return Err(MachineError::UnknownSnapshot),
                Err(error) => return Err(MachineError::Store(error)),
            }
            let machines = self.clone();
            let id = id.to_owned();
            return run_to_completion(async move {
                let fleet = machines.fleet()?;
                if machines.delete_fleet_snapshot(fleet, &id).await? {
                    tracing::info!(snapshot_id = %id, "snapshot deleted");
                    Ok(())
                } else {
                    Err(MachineError::UnknownSnapshot)
                }
            })
            .await;
        }
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

    fn preview_origin(&self) -> Result<&PreviewOrigin, MachineError> {
        self.network.previews.as_ref().ok_or_else(|| {
            MachineError::Conflict("previews are off; start machined with SLOP_PREVIEW_URL".into())
        })
    }

    pub async fn create_preview(
        &self,
        tenant: &Tenant,
        request: CreatePreview,
    ) -> Result<PreviewInfo, MachineError> {
        let origin = self.preview_origin()?;
        let ttl_secs = request.ttl_secs.unwrap_or(DEFAULT_PREVIEW_TTL_SECS);
        if !(1..=MAX_PREVIEW_TTL_SECS).contains(&ttl_secs) {
            return Err(MachineError::Invalid(format!(
                "ttl_secs must be between 1 and {MAX_PREVIEW_TTL_SECS}"
            )));
        }
        if request.port == 0 {
            return Err(MachineError::Invalid(
                "port must be between 1 and 65535".into(),
            ));
        }
        let mut state = self.state.lock().await;
        let machine = state
            .machines
            .get(&request.machine_id)
            .filter(|machine| machine.belongs_to(tenant))
            .ok_or(MachineError::UnknownMachine)?;
        if matches!(machine.state(), MachineState::Exited { .. }) {
            return Err(MachineError::Conflict("machine has exited".into()));
        }
        state.previews.retain(|_, preview| !preview.has_expired());
        let open = state
            .previews
            .values()
            .filter(|preview| preview.machine_id == request.machine_id)
            .count();
        if open >= MAX_PREVIEWS_PER_MACHINE {
            return Err(MachineError::Conflict(format!(
                "a machine may have at most {MAX_PREVIEWS_PER_MACHINE} previews"
            )));
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let preview = Preview::new(request.machine_id, request.port, ttl_secs);
        let info = preview.info(&id, origin);
        state.previews.insert(id, preview);
        tracing::info!(preview_id = %info.id, machine_id = %info.machine_id, port = info.port, expires_unix_ms = info.expires_unix_ms, "preview created");
        Ok(info)
    }

    pub async fn previews(&self, tenant: &Tenant) -> Vec<PreviewInfo> {
        let Some(origin) = self.network.previews.as_ref() else {
            return Vec::new();
        };
        let mut state = self.state.lock().await;
        state.previews.retain(|_, preview| !preview.has_expired());
        let mut infos: Vec<_> = state
            .previews
            .iter()
            .filter(|(_, preview)| state.owns(tenant, preview))
            .map(|(id, preview)| preview.info(id, origin))
            .collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }

    pub async fn preview(&self, tenant: &Tenant, id: &str) -> Option<PreviewInfo> {
        let origin = self.network.previews.as_ref()?;
        let state = self.state.lock().await;
        let preview = state
            .previews
            .get(id)
            .filter(|preview| !preview.has_expired() && state.owns(tenant, preview))?;
        Some(preview.info(id, origin))
    }

    pub async fn delete_preview(&self, tenant: &Tenant, id: &str) -> Result<(), MachineError> {
        let mut state = self.state.lock().await;
        match state.previews.get(id) {
            Some(preview) if !preview.has_expired() && state.owns(tenant, preview) => {
                state.previews.remove(id);
                tracing::info!(preview_id = %id, "preview deleted");
                Ok(())
            }
            _ => Err(MachineError::UnknownPreview),
        }
    }

    pub async fn open_preview(&self, host_header: &str) -> Option<PreviewUse> {
        let id = self.network.previews.as_ref()?.preview_id(host_header)?;
        let mut state = self.state.lock().await;
        if state.previews.get(id)?.has_expired() {
            state.previews.remove(id);
            return None;
        }
        let preview = state.previews.get(id)?;
        let machine = state.machines.get(&preview.machine_id)?.begin_use();
        Some(preview.begin_use(machine))
    }
}

impl Machines {
    fn fleet(&self) -> Result<&Fleet, MachineError> {
        self.fleet
            .as_ref()
            .ok_or_else(|| MachineError::Conflict("this host is not part of a fleet".into()))
    }

    fn changed(&self) {
        if let Some(fleet) = &self.fleet {
            fleet.changes.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub fn generation(&self) -> Generation {
        match &self.fleet {
            Some(fleet) => Generation {
                boot: fleet.boot,
                change: fleet.changes.load(Ordering::SeqCst),
            },
            None => Generation::default(),
        }
    }

    pub fn pool(&self) -> Option<&str> {
        self.fleet.as_ref().map(|fleet| fleet.pool.as_str())
    }

    pub fn store(&self) -> Option<&Store> {
        self.fleet.as_ref().map(|fleet| &*fleet.store)
    }

    pub fn egress_addresses(&self) -> Vec<String> {
        self.network
            .egress_addresses
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    pub async fn report(&self) -> (Generation, Vec<ReportedMachine>, Vec<ExecutionUsage>) {
        self.changed();
        let generation = self.generation();
        let machines = self
            .state
            .lock()
            .await
            .machines
            .values()
            .map(|machine| ReportedMachine {
                id: machine.id().to_owned(),
                tenant: machine.tenant().id().to_owned(),
                memory_mib: machine.info().memory_mib,
                state: machine.state(),
            })
            .collect();
        (generation, machines, self.tenants.recorded_executions())
    }

    pub async fn record_usage(&self) -> io::Result<()> {
        self.tenants.flush().await
    }

    pub async fn forget_executions(&self, ids: &[String]) -> io::Result<()> {
        self.tenants.forget_executions(ids).await
    }

    pub async fn room(&self) -> Room {
        let state = self.state.lock().await;
        if state.shutting_down {
            return Room::default();
        }
        let spares = u32::try_from(state.spares.len() + state.spares_booting).unwrap_or(u32::MAX);
        let spare_mib = spares.saturating_mul(DEFAULT_MEMORY_MIB + MACHINE_OVERHEAD_MIB);
        let slots = u32::try_from(self.slots.available_permits())
            .unwrap_or(u32::MAX)
            .saturating_add(spares);
        let budget_mib = u32::try_from(self.memory.available_permits())
            .unwrap_or(u32::MAX)
            .saturating_add(spare_mib);
        let host_mib = available_memory_mib()
            .unwrap_or(0)
            .saturating_sub(HOST_RESERVE_MIB)
            .saturating_add(spare_mib);
        let memory_mib = budget_mib
            .min(host_mib)
            .saturating_sub(MACHINE_OVERHEAD_MIB)
            .min(*MEMORY_MIB_RANGE.end());
        if slots == 0 || memory_mib < *MEMORY_MIB_RANGE.start() {
            return Room::default();
        }
        Room { slots, memory_mib }
    }

    fn abandoned_path(fleet: &Fleet, tenant: &Tenant, key: &str) -> PathBuf {
        let name = blake3::hash(format!("{}\n{key}", tenant.id()).as_bytes()).to_hex();
        fleet.abandoned_dir.join(name.as_str())
    }

    async fn refuse_abandoned(
        &self,
        tenant: &Tenant,
        attempt: Option<&Idempotency>,
    ) -> Result<(), MachineError> {
        let (Some(fleet), Some(attempt)) = (&self.fleet, attempt) else {
            return Ok(());
        };
        let path = Self::abandoned_path(fleet, tenant, attempt.key());
        if tokio::fs::try_exists(&path)
            .await
            .map_err(MachineError::Io)?
        {
            return Err(MachineError::Conflict(format!(
                "operation {} was settled as having made nothing",
                attempt.key()
            )));
        }
        Ok(())
    }

    pub async fn settle(
        &self,
        tenant: &Tenant,
        key: IdempotencyKey,
    ) -> Result<(Settlement, Generation), MachineError> {
        let fleet = self.fleet()?;
        let attempt = Idempotency::of(Some(key), String::new());
        let _turn = tenant.take_turn(attempt.as_ref()).await;
        let Some(attempt) = attempt else {
            return Err(MachineError::Invalid("an operation has a key".into()));
        };
        let made = self
            .state
            .lock()
            .await
            .machines
            .values()
            .find(|machine| {
                machine.belongs_to(tenant)
                    && machine
                        .idempotency()
                        .is_some_and(|recorded| recorded.key() == attempt.key())
            })
            .map(|machine| machine.info());
        let settlement = match made {
            Some(info) => Settlement::Made(info),
            None => {
                let path = Self::abandoned_path(fleet, tenant, attempt.key());
                tokio::fs::write(&path, attempt.key())
                    .await
                    .map_err(MachineError::Io)?;
                files::sync(&path).await.map_err(MachineError::Io)?;
                files::sync(&fleet.abandoned_dir)
                    .await
                    .map_err(MachineError::Io)?;
                Settlement::Nothing
            }
        };
        Ok((settlement, self.generation()))
    }

    async fn restorable_snapshot(
        &self,
        tenant: &Tenant,
        id: &str,
    ) -> Result<Restorable, MachineError> {
        let Some(fleet) = &self.fleet else {
            return self
                .snapshot(tenant, id)
                .await
                .map(Restorable::Local)
                .ok_or(MachineError::UnknownSnapshot);
        };
        let stored = match fleet.store.snapshot(id).await {
            Ok(Some(stored)) if stored.tenant == tenant.id() => stored,
            Ok(Some(_)) => return Err(MachineError::UnknownSnapshot),
            Ok(None) => {
                if let Some(copy) = self.snapshot(tenant, id).await {
                    let deleted = copy.deleted.write().await;
                    self.forget_copy(&copy, deleted).await;
                }
                return Err(MachineError::UnknownSnapshot);
            }
            Err(error) => return Err(MachineError::Store(error)),
        };
        if stored.info.pool.as_deref() != Some(fleet.pool.as_str()) {
            return Err(MachineError::Conflict(format!(
                "snapshot {id} was taken on CPU pool {:?} and this host is in {:?}",
                stored.info.pool, fleet.pool
            )));
        }
        Ok(Restorable::Stored(stored))
    }

    async fn local_copy(&self, restorable: Restorable) -> Result<Arc<Snapshot>, MachineError> {
        let stored = match restorable {
            Restorable::Local(snapshot) => return Ok(snapshot),
            Restorable::Stored(stored) => stored,
        };
        let fleet = self.fleet()?;
        let id = stored.info.id.clone();
        let turn = fleet.turn(&id);
        let copied = async {
            let _turn = turn.lock().await;
            if let Some(copy) = self.state.lock().await.snapshots.get(&id).cloned() {
                return Ok(copy);
            }
            match fleet.store.snapshot(&id).await {
                Ok(Some(_)) => self.fetch(fleet, stored).await,
                Ok(None) => Err(MachineError::UnknownSnapshot),
                Err(error) => Err(MachineError::Store(error)),
            }
        };
        let copied = copied.await;
        fleet.end_turn(&id, &turn);
        copied
    }

    async fn fetch(
        &self,
        fleet: &Fleet,
        stored: StoredSnapshot,
    ) -> Result<Arc<Snapshot>, MachineError> {
        let id = stored.info.id.clone();
        let started = Instant::now();
        let dir = self.snapshots_dir.join(&id);
        let staging = self.snapshots_dir.join(format!("{id}.fetch"));
        let record = fleet.copy_record(&stored);
        let copied = async {
            files::remove_dir_if_present(&staging).await?;
            tokio::fs::create_dir(&staging).await?;
            fleet
                .store
                .fetch(&stored, &staging, &fleet.images_dir)
                .await?;
            files::publish_dir(&staging, &dir).await?;
            files::commit(&dir, COPY_RECORD, &record).await
        };
        if let Err(error) = copied.await {
            files::remove_dir_logged(&staging).await;
            files::remove_dir_logged(&dir).await;
            tracing::warn!(snapshot_id = %id, %error, "snapshot download failed");
            let gone = error.kind() == io::ErrorKind::NotFound
                && matches!(fleet.store.snapshot(&id).await, Ok(None));
            return Err(if gone {
                MachineError::UnknownSnapshot
            } else {
                MachineError::Store(error)
            });
        }
        let copy = Arc::new(Snapshot {
            tenant: self.tenants.owner(&stored.tenant),
            info: stored.info,
            idempotency: None,
            dir,
            deleted: RwLock::new(false),
            storage: std::sync::Mutex::new(None),
        });
        self.state
            .lock()
            .await
            .snapshots
            .insert(id.clone(), copy.clone());
        tracing::info!(snapshot_id = %id, download_ms = started.elapsed().as_millis(), "snapshot downloaded");
        Ok(copy)
    }

    async fn forget_copy(&self, copy: &Arc<Snapshot>, mut deleted: RwLockWriteGuard<'_, bool>) {
        *deleted = true;
        {
            let mut state = self.state.lock().await;
            if state
                .snapshots
                .get(&copy.info.id)
                .is_some_and(|listed| Arc::ptr_eq(listed, copy))
            {
                state.snapshots.remove(&copy.info.id);
            }
        }
        if let Err(error) = files::discard(&copy.dir, COPY_RECORD).await {
            tracing::error!(snapshot_id = %copy.info.id, %error, "removing a deleted snapshot's copy failed");
        }
    }

    async fn delete_fleet_snapshot(&self, fleet: &Fleet, id: &str) -> Result<bool, MachineError> {
        let turn = fleet.turn(id);
        let deleted = async {
            let _no_download_in_flight = turn.lock().await;
            let copy = self.state.lock().await.snapshots.get(id).cloned();
            let deleted = match &copy {
                Some(copy) => Some(copy.deleted.write().await),
                None => None,
            };
            let existed = fleet
                .store
                .delete_snapshot(id)
                .await
                .map_err(MachineError::Store)?;
            if let (Some(copy), Some(deleted)) = (&copy, deleted) {
                self.forget_copy(copy, deleted).await;
            }
            Ok(existed)
        };
        let deleted = deleted.await;
        fleet.end_turn(id, &turn);
        deleted
    }
}

impl Fleet {
    fn copy_record(&self, stored: &StoredSnapshot) -> CopyRecord {
        CopyRecord {
            store: self.store.url().to_owned(),
            info: stored.info.clone(),
            tenant: stored.tenant.clone(),
        }
    }

    fn turn(&self, id: &str) -> Arc<Mutex<()>> {
        lock(&self.fetching)
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    fn end_turn(&self, id: &str, turn: &Arc<Mutex<()>>) {
        let mut fetching = lock(&self.fetching);
        if Arc::strong_count(turn) == 2 {
            fetching.remove(id);
        }
    }
}

impl State {
    fn unlist(&mut self, id: &str) -> Option<Arc<Machine>> {
        let machine = self.machines.remove(id)?;
        self.previews.retain(|_, preview| preview.machine_id != id);
        Some(machine)
    }

    fn owns(&self, tenant: &Tenant, preview: &Preview) -> bool {
        self.machines
            .get(&preview.machine_id)
            .is_some_and(|machine| machine.belongs_to(tenant))
    }
}

async fn record_mode(state_dir: &Path, tenancy: &Tenancy) -> io::Result<u64> {
    let wanted = match tenancy {
        Tenancy::Local(_) => Mode::Standalone,
        Tenancy::Fleet { .. } => Mode::Fleet,
    };
    let recorded = match files::read_record::<ModeRecord>(state_dir, MODE_RECORD).await {
        Ok(recorded) => Some(recorded),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let boots = match recorded {
        Some(recorded) if recorded.mode != wanted => {
            return Err(io::Error::other(format!(
                "{} was used {}; a state dir serves one mode",
                state_dir.display(),
                match recorded.mode {
                    Mode::Standalone => "by a standalone machined",
                    Mode::Fleet => "by a fleet host",
                }
            )));
        }
        Some(recorded) => recorded.boots,
        None if wanted == Mode::Fleet && holds_state(state_dir).await? => {
            return Err(io::Error::other(format!(
                "{} holds machines or snapshots of a standalone machined; a fleet host needs a state dir of its own",
                state_dir.display()
            )));
        }
        None => 0,
    };
    let record = ModeRecord {
        mode: wanted,
        boots: boots + 1,
    };
    files::replace_record(state_dir, MODE_RECORD, &record).await?;
    Ok(record.boots)
}

async fn holds_state(state_dir: &Path) -> io::Result<bool> {
    for kept in ["machines", "snapshots"] {
        if !subdirectories(&state_dir.join(kept)).await?.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn recover_snapshot(dir: &Path, tenants: &Tenants) -> io::Result<Option<Snapshot>> {
    if !tokio::fs::try_exists(dir.join(SNAPSHOT_RECORD)).await? {
        return Ok(None);
    }
    let record: SnapshotRecord = files::read_record(dir, SNAPSHOT_RECORD).await?;
    let tenant = tenants.owner(&record.tenant);
    let mib = stored_mib(record.info.stored_bytes);
    let metered_since = record.info.created_unix_ms.max(tenants.recorded_as_of());
    let storage = tenant.store(tenant.keep_disk(mib), mib, metered_since);
    Ok(Some(Snapshot {
        info: record.info,
        tenant,
        idempotency: record.idempotency,
        dir: dir.to_owned(),
        deleted: RwLock::new(false),
        storage: std::sync::Mutex::new(Some(storage)),
    }))
}

async fn recover_copy(
    dir: &Path,
    fleet: &Fleet,
    tenants: &Tenants,
) -> io::Result<Option<Snapshot>> {
    if !tokio::fs::try_exists(dir.join(COPY_RECORD)).await? {
        return Ok(None);
    }
    let copy: CopyRecord = files::read_record(dir, COPY_RECORD).await?;
    if copy.store != fleet.store.url() {
        return Ok(None);
    }
    Ok(Some(Snapshot {
        info: copy.info,
        tenant: tenants.owner(&copy.tenant),
        idempotency: None,
        dir: dir.to_owned(),
        deleted: RwLock::new(false),
        storage: std::sync::Mutex::new(None),
    }))
}

async fn drop_deleted_copies(store: &Store, copies: &mut HashMap<String, Arc<Snapshot>>) {
    let mut deleted = Vec::new();
    for id in copies.keys() {
        match store.snapshot(id).await {
            Ok(Some(_)) => {}
            Ok(None) => deleted.push(id.clone()),
            Err(error) => {
                tracing::warn!(snapshot_id = %id, %error, "kept a snapshot copy the store could not confirm");
            }
        }
    }
    for id in deleted {
        if let Some(copy) = copies.remove(&id)
            && let Err(error) = files::discard(&copy.dir, COPY_RECORD).await
        {
            tracing::error!(snapshot_id = %id, %error, "removing a deleted snapshot's copy failed");
        }
    }
}

async fn remove_unused_images(images_dir: &Path) -> io::Result<()> {
    let mut entries = tokio::fs::read_dir(images_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let partial = entry.file_name().to_string_lossy().contains(".partial-");
        let unused = std::os::unix::fs::MetadataExt::nlink(&entry.metadata().await?) == 1;
        if partial || unused {
            files::remove_if_present(tokio::fs::remove_file(entry.path()).await)?;
        }
    }
    files::sync(images_dir).await
}

async fn remove_old_files(dir: &Path, older_than: Duration) -> io::Result<()> {
    let cutoff = std::time::SystemTime::now() - older_than;
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.metadata().await?.modified()? < cutoff {
            files::remove_if_present(tokio::fs::remove_file(entry.path()).await)?;
        }
    }
    files::sync(dir).await
}

pub fn cpu_pool() -> io::Result<String> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo")?;
    Ok(pool_of(&cpuinfo, std::env::consts::ARCH))
}

fn pool_of(cpuinfo: &str, arch: &str) -> String {
    let first_cpu = cpuinfo.split("\n\n").next().unwrap_or_default();
    let field = |name: &str| {
        first_cpu.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().to_owned())
        })
    };
    let model = field("model name")
        .or_else(|| {
            Some(format!(
                "{}/{}",
                field("CPU implementer")?,
                field("CPU part")?
            ))
        })
        .unwrap_or_else(|| "unknown".into());
    let features = field("flags")
        .or_else(|| field("Features"))
        .unwrap_or_default();
    let features = blake3::hash(features.as_bytes()).to_hex();
    format!("{arch} {model} {}", &features[..12])
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

pub fn stored_mib(bytes: u64) -> u64 {
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

fn is_tenant_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
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
