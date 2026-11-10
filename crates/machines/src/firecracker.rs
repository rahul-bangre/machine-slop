use std::{
    fs::Permissions,
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::{Notify, watch},
};

use crate::{
    egress::{self, DNS_PORT, EGRESS_PORT, MachineEgress},
    files::{self, link_immutable},
    machines::{Admission, MACHINE_OVERHEAD_MIB, lock},
    tenants::TenantCharge,
};

pub const AGENTD_PORT: u32 = 3000;
pub(crate) const FIRECRACKER: &str = "firecracker";
pub(crate) const ROOTFS: &str = "rootfs.ext4";
pub(crate) const SCRATCH: &str = "scratch.ext4";
pub(crate) const SAVED_STATE: [&str; 2] = ["vmstate", "memory"];
const KERNEL: &str = "vmlinux";
const RESTORED: &str = "restored";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const VM_THREADS_BEYOND_VCPUS: u32 = 32;
const API_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const API_TIMEOUT: Duration = Duration::from_secs(600);
const CGROUP_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Jail {
    jailer: PathBuf,
    jails_dir: PathBuf,
    cgroup: String,
}

impl Jail {
    pub(crate) fn new(jailer: PathBuf, state_dir: &Path) -> io::Result<Self> {
        let state = std::fs::metadata(state_dir)?;
        Ok(Self {
            jailer,
            jails_dir: state_dir.join("jails"),
            cgroup: format!("machine-slop-{}-{}", state.dev(), state.ino()),
        })
    }

    pub(crate) fn cgroup(&self) -> PathBuf {
        Path::new(CGROUP_ROOT).join(&self.cgroup)
    }

    fn seat_dir(&self, uid: u32) -> PathBuf {
        self.jails_dir.join(FIRECRACKER).join(uid.to_string())
    }

    pub(crate) fn kill_orphans(&self) -> io::Result<()> {
        let entries = match std::fs::read_dir(self.cgroup()) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            entries => Some(entries?),
        };
        for entry in entries.into_iter().flatten() {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                std::fs::write(entry.path().join("cgroup.kill"), "1")?;
                remove_cgroup(&entry.path())?;
            }
        }
        files::remove_if_present(std::fs::remove_dir_all(&self.jails_dir))?;
        std::fs::create_dir(&self.jails_dir)?;
        std::fs::set_permissions(&self.jails_dir, Permissions::from_mode(0o700))
    }

    pub(crate) fn release_cgroup(&self) {
        if let Err(error) = files::remove_if_present(std::fs::remove_dir(self.cgroup())) {
            tracing::warn!(cgroup = %self.cgroup().display(), %error, "machine cgroup was not removed");
        }
    }
}

