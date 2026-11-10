use std::{
    collections::HashMap,
    io,
    ops::ControlFlow,
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};

use tokio::sync::{Mutex, Semaphore, watch};

use crate::{
    contract::{CreateMachine, MachineInfo},
    machine::{Activity, BootError, Image, Machine, MachineUse, kill_stale_firecracker},
};

const DEFAULT_VCPUS: u8 = 2;
const DEFAULT_MEMORY_MIB: u32 = 512;
const DEFAULT_IDLE_TIMEOUT_SECS: u32 = 600;
const MAX_VCPUS: u8 = 32;
const MEMORY_MIB_RANGE: std::ops::RangeInclusive<u32> = 128..=32768;
const IDLE_TIMEOUT_SECS_RANGE: std::ops::RangeInclusive<u32> = 1..=86400;

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
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    running: HashMap<String, Arc<Machine>>,
    shutting_down: bool,
}

impl Machines {
    pub async fn open(image_dir: PathBuf, state_dir: PathBuf, capacity: usize) -> io::Result<Self> {
        let image = Image::in_dir(&image_dir)?;
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
        Ok(Self {
            _state_lock: state_lock,
            image,
            machines_dir,
            slots: Arc::new(Semaphore::new(capacity)),
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
        let machines = self.clone();
        run_to_completion(async move { machines.boot(vcpus, memory_mib, idle_timeout_secs).await })
            .await
    }

    async fn boot(
        self: Arc<Self>,
        vcpus: u8,
        memory_mib: u32,
        idle_timeout_secs: u32,
    ) -> Result<Arc<Machine>, MachineError> {
        if self.state.lock().await.shutting_down {
            return Err(MachineError::ShuttingDown);
        }
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| MachineError::Capacity)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let machine = Machine::boot(
            id.clone(),
            self.machines_dir.join(&id),
            &self.image,
            vcpus,
            memory_mib,
            idle_timeout_secs,
            slot,
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
        for machine in &machines {
            machine.destroy().await;
        }
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

async fn remove_stale_machines(machines_dir: &std::path::Path) -> io::Result<()> {
    let mut entries = tokio::fs::read_dir(machines_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        kill_stale_firecracker(&entry.path());
        tokio::fs::remove_dir_all(entry.path()).await?;
    }
    Ok(())
}
