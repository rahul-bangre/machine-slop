use std::{
    collections::HashMap,
    io,
    ops::ControlFlow,
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch};

use crate::{
    contract::{CreateMachine, MachineInfo, UpdateMachine},
    egress::{EgressPolicy, MAX_MACHINE_TUNNEL_FILES},
    machine::{
        Activity, BootError, Image, Machine, MachineSpec, MachineUse, kill_stale_firecracker,
    },
};

const DEFAULT_VCPUS: u8 = 2;
const DEFAULT_MEMORY_MIB: u32 = 512;
const DEFAULT_IDLE_TIMEOUT_SECS: u32 = 600;
const MAX_VCPUS: u8 = 32;
const MEMORY_MIB_RANGE: std::ops::RangeInclusive<u32> = 128..=32768;
const IDLE_TIMEOUT_SECS_RANGE: std::ops::RangeInclusive<u32> = 1..=86400;
const OPEN_FILES_OUTSIDE_EGRESS: u64 = 4096;
const MACHINE_OVERHEAD_MIB: u32 = 64;
const HOST_RESERVE_MIB: u32 = 512;

pub(crate) struct Admission {
    _slot: OwnedSemaphorePermit,
    _memory: OwnedSemaphorePermit,
}

impl std::error::Error for MachineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Boot(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum MachineError {
    Invalid(String),
    Capacity,
    ShuttingDown,
    Boot(BootError),
}

impl std::fmt::Display for MachineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "{message}"),
            Self::Capacity => write!(f, "machine capacity reached"),
            Self::ShuttingDown => write!(f, "host is shutting down"),
            Self::Boot(error) => write!(f, "{error}"),
        }
    }
}

pub struct Machines {
    _state_lock: std::fs::File,
    image: Image,
    machines_dir: PathBuf,
    slots: Arc<Semaphore>,
    memory: Arc<Semaphore>,
    memory_budget_mib: u32,
    capacity: u32,
    create_successes: AtomicU64,
    create_failures: AtomicU64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    running: HashMap<String, Arc<Machine>>,
    shutting_down: bool,
}

impl Machines {
    pub async fn open(image_dir: PathBuf, state_dir: PathBuf, capacity: usize) -> io::Result<Self> {
        let capacity = u32::try_from(capacity).map_err(io::Error::other)?;
        let image = Image::in_dir(&image_dir)?;
        ensure_open_file_limit(
            capacity as u64 * MAX_MACHINE_TUNNEL_FILES + OPEN_FILES_OUTSIDE_EGRESS,
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
        tokio::fs::create_dir_all(&machines_dir).await?;
        let machines_dir = tokio::fs::canonicalize(machines_dir).await?;
        remove_stale_machines(&machines_dir).await?;
        let available = available_memory_mib()?;
        let default_budget = available.saturating_sub(HOST_RESERVE_MIB.max(available / 10));
        let memory_budget_mib = match std::env::var("SLOP_MEMORY_MIB") {
            Ok(value) => value
                .parse::<u32>()
                .map_err(io::Error::other)?
                .min(default_budget),
            Err(std::env::VarError::NotPresent) => default_budget,
            Err(error) => return Err(io::Error::other(error)),
        };
        Ok(Self {
            _state_lock: state_lock,
            image,
            machines_dir,
            slots: Arc::new(Semaphore::new(capacity as usize)),
            memory: Arc::new(Semaphore::new(memory_budget_mib as usize)),
            memory_budget_mib,
            capacity,
            create_successes: AtomicU64::new(0),
            create_failures: AtomicU64::new(0),
            state: Mutex::default(),
        })
    }

    pub async fn create(
        self: &Arc<Self>,
        request: CreateMachine,
    ) -> Result<Arc<Machine>, MachineError> {
        let vcpus = request.vcpus.unwrap_or(DEFAULT_VCPUS);
        let memory_mib = request.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB);
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
        let spec = MachineSpec {
            vcpus,
            memory_mib,
            idle_timeout_secs,
            egress: EgressPolicy::parse(&request.allowed_hosts).map_err(MachineError::Invalid)?,
        };
        let machines = self.clone();
        run_to_completion(async move {
            match machines.clone().boot(spec).await {
                Ok(machine) => {
                    machines.create_successes.fetch_add(1, Ordering::Relaxed);
                    let info = machine.info();
                    tracing::info!(machine_id = %info.id, vcpus = info.vcpus, memory_mib = info.memory_mib, boot_ms = info.boot_ms, "machine created");
                    Ok(machine)
                }
                Err(error) => {
                    machines.create_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(%error, "machine create failed");
                    Err(error)
                }
            }
        }).await
    }

