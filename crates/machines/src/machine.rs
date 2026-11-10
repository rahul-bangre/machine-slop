use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::{Child, Command},
    sync::{Mutex, OwnedSemaphorePermit, oneshot, watch},
};

use crate::contract::{MachineInfo, MachineState};

pub const AGENTD_PORT: u32 = 3000;
const SCRATCH_BYTES: u64 = 8 << 30;
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(1);
const CONSOLE_TAIL_BYTES: usize = 4096;

#[derive(Clone, Debug)]
pub struct Image {
    firecracker: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
}

impl Image {
    pub fn in_dir(dir: &Path) -> io::Result<Self> {
        let image = Self {
            firecracker: dir.join("firecracker"),
            kernel: dir.join("vmlinux"),
            rootfs: dir.join("rootfs.ext4"),
        };
        for path in [&image.firecracker, &image.kernel, &image.rootfs] {
            if !path.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("image file {} is missing", path.display()),
                ));
            }
        }
        Ok(image)
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
                    "machine exited during boot ({message}); console:\n{console}"
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

impl From<io::Error> for BootError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub struct Machine {
    id: String,
    dir: PathBuf,
    vcpus: u8,
    memory_mib: u32,
    boot_ms: u64,
    exit: watch::Receiver<Option<String>>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
    _slot: OwnedSemaphorePermit,
}

impl Machine {
    pub async fn boot(
        id: String,
        dir: PathBuf,
        image: &Image,
        vcpus: u8,
        memory_mib: u32,
        slot: OwnedSemaphorePermit,
    ) -> Result<Self, BootError> {
        let started = Instant::now();
        let child = match launch(&dir, image, vcpus, memory_mib).await {
            Ok(child) => child,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(error.into());
            }
        };
        let (exit, kill) = supervise(child);
        let mut machine = Self {
            id,
            dir,
            vcpus,
            memory_mib,
            boot_ms: 0,
            exit,
            kill: Mutex::new(Some(kill)),
            _slot: slot,
        };
        if let Err(error) = machine.wait_until_ready().await {
            machine.destroy().await;
            return Err(error);
        }
        machine.boot_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        Ok(machine)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn info(&self) -> MachineInfo {
        MachineInfo {
            id: self.id.clone(),
            state: match self.exit.borrow().as_ref() {
                None => MachineState::Running,
                Some(message) => MachineState::Exited {
                    message: message.clone(),
                },
            },
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            boot_ms: self.boot_ms,
        }
    }

    pub async fn connect(&self) -> io::Result<UnixStream> {
        connect_vsock(&self.dir.join("vsock.sock"), AGENTD_PORT).await
    }

    pub async fn destroy(&self) {
        if let Some(kill) = self.kill.lock().await.take() {
            let _ = kill.send(());
        }
        let mut exit = self.exit.clone();
        let _ = exit.wait_for(Option::is_some).await;
        if let Err(error) = tokio::fs::remove_dir_all(&self.dir).await
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("failed to remove {}: {error}", self.dir.display());
        }
    }

    async fn wait_until_ready(&self) -> Result<(), BootError> {
        let deadline = Instant::now() + BOOT_TIMEOUT;
        loop {
            let exited = self.exit.borrow().clone();
            if let Some(message) = exited {
                return Err(BootError::Exited {
                    message,
                    console: self.console_tail().await,
                });
            }
            if tokio::time::timeout(HEALTH_TIMEOUT, self.health())
                .await
                .is_ok_and(|healthy| healthy.unwrap_or(false))
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BootError::TimedOut {
                    console: self.console_tail().await,
                });
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn health(&self) -> io::Result<bool> {
        let mut stream = self.connect().await?;
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: agentd\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok(response.starts_with(b"HTTP/1.1 200"))
    }

    async fn console_tail(&self) -> String {
        let console = tokio::fs::read(self.dir.join("console.log"))
            .await
            .unwrap_or_default();
        let start = console.len().saturating_sub(CONSOLE_TAIL_BYTES);
        String::from_utf8_lossy(&console[start..]).into_owned()
    }
}

async fn launch(dir: &Path, image: &Image, vcpus: u8, memory_mib: u32) -> io::Result<Child> {
    tokio::fs::create_dir_all(dir).await?;
    create_scratch_disk(&dir.join("scratch.ext4")).await?;
    tokio::fs::write(
        dir.join("config.json"),
        firecracker_config(image, vcpus, memory_mib).to_string(),
    )
    .await?;
    let child = spawn_firecracker(image, dir)?;
    if let Some(pid) = child.id() {
        tokio::fs::write(dir.join("firecracker.pid"), pid.to_string()).await?;
    }
    Ok(child)
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
                "path_on_host": image.rootfs,
                "is_root_device": true,
                "is_read_only": true,
            },
            {
                "drive_id": "scratch",
                "path_on_host": "scratch.ext4",
                "is_root_device": false,
                "is_read_only": false,
            },
        ],
        "machine-config": { "vcpu_count": vcpus, "mem_size_mib": memory_mib },
        "vsock": { "guest_cid": 3, "uds_path": "vsock.sock" },
    })
}

fn spawn_firecracker(image: &Image, dir: &Path) -> io::Result<Child> {
    let console = std::fs::File::create(dir.join("console.log"))?;
    Command::new(&image.firecracker)
        .args(["--api-sock", "api.sock", "--config-file", "config.json"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(console.try_clone()?)
        .stderr(console)
        .kill_on_drop(true)
        .spawn()
}

fn supervise(mut child: Child) -> (watch::Receiver<Option<String>>, oneshot::Sender<()>) {
    let (exit_sender, exit) = watch::channel(None);
    let (kill, kill_requested) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let status = tokio::select! {
            status = child.wait() => status,
            _ = kill_requested => {
                let _ = child.start_kill();
                child.wait().await
            }
        };
        let message = match status {
            Ok(status) => status.to_string(),
            Err(error) => format!("failed to wait for firecracker: {error}"),
        };
        exit_sender.send_replace(Some(message));
    });
    (exit, kill)
}

pub async fn connect_vsock(path: &Path, port: u32) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(path).await?;
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await?;
    let mut reply = Vec::new();
    loop {
        let byte = stream.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        reply.push(byte);
        if reply.len() > 64 {
            return Err(io::Error::other("vsock handshake reply too long"));
        }
    }
    if reply.starts_with(b"OK ") {
        Ok(stream)
    } else {
        Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!(
                "vsock port {port} refused: {}",
                String::from_utf8_lossy(&reply)
            ),
        ))
    }
}

pub fn kill_stale_firecracker(dir: &Path) {
    let Ok(pid) = std::fs::read_to_string(dir.join("firecracker.pid")) else {
        return;
    };
    let Some(pid) = pid
        .trim()
        .parse()
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    let running_in_dir = std::fs::read_link(format!("/proc/{}/cwd", pid.as_raw_nonzero()))
        .is_ok_and(|cwd| cwd == dir);
    if running_in_dir {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
}
