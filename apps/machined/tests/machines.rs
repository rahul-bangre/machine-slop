use std::{os::unix::fs::MetadataExt, path::PathBuf, process::Stdio, time::Duration};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
};

const API_KEY: &str = "test-key";
const OTHER_API_KEY: &str = "other-key";
const AUTHORIZED: &str = "Host: machined\r\nAuthorization: Bearer test-key\r\n";

struct Host {
    child: Child,
    address: String,
    state_dir: PathBuf,
    env: Vec<(&'static str, &'static str)>,
}

impl Host {
    async fn start(name: &str) -> Self {
        Self::start_with(name, &[]).await
    }

    async fn start_with(name: &str, env: &[(&'static str, &'static str)]) -> Self {
        let state_dir =
            std::env::temp_dir().join(format!("machined-test-{}-{name}", std::process::id()));
        let (child, address) = spawn_machined(&state_dir, env).await;
        Self {
            child,
            address,
            state_dir: std::fs::canonicalize(&state_dir).expect("state dir exists"),
            env: env.to_vec(),
        }
    }

    async fn restart(&mut self, signal: &str) {
        self.stop(signal).await;
        (self.child, self.address) = spawn_machined(&self.state_dir, &self.env).await;
    }

    async fn stop(&mut self, signal: &str) {
        let pid = self.child.id().expect("machined pid").to_string();
        Command::new("kill")
            .args([signal, &pid])
            .status()
            .await
            .expect("signal machined");
        let status = tokio::time::timeout(Duration::from_secs(60), self.child.wait())
            .await
            .expect("machined exits")
            .expect("wait for machined");
        assert_eq!(status.success(), signal == "-TERM");
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        self.send(method, path, &body).await
    }

    async fn send(&self, method: &str, path: &str, body: &str) -> (u16, Value) {
        self.send_with(AUTHORIZED, method, path, body).await
    }

    async fn request_as(
        &self,
        headers: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        self.send_with(&format!("Host: machined\r\n{headers}"), method, path, &body)
            .await
    }

