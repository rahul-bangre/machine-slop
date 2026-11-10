use std::{
    io,
    net::IpAddr,
    ops::Deref,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
    sync::watch,
};

use crate::{
    contract::{EgressCredential, MachineInfo, MachineState},
    egress::{Egress, EgressPolicy, MachineEgress},
    files,
    firecracker::Vm,
    gateway::Gateway,
    machines::{Admission, MachineError},
};

const SCRATCH_BYTES: u64 = 8 << 30;
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(1);
const CONSOLE_TAIL_BYTES: usize = 4096;

const RECORD: &str = "machine.json";
const CHECKPOINT: &str = "checkpoint";
const CHECKPOINT_STAGING: &str = "checkpoint.tmp";
const CHECKPOINT_WITHDRAWN: &str = "checkpoint.withdrawn";
const RESTORED: &str = "restored";
const FIRECRACKER: &str = "firecracker";
const ROOTFS: &str = "rootfs.ext4";
const SAVED_STATE: [&str; 2] = ["vmstate", "memory"];
const SCRATCH: &str = "scratch.ext4";

#[derive(Clone, Debug)]
pub struct Image {
    firecracker: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
}

impl Image {
    pub fn in_dir(dir: &Path) -> io::Result<Self> {
        let file = |name: &str| {
            let path = dir.join(name);
            match std::fs::canonicalize(&path) {
                Ok(file) if file.is_file() => Ok(file),
                _ => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("image file {} is missing", path.display()),
                )),
            }
        };
        Ok(Self {
            firecracker: file("firecracker")?,
            kernel: file("vmlinux")?,
            rootfs: file("rootfs.ext4")?,
        })
    }
}

#[derive(Debug)]
pub enum BootError {
    Io(io::Error),
    Exited { message: String, console: String },
    TimedOut { console: String },
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Exited { message, console } => {
                write!(
                    f,
                    "machine exited before agentd was ready ({message}); console:\n{console}"
                )
            }
            Self::TimedOut { console } => write!(
                f,
                "agentd was not ready within {}s; console:\n{console}",
                BOOT_TIMEOUT.as_secs()
            ),
        }
    }
}

impl std::error::Error for BootError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for BootError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub enum Disk {
    Copy,
    Take,
}

pub(crate) struct MachineSpec {
    pub vcpus: u8,
    pub memory_mib: u32,
    pub idle_timeout_secs: u32,
    pub egress: EgressPolicy,
}

pub struct Booted {
    id: String,
    dir: PathBuf,
    vm: Vm,
    vcpus: u8,
    memory_mib: u32,
    egress: Egress,
}