fn remove_cgroup(cgroup: &Path) -> io::Result<()> {
    let deadline = std::time::Instant::now() + CGROUP_DRAIN_TIMEOUT;
    loop {
        match std::fs::remove_dir(cgroup) {
            Err(error) if error.kind() == io::ErrorKind::ResourceBusy => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::other(format!(
                        "processes in {} survived SIGKILL",
                        cgroup.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            result => return files::remove_if_present(result),
        }
    }
}

pub(crate) struct Launch<'a> {
    pub dir: &'a Path,
    pub jail: &'a Jail,
    pub admission: Admission,
    pub egress: MachineEgress,
    pub vcpus: u8,
    pub memory_mib: u32,
}

#[derive(Clone)]
pub struct Vm {
    root: PathBuf,
    pid: u32,
    admission: Arc<std::sync::Mutex<Option<Admission>>>,
    exit: watch::Receiver<Option<String>>,
    kill: Arc<Notify>,
}

impl Vm {
    pub(crate) async fn boot(launch: Launch<'_>, kernel: &Path, config: Value) -> io::Result<Self> {
        let root = launch.prepare_jail().await?;
        link_immutable(kernel, &root.join(KERNEL)).await?;
        tokio::fs::write(root.join("config.json"), config.to_string()).await?;
        let console = std::fs::File::create(launch.dir.join("console.log"))?;
        Self::spawn(launch, root, console, &["--config-file", "config.json"])
    }

    pub(crate) async fn restore(launch: Launch<'_>, saved: &Path) -> io::Result<Self> {
        let root = launch.prepare_jail().await?;
        tokio::fs::create_dir(root.join(RESTORED)).await?;
        for name in SAVED_STATE {
            link_immutable(&saved.join(name), &root.join(RESTORED).join(name)).await?;
        }
        let console = std::fs::File::options()
            .create(true)
            .append(true)
            .open(launch.dir.join("console.log"))?;
        let vm = Self::spawn(launch, root, console, &[])?;
        if let Err(error) = vm.load().await {
            vm.kill().await;
            return Err(error);
        }
        Ok(vm)
    }

    fn spawn(
        launch: Launch<'_>,
        root: PathBuf,
        console: std::fs::File,
        args: &[&str],
    ) -> io::Result<Self> {
        let Launch {
            dir,
            jail,
            admission,
            egress,
            vcpus,
            memory_mib,
        } = launch;
        let uid = admission.uid().to_string();
        let seat_dir = jail.seat_dir(admission.uid());
        let cgroup = jail.cgroup().join(&uid);
        remove_cgroup(&cgroup)?;
        let egress_socket = root.join(host_socket_name(EGRESS_PORT));
        let egress_listener = UnixListener::bind(&egress_socket)?;
        std::fs::set_permissions(&egress_socket, Permissions::from_mode(0o777))?;
        let dns_socket = root.join(host_socket_name(DNS_PORT));
        let dns_listener = UnixListener::bind(&dns_socket)?;
        std::fs::set_permissions(&dns_socket, Permissions::from_mode(0o777))?;
        let reserved_mib = u64::from(memory_mib) + u64::from(MACHINE_OVERHEAD_MIB);
        let memory_file_being_saved_mib = u64::from(memory_mib);
        let memory_max_bytes = (reserved_mib + memory_file_being_saved_mib) << 20;
        let mut child = Command::new(&jail.jailer)
            .args(["--id", &uid, "--exec-file"])
            .arg(dir.join(FIRECRACKER))
            .args(["--uid", &uid, "--gid", &uid, "--chroot-base-dir"])
            .arg(&jail.jails_dir)
            .args(["--cgroup-version", "2", "--parent-cgroup", &jail.cgroup])
            .args(["--cgroup", &format!("memory.max={memory_max_bytes}")])
            .args(["--cgroup", "memory.swap.max=0"])
            .args([
                "--cgroup",
                &format!("cpu.max={} 100000", u32::from(vcpus) * 100_000),
            ])
            .args([
                "--cgroup",
                &format!("pids.max={}", u32::from(vcpus) + VM_THREADS_BEYOND_VCPUS),
            ])
            .args(["--", "--api-sock", "api.sock"])
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(console.try_clone()?)
            .stderr(console)
            .kill_on_drop(true)
            .spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("firecracker exited before it could be tracked"))?;
        let vm_dir = dir.to_owned();
        let admission = Arc::new(std::sync::Mutex::new(Some(admission)));
        let held_until_exit = admission.clone();
        let (exit_sender, exit) = watch::channel(None);
        let kill = Arc::new(Notify::new());
        let kill_requested = kill.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                () = kill_requested.notified() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let message = match status {
                Ok(status) => status.to_string(),
                Err(error) => format!("failed to wait for firecracker: {error}"),
            };
            tracing::info!(path = %vm_dir.display(), %message, "firecracker exited");
            let dismantled = tokio::task::spawn_blocking(move || {
                remove_cgroup(&cgroup)?;
                files::remove_if_present(std::fs::remove_dir_all(seat_dir))
            })
            .await
            .unwrap_or_else(|error| Err(io::Error::other(error)));
            if let Err(error) = dismantled {
                tracing::error!(%error, "machine jail was not removed");
            }
            drop(lock(&held_until_exit).take());
            exit_sender.send_replace(Some(message));
        });
        let mut exited = exit.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = egress::serve_machine(egress_listener, dns_listener, egress) => {}
                _ = exited.wait_for(Option::is_some) => {}
            }
        });
        Ok(Self {
            root,
            pid,
            admission,
            exit,
            kill,
        })
    }

    pub(crate) fn start_metering(&self, machine_id: &str) {
        if let Some(admission) = &mut *lock(&self.admission) {
            admission.start_metering(machine_id);
        }
    }

    pub(crate) fn charge(&self, charge: TenantCharge) -> Result<(), TenantCharge> {
        match &mut *lock(&self.admission) {
            Some(admission) => {
                admission.charge(charge);
                Ok(())
            }
            None => Err(charge),
        }
    }

    pub fn exited(&self) -> Option<String> {
        self.exit.borrow().clone()
    }

    pub async fn kill(&self) {
        self.kill.notify_one();
        let mut exit = self.exit.clone();
        let _ = exit.wait_for(Option::is_some).await;
    }

    pub async fn pause(&self) -> io::Result<()> {
        self.api("PATCH", "/vm", json!({ "state": "Paused" })).await
    }

    pub async fn resume(&self) -> io::Result<()> {
        self.api("PATCH", "/vm", json!({ "state": "Resumed" }))
            .await
    }

    pub async fn save(&self, into: &Path) -> io::Result<()> {
        let saved = async {
            self.api(
                "PUT",
                "/snapshot/create",
                json!({
                    "snapshot_type": "Full",
                    "snapshot_path": SAVED_STATE[0],
                    "mem_file_path": SAVED_STATE[1],
                }),
            )
            .await?;
            for name in SAVED_STATE {
                self.take_saved(name, into)?;
            }
            Ok(())
        };
        let saved = saved.await;
        if saved.is_err() {
            for name in SAVED_STATE {
                files::remove_if_present(tokio::fs::remove_file(self.root.join(name)).await)?;
            }
        }
        saved
    }

    async fn load(&self) -> io::Result<()> {
        self.api(
            "PUT",
            "/snapshot/load",
            json!({
                "snapshot_path": format!("{RESTORED}/{}", SAVED_STATE[0]),
                "mem_backend": {
                    "backend_type": "File",
                    "backend_path": format!("{RESTORED}/{}", SAVED_STATE[1]),
                },
                "resume_vm": false,
            }),
        )
        .await
    }

    pub async fn connect(&self) -> io::Result<UnixStream> {
        vsock_handshake(self.connect_jailed("vsock.sock").await?, AGENTD_PORT).await
    }

    async fn connect_jailed(&self, socket: &str) -> io::Result<UnixStream> {
        let stream = UnixStream::connect(self.root.join(socket)).await?;
        let served_by = stream.peer_cred()?.pid();
        if served_by != i32::try_from(self.pid).ok() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{socket} in the jail is not served by this machine's VM"),
            ));
        }
        Ok(stream)
    }

    fn take_saved(&self, name: &str, into: &Path) -> io::Result<()> {
        use rustix::fs::{AtFlags, CWD, Mode, OFlags};
        let jailed = self.root.join(name);
        let file = rustix::fs::open(
            &jailed,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let saved = rustix::fs::fstat(&file)?;
        let vm_uid = std::fs::metadata(format!("/proc/{}", self.pid))?.uid();
        let is_file = rustix::fs::FileType::from_raw_mode(saved.st_mode).is_file();
        if !is_file || saved.st_uid != vm_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{name} in the jail is not a file the VM saved"),
            ));
        }
        rustix::fs::fchown(
            &file,
            Some(rustix::process::geteuid()),
            Some(rustix::process::getegid()),
        )?;
        rustix::fs::fchmod(&file, Mode::from_raw_mode(0o444))?;
        rustix::fs::linkat(
            CWD,
            format!("/proc/self/fd/{}", file.as_raw_fd()),
            CWD,
            into.join(name),
            AtFlags::SYMLINK_FOLLOW,
        )?;
        std::fs::remove_file(jailed)
    }

    async fn api(&self, method: &str, path: &str, body: Value) -> io::Result<()> {
        match tokio::time::timeout(API_TIMEOUT, self.request(method, path, body)).await {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("firecracker {method} {path} did not answer"),
            )),
        }
    }

    async fn request(&self, method: &str, path: &str, body: Value) -> io::Result<()> {
        let mut stream = BufReader::new(self.connect_api().await?);
        let body = body.to_string();
        stream
            .write_all(
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await?;
        let mut status_line = String::new();
        stream.read_line(&mut status_line).await?;
        let mut content_length = 0;
        loop {
            let mut header = String::new();
            if stream.read_line(&mut header).await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let Some((name, value)) = header.trim_end().split_once(':') else {
                break;
            };
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().map_err(io::Error::other)?;
            }
        }
        let mut response = vec![0; content_length];
        stream.read_exact(&mut response).await?;
        let status = status_line.split(' ').nth(1).unwrap_or_default();
        if status.starts_with('2') {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "firecracker {method} {path} returned {}: {}",
                status_line.trim_end(),
                String::from_utf8_lossy(&response)
            )))
        }
    }

    async fn connect_api(&self) -> io::Result<UnixStream> {
        let deadline = tokio::time::Instant::now() + API_SOCKET_TIMEOUT;
        loop {
            match self.connect_jailed("api.sock").await {
                Ok(stream) => return Ok(stream),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Err(error),
                Err(error) => {
                    if let Some(message) = self.exited() {
                        return Err(io::Error::other(format!("firecracker exited: {message}")));
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

fn host_socket_name(port: u32) -> String {
    format!("vsock.sock_{port}")
}

async fn vsock_handshake(mut stream: UnixStream, port: u32) -> io::Result<UnixStream> {
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

impl Launch<'_> {
    async fn prepare_jail(&self) -> io::Result<PathBuf> {
        let seat_dir = self.jail.seat_dir(self.admission.uid());
        files::remove_dir_if_present(&seat_dir).await?;
        let root = seat_dir.join("root");
        tokio::fs::create_dir_all(&root).await?;
        for disk in [ROOTFS, SCRATCH] {
            tokio::fs::hard_link(self.dir.join(disk), root.join(disk)).await?;
        }
        tokio::fs::set_permissions(root.join(SCRATCH), Permissions::from_mode(0o666)).await?;
        Ok(root)
    }
}