    async fn boot(self: Arc<Self>, spec: MachineSpec) -> Result<Arc<Machine>, MachineError> {
        let admission = {
            let state = self.state.lock().await;
            if state.shutting_down {
                return Err(MachineError::ShuttingDown);
            }
            let required = spec.memory_mib + MACHINE_OVERHEAD_MIB;
            if available_memory_mib()
                .map_err(|error| MachineError::Boot(BootError::Io(error)))?
                .saturating_sub(HOST_RESERVE_MIB)
                < required
            {
                return Err(MachineError::Capacity);
            }
            reserve(&self.slots, &self.memory, required)?
        };
        let id = uuid::Uuid::new_v4().simple().to_string();
        let machine = Machine::boot(
            id.clone(),
            self.machines_dir.join(&id),
            &self.image,
            spec,
            admission,
        )
        .await
        .map_err(MachineError::Boot)?;
        let mut state = self.state.lock().await;
        if state.shutting_down {
            drop(state);
            machine.destroy().await;
            return Err(MachineError::ShuttingDown);
        }
        let machine = Arc::new(machine);
        state.running.insert(id.clone(), machine.clone());
        drop(state);
        tokio::spawn(destroy_when_idle(
            Arc::downgrade(&self),
            id,
            machine.activity(),
            machine.idle_timeout(),
        ));
        Ok(machine)
    }

    pub fn create_counts(&self) -> (u64, u64) {
        (
            self.create_successes.load(Ordering::Relaxed),
            self.create_failures.load(Ordering::Relaxed),
        )
    }

    pub fn resource_usage(&self) -> (usize, u32, u32) {
        (
            self.capacity as usize - self.slots.available_permits(),
            self.memory_budget_mib - self.memory.available_permits() as u32,
            self.memory_budget_mib,
        )
    }

    pub async fn update(
        &self,
        id: &str,
        request: UpdateMachine,
    ) -> Result<Option<MachineInfo>, MachineError> {
        let egress = EgressPolicy::parse(&request.allowed_hosts).map_err(MachineError::Invalid)?;
        Ok(self.get(id).await.map(|machine| machine.set_egress(egress)))
    }

    pub async fn get(&self, id: &str) -> Option<Arc<Machine>> {
        self.state.lock().await.running.get(id).cloned()
    }

    pub async fn begin_use(&self, id: &str) -> Option<MachineUse> {
        self.state
            .lock()
            .await
            .running
            .get(id)
            .map(Machine::begin_use)
    }

    pub async fn list(&self) -> Vec<MachineInfo> {
        let mut infos: Vec<_> = self
            .state
            .lock()
            .await
            .running
            .values()
            .map(|machine| machine.info())
            .collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }

    async fn destroy_if_expired(&self, id: &str) -> ControlFlow<()> {
        let expired = {
            let mut state = self.state.lock().await;
            match state.running.get(id) {
                Some(machine) if !machine.is_expired() => return ControlFlow::Continue(()),
                Some(_) => state.running.remove(id),
                None => None,
            }
        };
        if let Some(machine) = expired {
            machine.destroy().await;
        }
        ControlFlow::Break(())
    }

    pub async fn destroy(&self, id: &str) -> bool {
        let Some(machine) = self.state.lock().await.running.remove(id) else {
            return false;
        };
        run_to_completion(async move { machine.destroy().await }).await;
        true
    }