impl Booted {
    pub async fn from_image(
        id: String,
        dir: PathBuf,
        image: &Image,
        vcpus: u8,
        memory_mib: u32,
        egress: Egress,
        admission: Admission,
    ) -> Result<Self, BootError> {
        let boot = async {
            tokio::fs::create_dir(&dir).await?;
            files::link_immutable(&image.firecracker, &dir.join(FIRECRACKER)).await?;
            files::link_immutable(&image.rootfs, &dir.join(ROOTFS)).await?;
            create_scratch_disk(&dir.join(SCRATCH)).await?;
            let config = firecracker_config(image, vcpus, memory_mib).to_string();
            tokio::fs::write(dir.join("config.json"), config).await?;
            Vm::boot(&dir, admission, egress.route())
        };
        let vm = boot.await;
        Self::once_ready(id, dir, vm, vcpus, memory_mib, egress).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn from_saved(
        id: String,
        dir: PathBuf,
        saved: &Path,
        disk: Disk,
        vcpus: u8,
        memory_mib: u32,
        egress: Egress,
        admission: Admission,
    ) -> Result<Self, BootError> {
        let restore = async {
            tokio::fs::create_dir(&dir).await?;
            for name in [FIRECRACKER, ROOTFS] {
                files::link_immutable(&saved.join(name), &dir.join(name)).await?;
            }
            tokio::fs::create_dir(dir.join(RESTORED)).await?;
            for name in SAVED_STATE {
                files::link_immutable(&saved.join(name), &dir.join(RESTORED).join(name)).await?;
            }
            match disk {
                Disk::Copy => files::copy_disk(&saved.join(SCRATCH), &dir.join(SCRATCH)).await?,
                Disk::Take => tokio::fs::rename(saved.join(SCRATCH), dir.join(SCRATCH)).await?,
            }
            let vm = restore(&dir, RESTORED, admission, egress.route()).await?;
            if let Err(error) = vm.resume().await {
                vm.kill().await;
                return Err(error);
            }
            Ok(vm)
        };
        let vm = restore.await;
        Self::once_ready(id, dir, vm, vcpus, memory_mib, egress).await
    }

    async fn once_ready(
        id: String,
        dir: PathBuf,
        vm: io::Result<Vm>,
        vcpus: u8,
        memory_mib: u32,
        egress: Egress,
    ) -> Result<Self, BootError> {
        let ready = match vm {
            Ok(vm) => match wait_until_ready(&vm, &dir).await {
                Ok(()) => Ok(vm),
                Err(error) => {
                    vm.kill().await;
                    Err(error)
                }
            },
            Err(error) => Err(error.into()),
        };
        match ready {
            Ok(vm) => Ok(Self {
                id,
                dir,
                vm,
                vcpus,
                memory_mib,
                egress,
            }),
            Err(error) => {
                files::remove_dir_logged(&dir).await;
                Err(error)
            }
        }
    }

    pub fn is_alive(&self) -> bool {
        self.vm.exited().is_none()
    }

    pub async fn record(
        self,
        request_started: Instant,
        idle_timeout_secs: u32,
        egress: EgressPolicy,
    ) -> Result<Machine, BootError> {
        self.egress.replace(egress);
        let machine = Machine {
            id: self.id,
            dir: self.dir,
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            boot_ms: request_started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            idle_timeout_secs,
            egress: self.egress,
            activity: watch::Sender::new(Activity::idle_from_now()),
            phase: std::sync::Mutex::new(Phase::Running(self.vm.clone())),
            lifecycle: tokio::sync::Mutex::default(),
        };
        if let Err(error) = files::commit(
            &machine.dir,
            RECORD,
            &machine.record(&machine.egress.policy()),
        )
        .await
        {
            self.vm.kill().await;
            if let Err(error) = files::discard(&machine.dir, RECORD).await {
                tracing::error!(machine_id = %machine.id, path = %machine.dir.display(), %error, "machine cleanup failed");
            }
            return Err(error.into());
        }
        Ok(machine)
    }

    pub async fn discard(self) {
        self.vm.kill().await;
        files::remove_dir_logged(&self.dir).await;
    }
}

#[derive(Deserialize, Serialize)]
struct Record {
    vcpus: u8,
    memory_mib: u32,
    boot_ms: u64,
    idle_timeout_secs: u32,
    allowed_hosts: Vec<String>,
    #[serde(default)]
    egress_credentials: Vec<EgressCredential>,
    #[serde(default)]
    egress_address: Option<IpAddr>,
}

enum Phase {
    Running(Vm),
    Paused,
    Exited(String),
    Destroyed,
}

#[derive(Clone, Copy)]
pub(crate) struct Activity {
    in_use: usize,
    idle_since: tokio::time::Instant,
}

impl Activity {
    fn idle_from_now() -> Self {
        Self {
            in_use: 0,
            idle_since: tokio::time::Instant::now(),
        }
    }

    pub(crate) fn idle_deadline(&self, idle_timeout: Duration) -> Option<tokio::time::Instant> {
        (self.in_use == 0).then(|| self.idle_since + idle_timeout)
    }
}

pub struct MachineUse(Arc<Machine>);

impl Deref for MachineUse {
    type Target = Machine;

