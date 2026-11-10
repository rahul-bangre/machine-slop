use std::{
    io,
    os::unix::ffi::OsStrExt,
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
    egress::{self, DNS_PORT, EGRESS_PORT, EgressRoute},
    machines::Admission,
};

pub const AGENTD_PORT: u32 = 3000;
const API_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const API_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct Vm {
    dir: PathBuf,
    exit: watch::Receiver<Option<String>>,
    kill: Arc<Notify>,
}

impl Vm {
    pub fn boot(dir: &Path, admission: Admission, egress: EgressRoute) -> io::Result<Self> {
        let console = std::fs::File::create(dir.join("console.log"))?;
        Self::spawn(
            dir,
            console,
            &["--config-file", "config.json"],
            admission,
            egress,
        )
    }

    pub fn unconfigured(dir: &Path, admission: Admission, egress: EgressRoute) -> io::Result<Self> {
        for socket in [
            "api.sock",
            "vsock.sock",
            &host_socket_name(EGRESS_PORT),
            &host_socket_name(DNS_PORT),
        ] {
            crate::files::remove_if_present(std::fs::remove_file(dir.join(socket)))?;
        }
        let console = std::fs::File::options()
            .create(true)
            .append(true)
            .open(dir.join("console.log"))?;
        Self::spawn(dir, console, &[], admission, egress)
    }

    fn spawn(
        dir: &Path,
        console: std::fs::File,
        args: &[&str],
        admission: Admission,
        egress: EgressRoute,
    ) -> io::Result<Self> {
        let egress_listener = UnixListener::bind(dir.join(host_socket_name(EGRESS_PORT)))?;
        let dns_listener = UnixListener::bind(dir.join(host_socket_name(DNS_PORT)))?;
        let mut child = Command::new(dir.join("firecracker"))
            .args(["--api-sock", "api.sock"])
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(console.try_clone()?)
            .stderr(console)
            .kill_on_drop(true)
            .spawn()?;
        let vm_dir = dir.to_owned();
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
            drop(admission);
            exit_sender.send_replace(Some(message));
        });
        let mut exited = exit.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = egress::serve(egress_listener, dns_listener, egress.policy, egress.gateway) => {}
                _ = exited.wait_for(Option::is_some) => {}
            }
        });
        Ok(Self {
            dir: dir.to_owned(),
            exit,
            kill,
        })
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

    pub async fn save(&self, dir: &Path) -> io::Result<()> {
        self.api(
            "PUT",
            "/snapshot/create",
            json!({
                "snapshot_type": "Full",
                "snapshot_path": dir.join("vmstate"),
                "mem_file_path": dir.join("memory"),
            }),
        )
        .await
    }

    pub async fn load(&self, dir: &str) -> io::Result<()> {
        self.api(
            "PUT",
            "/snapshot/load",
            json!({
                "snapshot_path": format!("{dir}/vmstate"),
                "mem_backend": { "backend_type": "File", "backend_path": format!("{dir}/memory") },
                "resume_vm": false,
            }),
        )
        .await
    }

    pub async fn connect(&self) -> io::Result<UnixStream> {
        connect_vsock(&self.dir.join("vsock.sock"), AGENTD_PORT).await
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
            match UnixStream::connect(self.dir.join("api.sock")).await {
                Ok(stream) => return Ok(stream),
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

pub fn kill_orphans(machines_dir: &Path) -> io::Result<()> {
    let runs_under_machines_dir = |process: &Path| {
        std::fs::read_link(process.join("cwd")).is_ok_and(|cwd| cwd.starts_with(machines_dir))
    };
    let is_firecracker = |process: &Path| {
        std::fs::read(process.join("cmdline")).is_ok_and(|cmdline| {
            let program = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
            Path::new(std::ffi::OsStr::from_bytes(program)).file_name()
                == Some(std::ffi::OsStr::new("firecracker"))
        })
    };
    let mut orphans = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let pid = entry.file_name().to_str().and_then(|pid| pid.parse().ok());
        if let Some(pid) = pid.and_then(rustix::process::Pid::from_raw)
            && runs_under_machines_dir(&entry.path())
            && is_firecracker(&entry.path())
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            orphans.push(entry.path());
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while orphans.iter().any(|orphan| runs_under_machines_dir(orphan)) {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::other(
                "processes of an earlier machined survived SIGKILL",
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