    pub async fn shutdown(&self) {
        let machines: Vec<_> = {
            let mut state = self.state.lock().await;
            state.shutting_down = true;
            state.running.drain().map(|(_, machine)| machine).collect()
        };
        let cleanup: Vec<_> = machines
            .into_iter()
            .map(|machine| tokio::spawn(async move { machine.destroy().await }))
            .collect();
        for task in cleanup {
            task.await.expect("machine cleanup task panicked");
        }
        let _all_slots = self
            .slots
            .clone()
            .acquire_many_owned(self.capacity)
            .await
            .expect("machine admission stays open for shutdown accounting");
    }
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

async fn run_to_completion<T: Send + 'static>(work: impl Future<Output = T> + Send + 'static) -> T {
    match tokio::spawn(work).await {
        Ok(output) => output,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
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

async fn remove_stale_machines(machines_dir: &std::path::Path) -> io::Result<()> {
    let mut entries = tokio::fs::read_dir(machines_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        kill_stale_firecracker(&entry.path());
        tokio::fs::remove_dir_all(entry.path()).await?;
    }
    Ok(())
}

fn reserve(
    slots: &Arc<Semaphore>,
    memory: &Arc<Semaphore>,
    required: u32,
) -> Result<Admission, MachineError> {
    let slot = slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| MachineError::Capacity)?;
    let memory = memory
        .clone()
        .try_acquire_many_owned(required)
        .map_err(|_| MachineError::Capacity)?;
    Ok(Admission {
        _slot: slot,
        _memory: memory,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn host(dir: &std::path::Path) -> Arc<Machines> {
        for name in ["firecracker", "vmlinux", "rootfs.ext4"] {
            std::fs::write(dir.join(name), []).unwrap();
        }
        Arc::new(Machines {
            _state_lock: std::fs::File::create(dir.join("lock")).unwrap(),
            image: Image::in_dir(dir).unwrap(),
            machines_dir: dir.into(),
            slots: Arc::new(Semaphore::new(2)),
            memory: Arc::new(Semaphore::new(1024)),
            memory_budget_mib: 1024,
            capacity: 2,
            create_successes: AtomicU64::new(0),
            create_failures: AtomicU64::new(0),
            state: Mutex::default(),
        })
    }

    #[tokio::test]
    async fn shutdown_signals_all_vms_before_waiting_and_releases_retained_handles() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let mut retained = Vec::new();
        let mut exits = Vec::new();
        let mut kills = Vec::new();
        for id in ["first", "second"] {
            let machine_dir = dir.path().join(id);
            std::fs::create_dir(&machine_dir).unwrap();
            let (machine, killed, exit) = crate::machine::controlled_machine(
                id,
                machine_dir,
                reserve(&host.slots, &host.memory, 512).unwrap(),
            );
            host.state
                .lock()
                .await
                .running
                .insert(id.into(), machine.clone());
            retained.push(machine);
            exits.push(exit);
            kills.push(killed);
        }
        let shutdown_host = host.clone();
        let shutdown = tokio::spawn(async move { shutdown_host.shutdown().await });
        for killed in kills {
            tokio::time::timeout(Duration::from_secs(2), killed)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(!shutdown.is_finished());
        assert_eq!(host.resource_usage(), (2, 1024, 1024));
        for exit in exits {
            exit.send_replace(Some("killed".into()));
        }
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(host.resource_usage(), (0, 0, 1024));
        for machine in retained {
            assert!(!dir.path().join(machine.id()).exists());
            machine.destroy().await;
        }
        assert!(matches!(
            host.create(serde_json::from_str("{}").unwrap()).await,
            Err(MachineError::ShuttingDown)
        ));
    }

    #[tokio::test]
    async fn cancelling_shutdown_leaves_cleanup_running() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let machine_dir = dir.path().join("cancelled");
        std::fs::create_dir(&machine_dir).unwrap();
        let (retained, killed, exit) = crate::machine::controlled_machine(
            "cancelled",
            machine_dir.clone(),
            reserve(&host.slots, &host.memory, 512).unwrap(),
        );
        host.state
            .lock()
            .await
            .running
            .insert("cancelled".into(), retained.clone());
        let shutdown_host = host.clone();
        let shutdown = tokio::spawn(async move { shutdown_host.shutdown().await });
        tokio::time::timeout(Duration::from_secs(2), killed)
            .await
            .unwrap()
            .unwrap();
        shutdown.abort();
        assert!(shutdown.await.unwrap_err().is_cancelled());
        exit.send_replace(Some("killed".into()));
        tokio::time::timeout(Duration::from_secs(2), host.shutdown())
            .await
            .unwrap();
        assert!(!machine_dir.exists());
        assert_eq!(host.resource_usage(), (0, 0, 1024));
        retained.destroy().await;
    }

    #[tokio::test]
    async fn shutdown_waits_for_unpublished_admission_to_finish() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let admission = reserve(&host.slots, &host.memory, 512).unwrap();
        let shutdown_host = host.clone();
        let mut shutdown = tokio::spawn(async move { shutdown_host.shutdown().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut shutdown)
                .await
                .is_err()
        );
        drop(admission);
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(host.resource_usage(), (0, 0, 1024));
    }

    #[tokio::test]
    async fn failed_boot_cleans_files_and_releases_admission() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let machine_dir = dir.path().join("failed");
        let result = Machine::boot(
            "failed".into(),
            machine_dir.clone(),
            &host.image,
            MachineSpec {
                vcpus: 1,
                memory_mib: 128,
                idle_timeout_secs: 600,
                egress: EgressPolicy::parse(&[]).unwrap(),
            },
            reserve(&host.slots, &host.memory, 192).unwrap(),
        )
        .await;
        assert!(matches!(result, Err(BootError::Io(_))));
        assert!(!machine_dir.exists());
        assert_eq!(host.resource_usage(), (0, 0, 1024));
    }

    #[test]
    fn admission_reserves_memory_atomically_and_releases_failed_attempts() {
        let slots = Arc::new(Semaphore::new(2));
        let memory = Arc::new(Semaphore::new(1024));
        let first = reserve(&slots, &memory, 768).unwrap();
        assert!(matches!(
            reserve(&slots, &memory, 512),
            Err(MachineError::Capacity)
        ));
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(memory.available_permits(), 256);
        drop(first);
        let _second = reserve(&slots, &memory, 1024).unwrap();
        assert_eq!(memory.available_permits(), 0);
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