    fn deref(&self) -> &Machine {
        &self.0
    }
}

impl Drop for MachineUse {
    fn drop(&mut self) {
        self.0.end_use();
    }
}

pub struct Machine {
    id: String,
    dir: PathBuf,
    vcpus: u8,
    memory_mib: u32,
    boot_ms: u64,
    idle_timeout_secs: u32,
    egress: Egress,
    activity: watch::Sender<Activity>,
    phase: std::sync::Mutex<Phase>,
    lifecycle: tokio::sync::Mutex<()>,
}

impl Machine {
    pub async fn recover(
        id: String,
        dir: PathBuf,
        gateway: Arc<Gateway>,
    ) -> io::Result<Option<Self>> {
        if !tokio::fs::try_exists(dir.join(RECORD)).await? {
            tokio::fs::remove_dir_all(&dir).await?;
            return Ok(None);
        }
        let record: Record = files::read_record(&dir, RECORD).await?;
        let egress = EgressPolicy::new(
            &record.allowed_hosts,
            &record.egress_credentials,
            record.egress_address,
        )
        .map_err(io::Error::other)?;
        for stale in [CHECKPOINT_STAGING, CHECKPOINT_WITHDRAWN, RESTORED] {
            files::remove_dir_if_present(&dir.join(stale)).await?;
        }
        let machine = Self {
            id,
            dir,
            vcpus: record.vcpus,
            memory_mib: record.memory_mib,
            boot_ms: record.boot_ms,
            idle_timeout_secs: record.idle_timeout_secs,
            egress: Egress::new(egress, gateway),
            activity: watch::Sender::new(Activity::idle_from_now()),
            phase: std::sync::Mutex::new(Phase::Destroyed),
            lifecycle: tokio::sync::Mutex::default(),
        };
        machine.set(
            if tokio::fs::try_exists(machine.dir.join(CHECKPOINT)).await? {
                Phase::Paused
            } else {
                Phase::Exited(
                    "machined stopped while the machine was running, so its memory was lost".into(),
                )
            },
        );
        Ok(Some(machine))
    }

    pub(crate) fn egress_source(&self) -> Option<IpAddr> {
        self.egress.policy().source()
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn info(&self) -> MachineInfo {
        let egress = self.egress.policy();
        MachineInfo {
            id: self.id.clone(),
            state: self.state(),
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            boot_ms: self.boot_ms,
            idle_timeout_secs: self.idle_timeout_secs,
            allowed_hosts: egress.allowed_hosts(),
            credential_hosts: egress.credential_hosts(),
            egress_address: egress.source().map(|address| address.to_string()),
        }
    }

    fn record(&self, egress: &EgressPolicy) -> Record {
        Record {
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            boot_ms: self.boot_ms,
            idle_timeout_secs: self.idle_timeout_secs,
            allowed_hosts: egress.allowed_hosts(),
            egress_credentials: egress.credentials(),
            egress_address: egress.source(),
        }
    }

    pub fn state(&self) -> MachineState {
        match &*self.phase() {
            Phase::Running(vm) => match vm.exited() {
                None => MachineState::Running,
                Some(message) => MachineState::Exited { message },
            },
            Phase::Paused => MachineState::Paused,
            Phase::Exited(message) => MachineState::Exited {
                message: message.clone(),
            },
            Phase::Destroyed => MachineState::Exited {
                message: "machine was deleted".into(),
            },
        }
    }

    pub(crate) fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs.into())
    }

    pub(crate) fn activity(&self) -> watch::Receiver<Activity> {
        self.activity.subscribe()
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.activity
            .borrow()
            .idle_deadline(self.idle_timeout())
            .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
    }

    pub(crate) fn begin_use(self: &Arc<Self>) -> MachineUse {
        self.activity.send_modify(|activity| activity.in_use += 1);
        MachineUse(self.clone())
    }

