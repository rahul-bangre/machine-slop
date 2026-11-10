use std::{path::PathBuf, process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
};

struct Host {
    child: Child,
    address: String,
    state_dir: PathBuf,
}

impl Host {
    async fn start(name: &str) -> Self {
        let image_dir = std::env::var("SLOP_IMAGE_DIR")
            .expect("SLOP_IMAGE_DIR names a directory built by vm/image/build.sh");
        let state_dir =
            std::env::temp_dir().join(format!("machined-test-{}-{name}", std::process::id()));
        let mut child = Command::new(env!("CARGO_BIN_EXE_machined"))
            .arg("127.0.0.1:0")
            .arg(image_dir)
            .arg(&state_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn machined");
        let mut lines = BufReader::new(child.stderr.take().expect("stderr")).lines();
        let line = lines
            .next_line()
            .await
            .expect("read machined output")
            .expect("machined reports its address");
        let address = line
            .strip_prefix("machined listening on http://")
            .unwrap_or_else(|| panic!("unexpected machined output: {line}"))
            .to_owned();
        Self {
            child,
            address,
            state_dir: std::fs::canonicalize(&state_dir).expect("state dir exists"),
        }
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut stream = TcpStream::connect(&self.address).await.expect("connect");
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: machined\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
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

    async fn exchange(&self, head: &str, body: &[u8]) -> Vec<u8> {
        let mut stream = TcpStream::connect(&self.address).await.expect("connect");
        stream.write_all(head.as_bytes()).await.expect("write head");
        stream.write_all(body).await.expect("write body");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("read");
        response
    }

    async fn tunnel(&self, machine: &str, port: u16) -> Option<TcpStream> {
        let mut stream = TcpStream::connect(&self.address).await.expect("connect");
        stream
            .write_all(
                format!(
                    "GET /machines/{machine}/ports/{port} HTTP/1.1\r\nHost: machined\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n"
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
    }

    fn firecracker_pid(&self, machine: &str) -> String {
        std::fs::read_to_string(
            self.state_dir
                .join("machines")
                .join(machine)
                .join("firecracker.pid"),
        )
        .expect("firecracker pid file")
    }
}

fn running(pid: &str) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
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

    let pid = host.child.id().expect("machined pid").to_string();
    Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .await
        .expect("signal machined");
    let status = tokio::time::timeout(Duration::from_secs(10), host.child.wait())
        .await
        .expect("machined exits after SIGTERM")
        .expect("wait for machined");
    assert!(status.success());
    assert!(
        !running(&second_pid),
        "remaining machines are destroyed on shutdown"
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
        json!({ "allowed_hosts": ["*.*.example.com"] }),
        json!({ "allowed_hosts": "example.com" }),
        json!({ "allowed_hosts": null }),
    ] {
        let (status, _) = host.request("POST", "/machines", Some(body.clone())).await;
        assert_eq!(status, 422, "{body}");
    }
    let (_, machines) = host.request("GET", "/machines", None).await;
    assert_eq!(machines, json!([]));
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
}

#[tokio::test]
#[ignore = "needs /dev/kvm and SLOP_IMAGE_DIR; run with make vm-test"]
async fn a_create_abandoned_by_its_client_still_produces_a_listed_machine() {
    let host = Host::start("abandoned").await;
    let mut stream = TcpStream::connect(&host.address).await.expect("connect");
    stream
        .write_all(b"POST /machines HTTP/1.1\r\nHost: machined\r\nContent-Length: 0\r\n\r\n")
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
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
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
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
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
                "GET /machines/{machine}/processes/{process}/events HTTP/1.1\r\nHost: machined\r\n\r\n"
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
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
}

fn proxy_status_for(url: &str) -> String {
    format!("test \"$(curl -sS --noproxy '' -o /dev/null -w '%{{http_connect}}' {url})\" = 403")
}

#[tokio::test]
#[ignore = "needs /dev/kvm, SLOP_IMAGE_DIR and internet access; run with make vm-test"]
async fn egress_reaches_only_allowed_public_hosts() {
    let host = Host::start("egress").await;

    let allowed = host
        .create_with(Some(json!({ "allowed_hosts": ["Example.com"] })))
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

    let offline = host.create_with(Some(json!({ "allowed_hosts": [] }))).await;
    assert_eq!(offline["allowed_hosts"], json!([]));
    let offline = offline["id"].as_str().expect("machine id");
    assert_eq!(
        host.exit_code(offline, &proxy_status_for("https://example.com/"))
            .await,
        json!(0)
    );

    let unrestricted = host.create_with(None).await;
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

    for machine in [allowed, offline, unrestricted] {
        let (status, _) = host
            .request("DELETE", &format!("/machines/{machine}"), None)
            .await;
        assert_eq!(status, 204);
    }
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
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
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
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
                "PUT /machines/{machine}/files/workspace/site/data.bin HTTP/1.1\r\nHost: machined\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
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
                "GET /machines/{machine}/files/workspace/site/data.bin HTTP/1.1\r\nHost: machined\r\nConnection: close\r\n\r\n"
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
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
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
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
}

const SMALLEST_MACHINE_FOR_CHROMIUM_MIB: u32 = 1024;

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
            "memory_mib": SMALLEST_MACHINE_FOR_CHROMIUM_MIB,
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
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
}

#[tokio::test]
#[ignore = "needs /dev/kvm, SLOP_IMAGE_DIR and internet access; run with make vm-test"]
async fn chromium_reaches_https_sites_through_the_allowlist_without_flags() {
    let host = Host::start("chromium-egress").await;
    let info = host
        .create_with(Some(json!({
            "memory_mib": SMALLEST_MACHINE_FOR_CHROMIUM_MIB,
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
    std::fs::remove_dir_all(&host.state_dir).expect("remove state dir");
}
