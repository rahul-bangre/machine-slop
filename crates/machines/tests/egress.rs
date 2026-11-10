use std::path::PathBuf;

use slop_machines::{EgressPolicy, serve_egress};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::watch,
};

fn policy(patterns: &[&str]) -> EgressPolicy {
    EgressPolicy::parse(
        &patterns
            .iter()
            .map(|pattern| pattern.to_string())
            .collect::<Vec<_>>(),
    )
    .expect("valid patterns")
}

struct Proxy {
    socket: PathBuf,
    policy: watch::Sender<EgressPolicy>,
    server: tokio::task::JoinHandle<()>,
}

impl Proxy {
    fn start(name: &str, policy: EgressPolicy) -> Self {
        let dir = std::env::temp_dir().join(format!("egress-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create socket dir");
        let socket = dir.join("egress.sock");
        let listener = UnixListener::bind(&socket).expect("bind egress socket");
        let (policy, current_policy) = watch::channel(policy);
        let server = tokio::spawn(serve_egress(listener, current_policy));
        Self {
            socket,
            policy,
            server,
        }
    }

    async fn exchange(&self, request: &str) -> String {
        let mut guest = UnixStream::connect(&self.socket).await.expect("connect");
        guest.write_all(request.as_bytes()).await.expect("write");
        let mut response = String::new();
        match guest.read_to_string(&mut response).await {
            Ok(_) => response,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => String::new(),
            Err(error) => panic!("read: {error}"),
        }
    }

    async fn connect(&self, target: &str) -> String {
        self.exchange(&format!(
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"
        ))
        .await
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.server.abort();
        if let Some(dir) = self.socket.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn status(response: &str) -> &str {
    response.get(9..12).unwrap_or(response)
}

#[test]
fn patterns_match_exact_hosts_subdomains_or_everything() {
    let allowed = policy(&["PyPI.org.", "*.githubusercontent.com", "2001:4860:0::8888"]);
    assert!(allowed.allows("pypi.org"));
    assert!(allowed.allows("pypi.org."));
    assert!(!allowed.allows("files.pypi.org"));
    assert!(allowed.allows("raw.githubusercontent.com"));
    assert!(allowed.allows("a.b.githubusercontent.com"));
    assert!(!allowed.allows("githubusercontent.com"));
    assert!(!allowed.allows("evilgithubusercontent.com"));
    assert!(allowed.allows("2001:4860::8888"));
    assert_eq!(
        allowed.patterns(),
        ["pypi.org", "*.githubusercontent.com", "2001:4860::8888"]
    );

    assert!(policy(&["*"]).allows("anything.example"));
    assert!(!policy(&[]).allows("pypi.org"));
}

#[test]
fn malformed_patterns_are_rejected() {
    for pattern in [
        "",
        "*.",
        "*.*.example.com",
        "exa mple.com",
        "-example.com",
        "example.com:443",
        "https://example.com",
        "*.10.0.0.1",
        "*.0.1",
        "8.0.1",
        "0x08080808",
        "[::1]",
    ] {
        assert!(
            EgressPolicy::parse(&[pattern.to_owned()]).is_err(),
            "{pattern:?} is rejected"
        );
    }
}

#[tokio::test]
async fn hosts_outside_the_policy_are_refused() {
    let proxy = Proxy::start("refused", policy(&["pypi.org"]));
    let response = proxy.connect("example.com:443").await;
    assert_eq!(status(&response), "403", "{response}");
    assert!(response.ends_with("egress to example.com is not allowed for this machine\n"));

    let offline = Proxy::start("offline", policy(&[]));
    assert_eq!(status(&offline.connect("pypi.org:443").await), "403");
}

#[tokio::test]
async fn tunnels_reach_only_port_443() {
    let proxy = Proxy::start("port", policy(&["*"]));
    for target in ["github.com:22", "smtp.example.com:25", "example.com:80"] {
        let response = proxy.connect(target).await;
        assert_eq!(status(&response), "403", "{target}: {response}");
        assert!(
            response.contains("egress tunnels only to port 443"),
            "{target}: {response}"
        );
    }
}

#[tokio::test]
async fn non_public_addresses_are_refused_even_when_every_host_is_allowed() {
    let proxy = Proxy::start("private", policy(&["*"]));
    for target in [
        "localhost:443",
        "127.0.0.1:443",
        "10.0.0.1:443",
        "169.254.169.254:443",
        "100.64.0.1:443",
        "[::1]:443",
        "[::ffff:127.0.0.1]:443",
        "[fd00::1]:443",
        "[fe80::1]:443",
    ] {
        let response = proxy.connect(target).await;
        assert_eq!(status(&response), "403", "{target}: {response}");
        assert!(
            response.contains("has no public address"),
            "{target}: {response}"
        );
    }
}

#[tokio::test]
async fn only_well_formed_connect_requests_are_tunnelled() {
    let proxy = Proxy::start("malformed", policy(&["*"]));
    assert_eq!(
        status(
            &proxy
                .exchange("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
                .await
        ),
        "405"
    );
    assert_eq!(status(&proxy.connect("example.com").await), "400");
    assert_eq!(status(&proxy.connect("example.com:0").await), "400");
    for numeric_name in [
        "exa mple.com:443",
        "8.0.1:443",
        "134744072:443",
        "0x08080808:443",
    ] {
        assert_eq!(
            status(&proxy.connect(numeric_name).await),
            "400",
            "{numeric_name}"
        );
    }
    assert_eq!(status(&proxy.exchange("not http\r\n\r\n").await), "400");

    let oversized_head = format!("CONNECT a:1 HTTP/1.1\r\nX: {}\r\n", "a".repeat(9000));
    let response = proxy.exchange(&oversized_head).await;
    assert!(
        response.is_empty() || status(&response) == "400",
        "{response}"
    );
}

#[tokio::test]
async fn a_machine_holds_a_bounded_number_of_proxy_connections() {
    let proxy = Proxy::start("bounded", policy(&[]));
    let mut held = Vec::new();
    for _ in 0..256 {
        let mut guest = UnixStream::connect(&proxy.socket).await.expect("connect");
        guest.write_all(b"CONNECT ").await.expect("write");
        held.push(guest);
    }
    assert_eq!(
        proxy.connect("pypi.org:443").await,
        "",
        "connection over the bound is closed unanswered"
    );

    drop(held.pop());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let response = proxy.connect("pypi.org:443").await;
        if status(&response) == "403" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a released connection frees its place"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn an_updated_policy_decides_the_next_connection() {
    let proxy = Proxy::start("updated", policy(&[]));
    let refused = proxy.connect("10.0.0.1:443").await;
    assert!(
        refused.contains("egress to 10.0.0.1 is not allowed"),
        "{refused}"
    );

    proxy.policy.send_replace(policy(&["10.0.0.1"]));
    let allowed_but_private = proxy.connect("10.0.0.1:443").await;
    assert!(
        allowed_but_private.contains("10.0.0.1 has no public address"),
        "{allowed_but_private}"
    );
}