    async fn send_with(&self, headers: &str, method: &str, path: &str, body: &str) -> (u16, Value) {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.send_unbounded(headers, method, path, body),
        )
        .await
        .expect("request completes within 60 seconds")
    }

    async fn send_unbounded(
        &self,
        headers: &str,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, Value) {
        let mut stream = TcpStream::connect(&self.address).await.expect("connect");
        let request = format!(
            "{method} {path} HTTP/1.1\r\n{headers}Connection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        let (head, body) = response.split_once("\r\n\r\n").expect("http response");
        let status = head[9..12].parse().expect("status code");
        (
            status,
            serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_owned())),
        )
    }

    async fn create(&self) -> String {
        self.create_with(None).await["id"]
            .as_str()
            .expect("machine id")
            .to_owned()
    }

    async fn create_with(&self, body: Option<Value>) -> Value {
        let (status, info) = self.request("POST", "/machines", body).await;
        assert_eq!(status, 201, "create machine: {info}");
        assert_eq!(info["state"], json!({ "state": "running" }));
        info
    }

    async fn start_process(&self, machine: &str, script: &str) -> String {
        let (status, process) = self
            .request(
                "POST",
                &format!("/machines/{machine}/processes"),
                Some(json!({ "program": "/bin/sh", "args": ["-c", script] })),
            )
            .await;
        assert_eq!(status, 201, "start process: {process}");
        process["id"].as_str().expect("process id").to_owned()
    }

    async fn wait_until_gone(&self, machine: &str, within: Duration) {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let (status, info) = self
                .request("GET", &format!("/machines/{machine}"), None)
                .await;
            if status == 404 {
                return;
            }
            assert_eq!(status, 200, "machine info: {info}");
            assert!(
                tokio::time::Instant::now() < deadline,
                "machine {machine} still listed after {within:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn exit_code(&self, machine: &str, script: &str) -> Value {
        let id = self.start_process(machine, script).await;
        let (status, result) = self
            .request(
                "GET",
                &format!("/machines/{machine}/processes/{id}/wait"),
                None,
            )
            .await;
        assert_eq!(status, 200, "wait: {result}");
        result["code"].clone()
    }

    async fn start_holding(&self, machine: &str, code: u8) -> String {
        let (status, process) = self
            .request(
                "POST",
                &format!("/machines/{machine}/processes"),
                Some(json!({
                    "program": "/bin/sh",
                    "args": ["-c", format!("code={code}; read line; exit $code")],
                    "keep_stdin_open": true,
                })),
            )
            .await;
        assert_eq!(status, 201, "start process: {process}");
        process["id"].as_str().expect("process id").to_owned()
    }

    async fn is_running(&self, machine: &str, process: &str) -> bool {
        let (status, info) = self
            .request(
                "GET",
                &format!("/machines/{machine}/processes/{process}"),
                None,
            )
            .await;
        assert_eq!(status, 200, "process info: {info}");
        info["status"]["state"] == "running"
    }

    async fn release(&self, machine: &str, process: &str) -> Value {
        let path = format!("/machines/{machine}/processes/{process}");
        let (status, body) = self
            .send("POST", &format!("{path}/stdin?offset=0"), "go\n")
            .await;
        assert_eq!(status, 204, "write stdin: {body}");
        let (status, result) = self.request("GET", &format!("{path}/wait"), None).await;
        assert_eq!(status, 200, "wait: {result}");
        result["code"].clone()
    }

    async fn transition(&self, machine: &str, to: &str) -> (u16, Value) {
        self.request("POST", &format!("/machines/{machine}/{to}"), None)
            .await
    }

    async fn pause(&self, machine: &str) {
        let (status, info) = self.transition(machine, "pause").await;
        assert_eq!(status, 200, "pause: {info}");
        assert_eq!(info["state"], json!({ "state": "paused" }));
    }

    async fn resume(&self, machine: &str) {
        let (status, info) = self.transition(machine, "resume").await;
        assert_eq!(status, 200, "resume: {info}");
        assert_eq!(info["state"], json!({ "state": "running" }));
    }

    async fn snapshot(&self, machine: &str) -> String {
        let (status, snapshot) = self
            .request("POST", "/snapshots", Some(json!({ "machine_id": machine })))
            .await;
        assert_eq!(status, 201, "snapshot: {snapshot}");
        assert_eq!(snapshot["machine_id"], json!(machine));
        snapshot["id"].as_str().expect("snapshot id").to_owned()
    }

    async fn create_from(&self, snapshot: &str) -> String {
        let (status, info) = self
            .request(
                "POST",
                "/machines",
                Some(json!({ "snapshot_id": snapshot })),
            )
            .await;
        assert_eq!(status, 201, "create from snapshot: {info}");
        assert_eq!(info["state"], json!({ "state": "running" }));
        info["id"].as_str().expect("machine id").to_owned()
    }

    async fn fork(&self, machine: &str) -> String {
        let (status, info) = self
            .request("POST", &format!("/machines/{machine}/fork"), None)
            .await;
        assert_eq!(status, 201, "fork: {info}");
        assert_eq!(info["state"], json!({ "state": "running" }));
        info["id"].as_str().expect("machine id").to_owned()
    }

    async fn state(&self, machine: &str) -> Value {
        let (status, info) = self
            .request("GET", &format!("/machines/{machine}"), None)
            .await;
        assert_eq!(status, 200, "machine info: {info}");
        info["state"]["state"].clone()
    }

    async fn delete(&self, path: &str) {
        let (status, body) = self.request("DELETE", path, None).await;
        assert_eq!(status, 204, "delete {path}: {body}");
    }

    fn machine_dir(&self, machine: &str) -> PathBuf {
        self.state_dir.join("machines").join(machine)
    }

    async fn exchange(&self, head: &str, body: &[u8]) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(60), async {
            let mut stream = TcpStream::connect(&self.address).await.expect("connect");
            stream.write_all(head.as_bytes()).await.expect("write head");
            stream.write_all(body).await.expect("write body");
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.expect("read");
            response
        })
        .await
        .expect("exchange completes within 60 seconds")
    }

    async fn tunnel(&self, machine: &str, port: u16) -> Option<TcpStream> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut stream = TcpStream::connect(&self.address).await.expect("connect");
            stream
                .write_all(
                    format!(
                        "GET /machines/{machine}/ports/{port} HTTP/1.1\r\n{AUTHORIZED}Connection: Upgrade\r\nUpgrade: tcp\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("write upgrade");
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(stream.read_u8().await.expect("read upgrade response"));
            }
            head.starts_with(b"HTTP/1.1 101 ").then_some(stream)
        })
        .await
        .expect("tunnel upgrade completes within 10 seconds")
    }

    fn cgroup(&self) -> PathBuf {
        let state = std::fs::metadata(&self.state_dir).expect("state dir");
        PathBuf::from(format!(
            "/sys/fs/cgroup/machine-slop-{}-{}",
            state.dev(),
            state.ino()
        ))
    }

    fn jail_of(&self, machine: &str) -> String {
        let scratch = std::fs::metadata(self.machine_dir(machine).join("scratch.ext4"))
            .expect("scratch disk")
            .ino();
        std::fs::read_dir(self.state_dir.join("jails/firecracker"))
            .expect("jails")
            .filter_map(Result::ok)
            .find(|jail| {
                std::fs::metadata(jail.path().join("root/scratch.ext4"))
                    .is_ok_and(|disk| disk.ino() == scratch)
            })
            .expect("the machine has a jail")
            .file_name()
            .into_string()
            .expect("jail id")
    }

    fn firecracker_pid(&self, machine: &str) -> String {
        let pids = vm_pids(&self.cgroup().join(self.jail_of(machine)));
        assert_eq!(pids.len(), 1, "machine {machine} has one VM");
        pids[0].clone()
    }

    fn all_vms(&self) -> usize {
        std::fs::read_dir(self.cgroup())
            .map(|jails| {
                jails
                    .filter_map(Result::ok)
                    .filter(|jail| jail.path().is_dir())
                    .map(|jail| vm_pids(&jail.path()).len())
                    .sum()
            })
            .unwrap_or(0)
    }

    async fn finish(mut self) {
        if self.child.try_wait().expect("machined status").is_none() {
            self.stop("-TERM").await;
        }
        std::fs::remove_dir_all(&self.state_dir).expect("remove state dir");
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let Some(pid) = self.child.id() else {
            return;
        };
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        let machined_stopped_its_vms = std::time::Instant::now() + Duration::from_secs(30);
        while matches!(self.child.try_wait(), Ok(None))
            && std::time::Instant::now() < machined_stopped_its_vms
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.start_kill();
        if let Ok(jails) = std::fs::read_dir(self.cgroup()) {
            for jail in jails.filter_map(Result::ok) {
                let _ = std::fs::write(jail.path().join("cgroup.kill"), "1");
            }
        }
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}

fn vm_pids(cgroup: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(cgroup.join("cgroup.procs"))
        .map(|pids| pids.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn digest(key: &str) -> String {
    Sha256::digest(key.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn spawn_machined(
    state_dir: &std::path::Path,
    env: &[(&'static str, &'static str)],
) -> (Child, String) {
    let image_dir = std::env::var("SLOP_IMAGE_DIR")
        .expect("SLOP_IMAGE_DIR names a directory built by vm/image/build.sh");
    let quotas = json!({
        "running_machines": 16,
        "memory_mib": 1 << 20,
        "disk_gib": 1 << 20,
        "creates_per_second": 1000,
    });
    let tenants = json!({ "tenants": [
        { "id": "tenant", "api_key_sha256": [digest(API_KEY)], "quotas": quotas },
        { "id": "other", "api_key_sha256": [digest(OTHER_API_KEY)], "quotas": quotas },
    ]});
    std::fs::create_dir_all(state_dir).expect("create state dir");
    let tenants_file = state_dir.join("tenants.json");
    std::fs::write(&tenants_file, tenants.to_string()).expect("write tenants file");
    let mut child = Command::new(env!("CARGO_BIN_EXE_machined"))
        .arg("127.0.0.1:0")
        .arg(image_dir)
        .arg(state_dir)
        .arg(tenants_file)
        .envs(env.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn machined");
    let mut lines = BufReader::new(child.stderr.take().expect("stderr")).lines();
    let address = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let line = lines
                .next_line()
                .await
                .expect("read machined output")
                .expect("machined reports its address");
            eprintln!("machined: {line}");
            let log: Value = serde_json::from_str(&line).expect("structured log");
            if let Some(address) = log["fields"]["address"].as_str() {
                break address
                    .strip_prefix("http://")
                    .expect("HTTP address")
                    .to_owned();
            }
        }
    })
    .await
    .expect("machined starts listening within 30 seconds");
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("machined: {line}");
        }
    });
    (child, address)
}