    fn end_use(&self) {
        self.activity.send_modify(|activity| {
            activity.in_use -= 1;
            if activity.in_use == 0 {
                activity.idle_since = tokio::time::Instant::now();
            }
        });
    }

    pub async fn connect(&self) -> io::Result<UnixStream> {
        let vm = match &*self.phase() {
            Phase::Running(vm) => vm.clone(),
            _ => return Err(io::Error::other("machine is not running")),
        };
        vm.connect().await
    }

    pub(crate) async fn exclusive(&self) -> Exclusive<'_> {
        Exclusive {
            _lifecycle: self.lifecycle.lock().await,
            machine: self,
        }
    }

    fn phase(&self) -> std::sync::MutexGuard<'_, Phase> {
        self.phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set(&self, phase: Phase) {
        let pauses = matches!(phase, Phase::Paused);
        let was_paused = matches!(std::mem::replace(&mut *self.phase(), phase), Phase::Paused);
        match (was_paused, pauses) {
            (false, true) => self.activity.send_modify(|activity| activity.in_use += 1),
            (true, false) => self.end_use(),
            _ => {}
        }
    }

    fn failed(&self, action: &str, error: impl std::fmt::Display) -> MachineError {
        MachineError::Failed {
            message: format!("{action} failed: {error}"),
            state: self.state(),
        }
    }
}

enum Live {
    Running(Vm),
    Paused,
}

pub(crate) struct Exclusive<'a> {
    machine: &'a Machine,
    _lifecycle: tokio::sync::MutexGuard<'a, ()>,
}

impl std::ops::Deref for Exclusive<'_> {
    type Target = Machine;

    fn deref(&self) -> &Machine {
        self.machine
    }
}

impl Exclusive<'_> {
    fn live(&self) -> Result<Live, MachineError> {
        match &*self.phase() {
            Phase::Running(vm) if vm.exited().is_none() => Ok(Live::Running(vm.clone())),
            Phase::Paused => Ok(Live::Paused),
            Phase::Running(_) | Phase::Exited(_) => {
                Err(MachineError::Conflict("machine has exited".into()))
            }
            Phase::Destroyed => Err(MachineError::UnknownMachine),
        }
    }

    pub async fn pause(&self) -> Result<(), MachineError> {
        let Live::Running(vm) = self.live()? else {
            return Ok(());
        };
        if let Err(error) = vm.pause().await {
            return Err(self.failed("pause", error));
        }
        if let Err(error) = self.checkpoint(&vm).await {
            let withdrawn = self.withdraw_checkpoint().await;
            self.carry_on(&vm, withdrawn).await;
            for unused in [CHECKPOINT_STAGING, CHECKPOINT_WITHDRAWN] {
                files::remove_dir_logged(&self.dir.join(unused)).await;
            }
            return Err(self.failed("pause", error));
        }
        self.set(Phase::Paused);
        vm.kill().await;
        files::remove_dir_logged(&self.dir.join(RESTORED)).await;
        Ok(())
    }

    async fn withdraw_checkpoint(&self) -> io::Result<()> {
        let checkpoint = self.dir.join(CHECKPOINT);
        if tokio::fs::try_exists(&checkpoint).await? {
            let withdrawn = self.dir.join(CHECKPOINT_WITHDRAWN);
            files::remove_dir_if_present(&withdrawn).await?;
            tokio::fs::rename(&checkpoint, &withdrawn).await?;
        }
        files::sync(&self.dir).await
    }

    async fn checkpoint(&self, vm: &Vm) -> io::Result<()> {
        let staging = self.dir.join(CHECKPOINT_STAGING);
        files::remove_dir_if_present(&staging).await?;
        tokio::fs::create_dir(&staging).await?;
        vm.save(&staging).await?;
        files::sync_contents(&self.dir).await?;
        files::publish_dir(&staging, &self.dir.join(CHECKPOINT)).await
    }

    async fn carry_on(&self, vm: &Vm, safe: io::Result<()>) {
        let continued = match safe {
            Ok(()) => vm.resume().await,
            Err(error) => Err(error),
        };
        if let Err(error) = continued {
            vm.kill().await;
            let still_published = tokio::fs::try_exists(self.dir.join(CHECKPOINT)).await;
            self.set(if matches!(still_published, Ok(true)) {
                Phase::Paused
            } else {
                Phase::Exited(format!(
                    "the VM could not continue after being paused for a save: {error}"
                ))
            });
        }
    }

    pub fn is_paused(&self) -> Result<bool, MachineError> {
        Ok(matches!(self.live()?, Live::Paused))
    }

    pub async fn resume(&self, admission: Admission) -> Result<(), MachineError> {
        let Live::Paused = self.live()? else {
            return Ok(());
        };
        let vm = match restore(&self.dir, CHECKPOINT, admission, self.egress.route()).await {
            Ok(vm) => vm,
            Err(error) => return Err(self.failed("resume", error)),
        };
        if let Err(error) = self.retire_checkpoint().await {
            vm.kill().await;
            let reinstated = async {
                let checkpoint = self.dir.join(CHECKPOINT);
                if !tokio::fs::try_exists(&checkpoint).await? {
                    tokio::fs::rename(self.dir.join(RESTORED), checkpoint).await?;
                }
                files::sync(&self.dir).await
            };
            if reinstated.await.is_err() {
                self.set(Phase::Exited(format!("resume failed: {error}")));
            }
            return Err(self.failed("resume", error));
        }
        let ready = async {
            vm.resume().await?;
            wait_until_ready(&vm, &self.dir).await
        };
        match ready.await {
            Ok(()) => {
                self.set(Phase::Running(vm));
                Ok(())
            }
            Err(error) => {
                vm.kill().await;
                self.set(Phase::Exited(format!("resume failed: {error}")));
                Err(self.failed("resume", error))
            }
        }
    }

    async fn retire_checkpoint(&self) -> io::Result<()> {
        files::remove_dir_if_present(&self.dir.join(RESTORED)).await?;
        tokio::fs::rename(self.dir.join(CHECKPOINT), self.dir.join(RESTORED)).await?;
        files::sync(&self.dir).await
    }

    pub async fn save_into(&self, dir: &Path) -> Result<(), MachineError> {
        let copy_disk_and_image = async {
            files::copy_disk(&self.dir.join(SCRATCH), &dir.join(SCRATCH)).await?;
            for name in [FIRECRACKER, ROOTFS] {
                files::link_immutable(&self.dir.join(name), &dir.join(name)).await?;
            }
            Ok(())
        };
        match self.live()? {
            Live::Paused => {
                let copy = async {
                    for name in SAVED_STATE {
                        let saved = self.dir.join(CHECKPOINT).join(name);
                        files::link_immutable(&saved, &dir.join(name)).await?;
                    }
                    copy_disk_and_image.await
                };
                copy.await.map_err(MachineError::Io)
            }
            Live::Running(vm) => {
                if let Err(error) = vm.pause().await {
                    return Err(self.failed("saving", error));
                }
                let saved = async {
                    vm.save(dir).await?;
                    copy_disk_and_image.await
                };
                let saved = saved.await;
                self.carry_on(&vm, Ok(())).await;
                match saved {
                    Ok(()) if self.state() == MachineState::Running => Ok(()),
                    Ok(()) => Err(self.failed("saving", "the VM could not continue")),
                    Err(error) => Err(self.failed("saving", error)),
                }
            }
        }
    }

    pub fn spec(&self) -> MachineSpec {
        MachineSpec {
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            idle_timeout_secs: self.idle_timeout_secs,
            egress: self.egress.policy(),
        }
    }

    pub async fn set_egress(&self, egress: EgressPolicy) -> Result<(), MachineError> {
        if matches!(&*self.phase(), Phase::Destroyed) {
            return Err(MachineError::UnknownMachine);
        }
        files::replace_record(&self.dir, RECORD, &self.record(&egress))
            .await
            .map_err(MachineError::Io)?;
        self.egress.replace(egress);
        Ok(())
    }

    pub async fn park(&self) {
        let Err(error) = self.pause().await else {
            return;
        };
        if let Ok(Live::Running(vm)) = self.live() {
            tracing::error!(machine_id = %self.id, %error, "machine was not saved at shutdown");
            vm.kill().await;
        }
    }

    pub async fn destroy(&self) -> io::Result<()> {
        let was_paused = matches!(&*self.phase(), Phase::Paused);
        let vm = match &*self.phase() {
            Phase::Running(vm) => Some(vm.clone()),
            _ => None,
        };
        self.set(Phase::Destroyed);
        if let Some(vm) = vm {
            vm.kill().await;
        }
        let discarded = files::discard(&self.dir, RECORD).await;
        if let Err(error) = &discarded {
            let still_recorded = tokio::fs::try_exists(self.dir.join(RECORD)).await;
            self.set(if was_paused && matches!(still_recorded, Ok(true)) {
                Phase::Paused
            } else {
                Phase::Exited(format!("deleting the machine failed: {error}"))
            });
        }
        discarded
    }
}