fn running(pid: &str) -> bool {
    std::fs::read_link(format!("/proc/{pid}/cwd")).is_ok()
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn machines_are_isolated_and_destroyed_with_their_vm() {
    let mut host = Host::start("lifecycle").await;
    let first = host.create().await;
    let second = host.create().await;
    let first_pid = host.firecracker_pid(&first);
    let second_pid = host.firecracker_pid(&second);

    assert_eq!(
        host.exit_code(&first, "echo written > /usr/local/marker")
            .await,
        json!(0)
    );
    assert_eq!(
        host.exit_code(&first, "test -f /usr/local/marker").await,
        json!(0)
    );
    assert_eq!(
        host.exit_code(&second, "test ! -e /usr/local/marker").await,
        json!(0)
    );
    assert_eq!(host.exit_code(&second, "exit 7").await, json!(7));

    let (status, _) = host
        .request("DELETE", &format!("/machines/{first}"), None)
        .await;
    assert_eq!(status, 204);
    assert!(
        !running(&first_pid),
        "first machine's firecracker exits on destroy"
    );
    assert!(!host.state_dir.join("machines").join(&first).exists());
    let (status, _) = host
        .request("GET", &format!("/machines/{first}"), None)
        .await;
    assert_eq!(status, 404);
    let (status, _) = host
        .request(
            "POST",
            &format!("/machines/{first}/processes"),
            Some(json!({ "program": "/bin/true" })),
        )
        .await;
    assert_eq!(status, 404);

    let (status, machines) = host.request("GET", "/machines", None).await;
    assert_eq!(status, 200);
    assert_eq!(machines.as_array().map(Vec::len), Some(1));
    assert_eq!(machines[0]["id"], json!(second));

    host.stop("-TERM").await;
    assert!(!running(&second_pid));
    assert_eq!(
        host.all_vms(),
        0,
        "no VM outlives a machined that shut down, spares included"
    );
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn invalid_requests_are_rejected_without_booting() {
    let host = Host::start("invalid").await;
    for body in [
        json!({ "vcpus": 0 }),
        json!({ "memory_mib": 64 }),
        json!({ "idle_timeout_secs": 0 }),
        json!({ "idle_timeout_secs": 86401 }),
        json!({ "disk": 1 }),
        json!({ "snapshot_id": "0", "vcpus": 1 }),
        json!({ "allowed_hosts": ["*.*.example.com"] }),
        json!({ "allowed_hosts": "example.com" }),
        json!({ "allowed_hosts": null }),
    ] {
        let (status, _) = host.request("POST", "/machines", Some(body.clone())).await;
        assert_eq!(status, 422, "{body}");
    }
    for (method, path, body) in [
        ("POST", "/machines", Some(json!({ "snapshot_id": "0" }))),
        ("POST", "/snapshots", Some(json!({ "machine_id": "0" }))),
        ("POST", "/machines/0/pause", None),
        ("POST", "/machines/0/resume", None),
        ("GET", "/snapshots/0", None),
        ("DELETE", "/snapshots/0", None),
    ] {
        let (status, _) = host.request(method, path, body).await;
        assert_eq!(status, 404, "{method} {path}");
    }
    let (status, _) = host.request("POST", "/snapshots", None).await;
    assert_eq!(status, 422);
    let (_, machines) = host.request("GET", "/machines", None).await;
    assert_eq!(machines, json!([]));
    let (_, snapshots) = host.request("GET", "/snapshots", None).await;
    assert_eq!(snapshots, json!([]));
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn a_create_abandoned_by_its_client_still_produces_a_listed_machine() {
    let host = Host::start("abandoned").await;
    let mut stream = TcpStream::connect(&host.address).await.expect("connect");
    stream
        .write_all(
            format!("POST /machines HTTP/1.1\r\n{AUTHORIZED}Content-Length: 0\r\n\r\n").as_bytes(),
        )
        .await
        .expect("write");
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(stream);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let machine = loop {
        let (_, machines) = host.request("GET", "/machines", None).await;
        if let Some(machine) = machines.as_array().and_then(|machines| machines.first()) {
            break machine.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "abandoned create never listed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(machine["state"], json!({ "state": "running" }));
    let id = machine["id"].as_str().expect("machine id");
    let (status, _) = host
        .request("DELETE", &format!("/machines/{id}"), None)
        .await;
    assert_eq!(status, 204);
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn a_paused_machine_resumes_with_its_processes_memory_and_disk() {
    let mut host = Host::start("pause").await;
    let machine = host.create().await;
    let holding = host.start_holding(&machine, 42).await;
    assert_eq!(
        host.exit_code(
            &machine,
            "echo in-memory > /dev/shm/marker && echo on-disk > /root/marker"
        )
        .await,
        json!(0)
    );
    let pid = host.firecracker_pid(&machine);

    host.pause(&machine).await;
    assert!(!running(&pid), "a paused machine's firecracker has exited");
    host.pause(&machine).await;
    let (status, body) = host
        .request("GET", &format!("/machines/{machine}/health"), None)
        .await;
    assert_eq!(status, 409, "a paused machine has no agent: {body}");

    host.restart("-TERM").await;
    assert_eq!(host.state(&machine).await, json!("paused"));

    host.resume(&machine).await;
    host.resume(&machine).await;
    assert!(
        host.is_running(&machine, &holding).await,
        "agentd still tracks the process it started before the pause"
    );
    assert_eq!(
        host.exit_code(
            &machine,
            "grep -qx in-memory /dev/shm/marker && grep -qx on-disk /root/marker"
        )
        .await,
        json!(0)
    );
    assert_eq!(host.release(&machine, &holding).await, json!(42));

    let holding = host.start_holding(&machine, 43).await;
    host.pause(&machine).await;
    host.resume(&machine).await;
    assert_eq!(host.release(&machine, &holding).await, json!(43));
    assert_eq!(
        host.exit_code(&machine, "grep -qx in-memory /dev/shm/marker")
            .await,
        json!(0)
    );
    host.delete(&format!("/machines/{machine}")).await;
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn concurrent_resumes_restore_one_vm() {
    let host = Host::start("resumes").await;
    let machine = host.create().await;
    let holding = host.start_holding(&machine, 9).await;
    host.pause(&machine).await;

    let (first, second, third, fourth) = tokio::join!(
        host.transition(&machine, "resume"),
        host.transition(&machine, "resume"),
        host.transition(&machine, "resume"),
        host.transition(&machine, "resume"),
    );
    for (status, info) in [first, second, third, fourth] {
        assert_eq!(status, 200, "resume: {info}");
        assert_eq!(info["state"], json!({ "state": "running" }));
    }
    host.firecracker_pid(&machine);
    assert_eq!(host.release(&machine, &holding).await, json!(9));
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn machines_from_a_snapshot_are_independent_of_it_its_source_and_each_other() {
    let mut host = Host::start("snapshot").await;
    let source = host.create().await;
    let holding = host.start_holding(&source, 7).await;
    assert_eq!(
        host.exit_code(
            &source,
            "echo in-memory > /dev/shm/marker && echo on-disk > /root/marker"
        )
        .await,
        json!(0)
    );
    let snapshot = host.snapshot(&source).await;
    assert_eq!(host.state(&source).await, json!("running"));
    assert_eq!(
        host.exit_code(
            &source,
            "echo source > /root/later && echo source > /dev/shm/later"
        )
        .await,
        json!(0)
    );
    let first = host.create_from(&snapshot).await;
    let second = host.create_from(&snapshot).await;
    let has_snapshot_state_only = "grep -qx in-memory /dev/shm/marker && grep -qx on-disk /root/marker \
        && test ! -e /root/later && test ! -e /dev/shm/later";
    for machine in [&first, &second] {
        assert_eq!(
            host.exit_code(machine, has_snapshot_state_only).await,
            json!(0),
            "a machine starts from the snapshot, not from its source's later writes"
        );
        assert!(host.is_running(machine, &holding).await);
    }
    assert_eq!(
        host.exit_code(
            &first,
            "echo first > /root/later && echo first > /dev/shm/later"
        )
        .await,
        json!(0)
    );
    assert_eq!(host.release(&first, &holding).await, json!(7));
    assert!(
        host.is_running(&second, &holding).await,
        "each machine has its own copy of the process"
    );
    assert!(host.is_running(&source, &holding).await);
    assert_eq!(
        host.exit_code(&source, "grep -qx source /root/later").await,
        json!(0)
    );

    host.delete(&format!("/machines/{source}")).await;
    host.restart("-TERM").await;
    let (_, snapshots) = host.request("GET", "/snapshots", None).await;
    assert_eq!(snapshots.as_array().map(Vec::len), Some(1));
    assert_eq!(snapshots[0]["id"], json!(snapshot));
    let (status, info) = host
        .request("GET", &format!("/snapshots/{snapshot}"), None)
        .await;
    assert_eq!((status, &info["machine_id"]), (200, &json!(source)));

    let third = host.create_from(&snapshot).await;
    assert_eq!(
        host.exit_code(&third, has_snapshot_state_only).await,
        json!(0),
        "writes by earlier machines never reach the snapshot"
    );

    host.delete(&format!("/snapshots/{snapshot}")).await;
    let (status, _) = host
        .request("GET", &format!("/snapshots/{snapshot}"), None)
        .await;
    assert_eq!(status, 404);
    let (status, _) = host
        .request(
            "POST",
            "/machines",
            Some(json!({ "snapshot_id": snapshot })),
        )
        .await;
    assert_eq!(status, 404);
    assert!(!host.state_dir.join("snapshots").join(&snapshot).exists());

    host.resume(&first).await;
    assert_eq!(
        host.exit_code(
            &first,
            "grep -qx first /root/later && grep -qx first /dev/shm/later"
        )
        .await,
        json!(0)
    );
    host.resume(&second).await;
    assert_eq!(host.release(&second, &holding).await, json!(7));
    host.pause(&third).await;
    host.resume(&third).await;
    assert_eq!(host.release(&third, &holding).await, json!(7));
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn a_snapshot_of_a_paused_machine_is_unchanged_by_the_machine_resuming() {
    let host = Host::start("paused-snapshot").await;
    let source = host.create().await;
    let holding = host.start_holding(&source, 3).await;
    assert_eq!(
        host.exit_code(
            &source,
            "echo before > /root/state && echo before > /dev/shm/state"
        )
        .await,
        json!(0)
    );
    host.pause(&source).await;
    let snapshot = host.snapshot(&source).await;
    assert_eq!(host.state(&source).await, json!("paused"));

    host.resume(&source).await;
    assert_eq!(
        host.exit_code(
            &source,
            "echo after > /root/state && echo after > /dev/shm/state"
        )
        .await,
        json!(0)
    );
    host.pause(&source).await;
    host.resume(&source).await;

    let restored = host.create_from(&snapshot).await;
    assert_eq!(
        host.exit_code(
            &restored,
            "grep -qx before /root/state && grep -qx before /dev/shm/state"
        )
        .await,
        json!(0),
        "the snapshot shares its memory file with its source, which only ever maps it privately"
    );
    assert_eq!(host.release(&restored, &holding).await, json!(3));
    assert_eq!(
        host.exit_code(
            &source,
            "grep -qx after /root/state && grep -qx after /dev/shm/state"
        )
        .await,
        json!(0)
    );
    assert_eq!(host.release(&source, &holding).await, json!(3));
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn a_killed_machined_loses_running_machines_and_keeps_paused_ones() {
    let mut host = Host::start("killed").await;
    let paused = host.create().await;
    let holding = host.start_holding(&paused, 5).await;
    host.pause(&paused).await;
    let resumed = host.create().await;
    host.pause(&resumed).await;
    host.resume(&resumed).await;
    let unsaved = host.create().await;
    let pids = [
        host.firecracker_pid(&resumed),
        host.firecracker_pid(&unsaved),
    ];

    host.restart("-KILL").await;
    for pid in &pids {
        assert!(!running(pid), "the next machined kills orphaned VMs");
    }
    for lost in [&resumed, &unsaved] {
        assert_eq!(host.state(lost).await, json!("exited"));
        let (status, body) = host.transition(lost, "resume").await;
        assert_eq!(
            status, 409,
            "memory that was not saved is not restored: {body}"
        );
    }
    assert_eq!(host.state(&paused).await, json!("paused"));
    host.resume(&paused).await;
    assert_eq!(host.release(&paused, &holding).await, json!(5));
    for machine in [&resumed, &unsaved, &paused] {
        host.delete(&format!("/machines/{machine}")).await;
    }
    let (_, machines) = host.request("GET", "/machines", None).await;
    assert_eq!(machines, json!([]));
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn a_fork_branches_a_running_machine_without_stopping_it() {
    let host = Host::start("fork").await;
    let source = host
        .create_with(Some(json!({ "memory_mib": SMALL_MACHINE_MIB })))
        .await["id"]
        .as_str()
        .expect("machine id")
        .to_owned();
    let holding = host.start_holding(&source, 11).await;
    assert_eq!(
        host.exit_code(
            &source,
            "echo in-memory > /dev/shm/marker && echo on-disk > /root/marker"
        )
        .await,
        json!(0)
    );
    let pid = host.firecracker_pid(&source);

    let branch = host.fork(&source).await;
    assert_ne!(branch, source);
    assert_eq!(host.state(&source).await, json!("running"));
    assert_eq!(
        host.firecracker_pid(&source),
        pid,
        "the source is the same VM as before the fork"
    );

    for (machine, own) in [(&source, "source"), (&branch, "branch")] {
        assert!(host.is_running(machine, &holding).await);
        let script = format!(
            "grep -qx in-memory /dev/shm/marker && grep -qx on-disk /root/marker \\
            && test ! -e /root/own && echo {own} > /root/own && echo {own} > /dev/shm/own"
        );
        assert_eq!(host.exit_code(machine, &script).await, json!(0));
    }
    assert_eq!(host.release(&branch, &holding).await, json!(11));
    assert!(
        host.is_running(&source, &holding).await,
        "each machine has its own copy of the process"
    );

    let second = host.fork(&branch).await;
    host.pause(&source).await;
    let third = host.fork(&source).await;
    assert_eq!(host.state(&source).await, json!("paused"));
    for (machine, own) in [(&second, "branch"), (&third, "source")] {
        let script = format!("grep -qx {own} /root/own && grep -qx {own} /dev/shm/own");
        assert_eq!(host.exit_code(machine, &script).await, json!(0));
    }
    assert_eq!(host.release(&third, &holding).await, json!(11));
    host.resume(&source).await;
    assert_eq!(host.release(&source, &holding).await, json!(11));
    assert!(
        std::fs::read_dir(host.state_dir.join("snapshots"))
            .expect("snapshots dir")
            .next()
            .is_none(),
        "a fork leaves no saved state behind"
    );
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn a_machine_unused_for_its_idle_timeout_is_destroyed() {
    let host = Host::start("idle").await;
    let requested = tokio::time::Instant::now();
    let (status, info) = host
        .request("POST", "/machines", Some(json!({ "idle_timeout_secs": 2 })))
        .await;
    assert_eq!(status, 201, "create machine: {info}");
    assert_eq!(info["idle_timeout_secs"], json!(2));
    let machine = info["id"].as_str().expect("machine id").to_owned();
    let pid = host.firecracker_pid(&machine);

    host.wait_until_gone(&machine, Duration::from_secs(10))
        .await;
    assert!(
        requested.elapsed() >= Duration::from_secs(2),
        "machine destroyed before its idle timeout"
    );
    let machine_dir = host.state_dir.join("machines").join(&machine);
    let destroyed_by = tokio::time::Instant::now() + Duration::from_secs(5);
    while running(&pid) || machine_dir.exists() {
        assert!(
            tokio::time::Instant::now() < destroyed_by,
            "an expired machine's firecracker exits and its files are removed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (status, _) = host
        .request("DELETE", &format!("/machines/{machine}"), None)
        .await;
    assert_eq!(status, 404);

    let (_, info) = host.request("POST", "/machines", None).await;
    assert_eq!(info["idle_timeout_secs"], json!(600));
    let id = info["id"].as_str().expect("machine id");
    let (status, _) = host
        .request("DELETE", &format!("/machines/{id}"), None)
        .await;
    assert_eq!(status, 204);
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn an_open_agent_request_keeps_a_machine_from_expiring() {
    let host = Host::start("in-use").await;
    let machine = host
        .create_with(Some(json!({ "idle_timeout_secs": 1 })))
        .await["id"]
        .as_str()
        .expect("machine id")
        .to_owned();
    let process = host.start_process(&machine, "sleep 600").await;

    let mut events = TcpStream::connect(&host.address).await.expect("connect");
    events
        .write_all(
            format!(
                "GET /machines/{machine}/processes/{process}/events HTTP/1.1\r\n{AUTHORIZED}\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("write");
    let mut head = [0; 12];
    events.read_exact(&mut head).await.expect("read status");
    assert_eq!(&head, b"HTTP/1.1 200");

    tokio::time::sleep(Duration::from_secs(3)).await;
    let (status, info) = host
        .request("GET", &format!("/machines/{machine}"), None)
        .await;
    assert_eq!(
        status, 200,
        "machine expired while a request was open: {info}"
    );

    drop(events);
    host.wait_until_gone(&machine, Duration::from_secs(10))
        .await;
    host.finish().await;
}

fn proxy_status_for(url: &str) -> String {
    format!("test \"$(curl -sS --noproxy '' -o /dev/null -w '%{{http_connect}}' {url})\" = 403")
}

#[tokio::test]
#[ignore = "needs /dev/kvm, SLOP_IMAGE_DIR and internet access; run with make vm-test"]
async fn egress_reaches_only_allowed_public_hosts() {
    let host = Host::start("egress").await;

    let allowed = host
        .create_with(Some(json!({
            "allowed_hosts": ["Example.com"],
            "memory_mib": SMALL_MACHINE_MIB,
        })))
        .await;
    assert_eq!(allowed["allowed_hosts"], json!(["example.com"]));
    let allowed = allowed["id"].as_str().expect("machine id");
    assert_eq!(
        host.exit_code(allowed, "curl -fsS -o /dev/null https://example.com/")
            .await,
        json!(0)
    );
    assert_eq!(
        host.exit_code(allowed, &proxy_status_for("https://example.org/"))
            .await,
        json!(0)
    );

    let offline = host
        .create_with(Some(
            json!({ "allowed_hosts": [], "memory_mib": SMALL_MACHINE_MIB }),
        ))
        .await;
    assert_eq!(offline["allowed_hosts"], json!([]));
    let offline = offline["id"].as_str().expect("machine id");
    assert_eq!(
        host.exit_code(offline, &proxy_status_for("https://example.com/"))
            .await,
        json!(0)
    );

    let unrestricted = host
        .create_with(Some(json!({ "memory_mib": SMALL_MACHINE_MIB })))
        .await;
    assert_eq!(unrestricted["allowed_hosts"], json!(["*"]));
    let unrestricted = unrestricted["id"].as_str().expect("machine id");
    assert_eq!(
        host.exit_code(unrestricted, &proxy_status_for("https://127.0.0.1/"))
            .await,
        json!(0)
    );
    assert_eq!(
        host.exit_code(unrestricted, "curl -fsS -o /dev/null https://example.org/")
            .await,
        json!(0)
    );

    host.pause(allowed).await;
    host.resume(allowed).await;
    let fork = host.fork(allowed).await;
    for restored in [allowed, &fork] {
        assert_eq!(
            host.exit_code(restored, "curl -fsS -o /dev/null https://example.com/")
                .await,
            json!(0),
            "a restored machine still reaches the hosts it is allowed"
        );
        assert_eq!(
            host.exit_code(restored, &proxy_status_for("https://example.org/"))
                .await,
            json!(0),
            "a restored machine is still refused the others"
        );
    }

    for machine in [allowed, offline, unrestricted, &fork] {
        let (status, _) = host
            .request("DELETE", &format!("/machines/{machine}"), None)
            .await;
        assert_eq!(status, 204);
    }
    host.finish().await;
}

const HOLD_TUNNEL_UNTIL_CLOSED: &str = r#"
import socket, sys
tunnel = socket.create_connection(("127.0.0.1", 3128))
tunnel.sendall(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
head = b""
while b"\r\n\r\n" not in head:
    head += tunnel.recv(1024)
if not head.startswith(b"HTTP/1.1 200"):
    sys.exit(2)
open("/tmp/tunnel-open", "w").close()
tunnel.settimeout(30)
sys.exit(0 if tunnel.recv(1) == b"" else 1)
"#;

#[tokio::test]
#[ignore = "needs /dev/kvm, SLOP_IMAGE_DIR and internet access; run with make vm-test"]
async fn allowed_hosts_change_while_a_machine_runs_and_revocation_closes_tunnels() {
    let host = Host::start("egress-update").await;
    let machine = host.create_with(Some(json!({ "allowed_hosts": [] }))).await;
    let machine = machine["id"].as_str().expect("machine id");
    let path = format!("/machines/{machine}");
    assert_eq!(
        host.exit_code(machine, &proxy_status_for("https://example.com/"))
            .await,
        json!(0)
    );

    let (status, info) = host
        .request(
            "PATCH",
            &path,
            Some(json!({ "allowed_hosts": ["example.com"] })),
        )
        .await;
    assert_eq!(status, 200, "{info}");
    assert_eq!(info["allowed_hosts"], json!(["example.com"]));
    assert_eq!(
        host.exit_code(machine, "curl -fsS -o /dev/null https://example.com/")
            .await,
        json!(0)
    );

    let (status, process) = host
        .request(
            "POST",
            &format!("{path}/processes"),
            Some(json!({ "program": "python3", "args": ["-c", HOLD_TUNNEL_UNTIL_CLOSED] })),
        )
        .await;
    assert_eq!(status, 201, "{process}");
    let process = format!("{path}/processes/{}", process["id"].as_str().expect("id"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while host.exit_code(machine, "test -f /tmp/tunnel-open").await != json!(0) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "tunnel never opened"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (_, running) = host.request("GET", &process, None).await;
    assert_eq!(running["status"]["state"], json!("running"), "{running}");

    let (status, info) = host
        .request("PATCH", &path, Some(json!({ "allowed_hosts": [] })))
        .await;
    assert_eq!(status, 200, "{info}");
    assert_eq!(info["allowed_hosts"], json!([]));
    let (_, result) = tokio::time::timeout(
        Duration::from_secs(5),
        host.request("GET", &format!("{process}/wait"), None),
    )
    .await
    .expect("revoking the host closes its open tunnel");
    assert_eq!(result["code"], json!(0), "{result}");
    assert_eq!(
        host.exit_code(machine, &proxy_status_for("https://example.com/"))
            .await,
        json!(0)
    );

    for (path, body, expected) in [
        (path.as_str(), json!({ "allowed_hosts": ["*.*"] }), 422),
        (path.as_str(), json!({}), 422),
        ("/machines/unknown", json!({ "allowed_hosts": [] }), 404),
    ] {
        let (status, response) = host.request("PATCH", path, Some(body.clone())).await;
        assert_eq!(status, expected, "{body}: {response}");
    }
    let (_, info) = host.request("GET", &path, None).await;
    assert_eq!(info["allowed_hosts"], json!([]));

    let (status, _) = host.request("DELETE", &path, None).await;
    assert_eq!(status, 204);
    host.finish().await;
}

fn body_of(response: &[u8]) -> &[u8] {
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("http response");
    &response[end + 4..]
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn files_and_ports_reach_the_machine_through_the_host() {
    let host = Host::start("files-ports").await;
    let machine = host.create().await;
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let put = host
        .exchange(
            &format!(
                "PUT /machines/{machine}/files/workspace/site/data.bin HTTP/1.1\r\n{AUTHORIZED}Connection: close\r\nContent-Length: {}\r\n\r\n",
                content.len()
            ),
            &content,
        )
        .await;
    assert!(
        put.starts_with(b"HTTP/1.1 204 "),
        "{}",
        String::from_utf8_lossy(&put)
    );
    let get = host
        .exchange(
            &format!(
                "GET /machines/{machine}/files/workspace/site/data.bin HTTP/1.1\r\n{AUTHORIZED}Connection: close\r\n\r\n"
            ),
            &[],
        )
        .await;
    assert_eq!(body_of(&get), content);

    let (status, _) = host
        .request(
            "POST",
            &format!("/machines/{machine}/processes"),
            Some(json!({
                "program": "python3",
                "args": ["-m", "http.server", "8000", "--bind", "127.0.0.1"],
                "cwd": "/workspace/site",
            })),
        )
        .await;
    assert_eq!(status, 201);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut tunnel = loop {
        if let Some(tunnel) = host.tunnel(&machine, 8000).await {
            break tunnel;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "web server in the machine never accepted a tunnel"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    tunnel
        .write_all(b"GET /data.bin HTTP/1.0\r\n\r\n")
        .await
        .expect("write through tunnel");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), tunnel.read_to_end(&mut response))
        .await
        .expect("the tunnel ends once the web server closes its connection")
        .expect("read through tunnel");
    assert!(response.starts_with(b"HTTP/1.0 200 "));
    assert_eq!(body_of(&response), content);

    let (status, _) = host
        .request("DELETE", &format!("/machines/{machine}"), None)
        .await;
    assert_eq!(status, 204);
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn an_open_port_tunnel_keeps_a_machine_from_expiring() {
    let host = Host::start("tunnel-in-use").await;
    let info = host
        .create_with(Some(json!({ "idle_timeout_secs": 1 })))
        .await;
    let machine = info["id"].as_str().expect("machine id");
    host.start_process(
        machine,
        "exec python3 -c 'import socket; s = socket.create_server((\"127.0.0.1\", 8200)); c, _ = s.accept(); c.recv(1)'",
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let tunnel = loop {
        if let Some(tunnel) = host.tunnel(machine, 8200).await {
            break tunnel;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "listener in the machine never accepted a tunnel"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    tokio::time::sleep(Duration::from_secs(3)).await;
    let (status, info) = host
        .request("GET", &format!("/machines/{machine}"), None)
        .await;
    assert_eq!(
        status, 200,
        "machine expired while a tunnel was open: {info}"
    );

    drop(tunnel);
    host.wait_until_gone(machine, Duration::from_secs(10)).await;
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn memory_admission_rejects_before_boot_and_recovers_after_destroy() {
    let host = Host::start_with(
        "memory",
        &[("SLOP_MEMORY_MIB", "700"), ("SLOP_SPARE_MACHINES", "0")],
    )
    .await;
    let first = host.create().await;
    let (status, _) = host.request("POST", "/machines", None).await;
    assert_eq!(status, 503);
    let machine_dirs = std::fs::read_dir(host.state_dir.join("machines"))
        .unwrap()
        .count();
    assert_eq!(machine_dirs, 1);
    let (status, metrics) = host.request("GET", "/metrics", None).await;
    assert_eq!(status, 200);
    let metrics = metrics.as_str().unwrap();
    assert!(metrics.contains("machined_memory_reserved_mib 576\n"));
    assert!(metrics.contains("machined_memory_budget_mib 700\n"));
    assert!(metrics.contains("machined_creates_total{result=\"failure\"} 1\n"));

    host.pause(&first).await;
    let (_, metrics) = host.request("GET", "/metrics", None).await;
    let metrics = metrics.as_str().unwrap();
    assert!(
        metrics.contains("machined_memory_reserved_mib 0\n"),
        "a paused machine reserves no memory: {metrics}"
    );
    assert!(metrics.contains("machined_machines{state=\"paused\"} 1\n"));
    let second = host.create().await;
    let (status, body) = host.transition(&first, "resume").await;
    assert_eq!(status, 503, "no memory to resume into: {body}");
    assert_eq!(host.state(&first).await, json!("paused"));

    host.delete(&format!("/machines/{second}")).await;
    host.resume(&first).await;
    host.delete(&format!("/machines/{first}")).await;
    let (_, metrics) = host.request("GET", "/metrics", None).await;
    let metrics = metrics.as_str().unwrap();
    assert!(metrics.contains("machined_machines_active 0\n"));
    assert!(metrics.contains("machined_memory_reserved_mib 0\n"));
    assert!(metrics.contains("machined_creates_total{result=\"success\"} 2\n"));
    host.finish().await;
}

const CHROMIUM_MACHINE_MIB: u32 = 1024;
const SMALL_MACHINE_MIB: u32 = 256;

const CHROMIUM_LOADS_A_PAGE_SERVED_IN_THE_MACHINE: &str = r#"
cd "$(mktemp -d)"
printf '%s' '<p id="status">static</p><script>document.getElementById("status").textContent = "script ran"</script>' > index.html
python3 -m http.server 8000 --bind 127.0.0.1 > /dev/null 2>&1 < /dev/null &
server=$!
tries=0
until curl -fsS -o /dev/null http://127.0.0.1:8000/; do
  tries=$((tries + 1))
  [ "$tries" -lt 100 ] || exit 2
  sleep 0.1
done
dom=$(timeout 90 chromium --headless=new --no-sandbox --dump-dom http://127.0.0.1:8000/ 2> /dev/null)
kill "$server"
case $dom in
  *'<p id="status">script ran</p>'*) exit 0 ;;
  *) exit 1 ;;
esac
"#;

const CHROMIUM_LOADS_ONLY_ALLOWED_HTTPS_SITES: &str = r#"
dump() {
  timeout 90 chromium --headless=new --no-sandbox --dump-dom "$1" 2> /dev/null
}
case $(dump https://example.com/) in
  *'<title>Example Domain</title>'*) ;;
  *) exit 1 ;;
esac
case $(dump https://example.org/) in
  *'Example Domain'*) exit 2 ;;
esac
"#;

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn chromium_runs_headless_in_a_machine_without_network() {
    let host = Host::start("chromium").await;
    let info = host
        .create_with(Some(json!({
            "memory_mib": CHROMIUM_MACHINE_MIB,
            "allowed_hosts": [],
        })))
        .await;
    let machine = info["id"].as_str().expect("machine id");
    assert_eq!(
        host.exit_code(machine, CHROMIUM_LOADS_A_PAGE_SERVED_IN_THE_MACHINE)
            .await,
        json!(0)
    );
    let (status, _) = host
        .request("DELETE", &format!("/machines/{machine}"), None)
        .await;
    assert_eq!(status, 204);
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm, SLOP_IMAGE_DIR and internet access; run with make vm-test"]
async fn chromium_reaches_https_sites_through_the_allowlist_without_flags() {
    let host = Host::start("chromium-egress").await;
    let info = host
        .create_with(Some(json!({
            "memory_mib": CHROMIUM_MACHINE_MIB,
            "allowed_hosts": ["example.com"],
        })))
        .await;
    let machine = info["id"].as_str().expect("machine id");
    assert_eq!(
        host.exit_code(machine, CHROMIUM_LOADS_ONLY_ALLOWED_HTTPS_SITES)
            .await,
        json!(0)
    );
    let (status, _) = host
        .request("DELETE", &format!("/machines/{machine}"), None)
        .await;
    assert_eq!(status, 204);
    host.finish().await;
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn a_machine_runs_jailed_and_only_its_tenant_reaches_it() {
    let host = Host::start("tenancy").await;
    let other = format!("Authorization: Bearer {OTHER_API_KEY}\r\n");
    for headers in [
        "",
        "Authorization: Bearer wrong\r\n",
        "Authorization: test-key\r\n",
    ] {
        let (status, _) = host.request_as(headers, "GET", "/machines", None).await;
        assert_eq!(status, 401, "{headers:?}");
    }

    let keyed = format!("Authorization: Bearer {API_KEY}\r\nIdempotency-Key: create-1\r\n");
    let (status, created) = host
        .request_as(
            &keyed,
            "POST",
            "/machines",
            Some(json!({ "allowed_hosts": [] })),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    let machine = created["id"].as_str().expect("machine id").to_owned();
    let (status, replayed) = host
        .request_as(
            &keyed,
            "POST",
            "/machines",
            Some(json!({ "allowed_hosts": [] })),
        )
        .await;
    assert_eq!((status, &replayed["id"]), (201, &created["id"]));
    let (status, _) = host
        .request_as(&keyed, "POST", "/machines", Some(json!({ "vcpus": 1 })))
        .await;
    assert_eq!(status, 422, "a key names one request");
    let (_, listed) = host.request("GET", "/machines", None).await;
    assert_eq!(listed.as_array().map(Vec::len), Some(1));

    for (method, path) in [
        ("GET", format!("/machines/{machine}")),
        ("POST", format!("/machines/{machine}/pause")),
        ("POST", format!("/machines/{machine}/fork")),
        ("GET", format!("/machines/{machine}/health")),
        ("DELETE", format!("/machines/{machine}")),
    ] {
        let (status, _) = host.request_as(&other, method, &path, None).await;
        assert_eq!(status, 404, "{method} {path} as another tenant");
    }
    let (_, others) = host.request_as(&other, "GET", "/machines", None).await;
    assert_eq!(others, json!([]));

    let pid = host.firecracker_pid(&machine);
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("status");
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|value| {
                value
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .expect("status field")
    };
    let uid: u32 = field("Uid:").parse().expect("uid");
    assert!(
        uid >= 1_000_000_000,
        "the VM runs as its own user, not {uid}"
    );
    assert_eq!(
        field("Seccomp:"),
        "2",
        "Firecracker's seccomp filters are on"
    );
    assert_eq!(
        std::fs::read_link(format!("/proc/{pid}/root")).expect("root"),
        PathBuf::from("/"),
        "the VM sees only its jail"
    );
    let cgroup = host.cgroup().join(uid.to_string());
    let limit = |file: &str| std::fs::read_to_string(cgroup.join(file)).expect(file);
    assert_eq!(
        limit("memory.max").trim(),
        ((512u64 + 64) << 20).to_string()
    );
    assert_eq!(limit("cpu.max").trim(), "200000 100000");

    tokio::time::sleep(Duration::from_secs(6)).await;
    let (status, usage) = host.request("GET", "/usage", None).await;
    assert_eq!(status, 200, "{usage}");
    assert_eq!(usage["tenant"], json!("tenant"));
    assert!(
        usage["vcpu_ms"].as_u64().is_some_and(|ms| ms > 0),
        "{usage}"
    );
    let (_, other_usage) = host.request_as(&other, "GET", "/usage", None).await;
    assert_eq!(other_usage["vcpu_ms"], json!(0));

    host.delete(&format!("/machines/{machine}")).await;
    assert!(!running(&pid));
    host.finish().await;
}