async fn restore(
    dir: &Path,
    saved: &str,
    admission: Admission,
    egress: MachineEgress,
) -> io::Result<Vm> {
    let vm = Vm::unconfigured(dir, admission, egress)?;
    if let Err(error) = vm.load(saved).await {
        vm.kill().await;
        return Err(error);
    }
    Ok(vm)
}

async fn wait_until_ready(vm: &Vm, dir: &Path) -> Result<(), BootError> {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if let Some(message) = vm.exited() {
            return Err(BootError::Exited {
                message,
                console: console_tail(dir).await,
            });
        }
        if tokio::time::timeout(HEALTH_TIMEOUT, health(vm))
            .await
            .is_ok_and(|healthy| healthy.unwrap_or(false))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BootError::TimedOut {
                console: console_tail(dir).await,
            });
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn health(vm: &Vm) -> io::Result<bool> {
    let mut stream = vm.connect().await?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: agentd\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    Ok(response.starts_with(b"HTTP/1.1 200"))
}

async fn console_tail(dir: &Path) -> String {
    let console = tokio::fs::read(dir.join("console.log"))
        .await
        .unwrap_or_default();
    let start = console.len().saturating_sub(CONSOLE_TAIL_BYTES);
    String::from_utf8_lossy(&console[start..]).into_owned()
}

async fn create_scratch_disk(path: &Path) -> io::Result<()> {
    tokio::fs::File::create(path)
        .await?
        .set_len(SCRATCH_BYTES)
        .await?;
    let output = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-E", "lazy_itable_init=1,lazy_journal_init=1"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "mkfs.ext4 failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn firecracker_config(image: &Image, vcpus: u8, memory_mib: u32) -> serde_json::Value {
    json!({
        "boot-source": {
            "kernel_image_path": image.kernel,
            "boot_args": "console=ttyS0 reboot=k panic=1 quiet loglevel=1 ro init=/sbin/slop-init",
        },
        "drives": [
            {
                "drive_id": "rootfs",
                "path_on_host": ROOTFS,
                "is_root_device": true,
                "is_read_only": true,
            },
            {
                "drive_id": "scratch",
                "path_on_host": SCRATCH,
                "is_root_device": false,
                "is_read_only": false,
            },
        ],
        "machine-config": { "vcpu_count": vcpus, "mem_size_mib": memory_mib },
        "vsock": { "guest_cid": 3, "uds_path": "vsock.sock" },
    })
}
