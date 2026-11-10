use std::{path::PathBuf, sync::Arc};

use rustls::{ClientConfig, RootCertStore, crypto::ring, pki_types::ServerName};
use slop_machines::{
    CERTIFICATE_AUTHORITY_PATH, EgressCredential, EgressPolicy, Gateway, serve_egress,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::watch,
};
use tokio_rustls::TlsConnector;

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
    dns_socket: PathBuf,
    gateway: Arc<Gateway>,
    policy: watch::Sender<EgressPolicy>,
    server: tokio::task::JoinHandle<()>,
}

impl Proxy {
    async fn start(name: &str, policy: EgressPolicy) -> Self {
        let dir = std::env::temp_dir().join(format!("egress-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create socket dir");
        let socket = dir.join("egress.sock");
        let dns_socket = dir.join("dns.sock");
        let listener = UnixListener::bind(&socket).expect("bind egress socket");
        let dns = UnixListener::bind(&dns_socket).expect("bind dns socket");
        let gateway = Gateway::open(&dir).await.expect("open gateway");
        let (policy, current_policy) = watch::channel(policy);
        let server = tokio::spawn(serve_egress(listener, dns, current_policy, gateway.clone()));
        Self {
            socket,
            dns_socket,
            gateway,
            policy,
            server,
        }
    }

    async fn tunnel(&self, target: &str) -> UnixStream {
        let mut guest = UnixStream::connect(&self.socket).await.expect("connect");
        guest
            .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            guest
                .read_exact(&mut byte)
                .await
                .expect("read tunnel response");
            head.push(byte[0]);
        }
        assert!(
            head.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&head)
        );
        guest
    }

    async fn resolve(&self, name: &str, record_type: u16) -> Vec<u8> {
        let mut query = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            query.push(u8::try_from(label.len()).expect("short label"));
            query.extend(label.as_bytes());
        }
        query.push(0);
        query.extend(record_type.to_be_bytes());
        query.extend(1u16.to_be_bytes());
        let mut guest = UnixStream::connect(&self.dns_socket)
            .await
            .expect("connect dns");
        let length = u16::try_from(query.len()).expect("short query");
        guest.write_all(&length.to_be_bytes()).await.expect("write");
        guest.write_all(&query).await.expect("write");
        let mut length = [0; 2];
        guest.read_exact(&mut length).await.expect("read length");
        let mut response = vec![0; usize::from(u16::from_be_bytes(length))];
        guest
            .read_exact(&mut response)
            .await
            .expect("read response");
        assert_eq!(&response[..2], &[0x12, 0x34]);
        response
    }

    fn trusting_gateway(&self) -> TlsConnector {
        let mut roots = RootCertStore::empty();
        let pem = self.gateway.certificate_pem().as_bytes().to_vec();
        for certificate in rustls_pemfile_certificates(&pem) {
            roots.add(certificate).expect("gateway certificate");
        }
        connector(roots)
    }

    async fn exchange(&self, request: &str) -> String {
        let mut guest = UnixStream::connect(&self.socket).await.expect("connect");
        if let Err(error) = guest.write_all(request.as_bytes()).await {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ),
                "write: {error}"
            );
        }
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
        allowed.allowed_hosts(),
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
    let proxy = Proxy::start("refused", policy(&["pypi.org"])).await;
    let response = proxy.connect("example.com:443").await;
    assert_eq!(status(&response), "403", "{response}");
    assert!(response.ends_with("egress to example.com is not allowed for this machine\n"));

    let offline = Proxy::start("offline", policy(&[])).await;
    assert_eq!(status(&offline.connect("pypi.org:443").await), "403");
}

#[tokio::test]
async fn tunnels_reach_only_port_443() {
    let proxy = Proxy::start("port", policy(&["*"])).await;
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
    let proxy = Proxy::start("private", policy(&["*"])).await;
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
    let proxy = Proxy::start("malformed", policy(&["*"])).await;
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
    let proxy = Proxy::start("bounded", policy(&[])).await;
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
    let proxy = Proxy::start("updated", policy(&[])).await;
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

fn connector(roots: RootCertStore) -> TlsConnector {
    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

fn public_roots() -> RootCertStore {
    RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

fn rustls_pemfile_certificates(pem: &[u8]) -> Vec<rustls::pki_types::CertificateDer<'static>> {
    use rustls::pki_types::pem::PemObject;
    rustls::pki_types::CertificateDer::pem_slice_iter(pem)
        .collect::<Result<_, _>>()
        .expect("pem certificates")
}

fn rcode(response: &[u8]) -> u8 {
    response[3] & 0x0f
}

fn answer_count(response: &[u8]) -> u16 {
    u16::from_be_bytes([response[6], response[7]])
}

#[tokio::test]
async fn the_gateway_certificate_is_served_to_the_machine_and_kept_across_restarts() {
    let proxy = Proxy::start("authority", policy(&[])).await;
    let response = proxy
        .exchange(&format!(
            "GET {CERTIFICATE_AUTHORITY_PATH} HTTP/1.1\r\nHost: egress\r\n\r\n"
        ))
        .await;
    assert_eq!(status(&response), "200", "{response}");
    let pem = proxy.gateway.certificate_pem();
    assert!(response.ends_with(pem), "{response}");
    assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
    let dir = proxy.socket.parent().expect("socket dir");
    let reopened = Gateway::open(dir).await.expect("reopen gateway");
    assert_eq!(reopened.certificate_pem(), pem);
}

#[tokio::test]
async fn dns_answers_only_names_the_policy_allows_and_only_with_public_addresses() {
    let proxy = Proxy::start("dns", policy(&["localhost", "*.example.com"])).await;
    let refused = proxy.resolve("example.org", 1).await;
    assert_eq!(rcode(&refused), 5, "a name outside the list is refused");
    assert_eq!(answer_count(&refused), 0);

    let loopback = proxy.resolve("LocalHost", 1).await;
    assert_eq!(rcode(&loopback), 0);
    assert_eq!(
        answer_count(&loopback),
        0,
        "loopback addresses are never answered"
    );

    let mail = proxy.resolve("www.example.com", 15).await;
    assert_eq!(rcode(&mail), 0, "other record types are answered empty");
    assert_eq!(answer_count(&mail), 0);

    proxy.policy.send_replace(policy(&[]));
    assert_eq!(rcode(&proxy.resolve("localhost", 1).await), 5);
}

#[tokio::test]
#[ignore = "needs internet access"]
async fn dns_resolves_allowed_public_names() {
    let proxy = Proxy::start("dns-public", policy(&["example.com"])).await;
    let response = proxy.resolve("example.com", 1).await;
    assert_eq!(rcode(&response), 0);
    assert!(answer_count(&response) > 0);
}

#[tokio::test]
#[ignore = "needs internet access"]
async fn a_tunnel_carries_only_tls_naming_the_host_it_was_opened_for() {
    let proxy = Proxy::start("server-name", policy(&["example.com", "example.org"])).await;
    let matching = proxy.tunnel("example.com:443").await;
    let name = ServerName::try_from("example.com").expect("name");
    connector(public_roots())
        .connect(name, matching)
        .await
        .expect("a hello naming the tunnelled host reaches it");

    let fronted = proxy.tunnel("example.com:443").await;
    let other = ServerName::try_from("example.org").expect("name");
    let refused = connector(public_roots()).connect(other, fronted).await;
    let error = refused.expect_err("a hello naming another host is refused");
    assert!(
        error.to_string().contains("AccessDenied"),
        "the guest sees an access_denied alert: {error}"
    );

    let mut plaintext = proxy.tunnel("example.com:443").await;
    plaintext
        .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await
        .expect("write");
    let mut rest = Vec::new();
    let _ = plaintext.read_to_end(&mut rest).await;
    assert!(rest.is_empty(), "bytes that are not TLS are not relayed");
}

#[tokio::test]
#[ignore = "needs internet access"]
async fn credentials_are_injected_for_their_host_without_entering_the_machine() {
    let echo = "postman-echo.com";
    let credential = EgressCredential {
        host: echo.to_owned(),
        headers: [("x-injected-key".to_owned(), "secret-from-host".to_owned())].into(),
    };
    let policy = EgressPolicy::new(&[echo.to_owned()], &[credential], None).expect("policy");
    let proxy = Proxy::start("inject", policy).await;

    let request = |host: &str| {
        format!(
            "GET /headers HTTP/1.1\r\nHost: {host}\r\nX-Injected-Key: guest-placeholder\r\nConnection: close\r\n\r\n"
        )
    };
    let name = ServerName::try_from(echo).expect("name");
    let mut tls = proxy
        .trusting_gateway()
        .connect(name.clone(), proxy.tunnel(&format!("{echo}:443")).await)
        .await
        .expect("the gateway presents a certificate for the host signed by its authority");
    tls.write_all(request(echo).as_bytes())
        .await
        .expect("write");
    let mut response = String::new();
    tls.read_to_string(&mut response).await.expect("read");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("secret-from-host"), "{response}");
    assert!(!response.contains("guest-placeholder"), "{response}");

    let mut misdirected = proxy
        .trusting_gateway()
        .connect(name.clone(), proxy.tunnel(&format!("{echo}:443")).await)
        .await
        .expect("handshake");
    misdirected
        .write_all(request("elsewhere.example").as_bytes())
        .await
        .expect("write");
    let mut response = String::new();
    misdirected
        .read_to_string(&mut response)
        .await
        .expect("read");
    assert!(response.starts_with("HTTP/1.1 421"), "{response}");

    let untrusting = connector(public_roots())
        .connect(name, proxy.tunnel(&format!("{echo}:443")).await)
        .await;
    assert!(
        untrusting.is_err(),
        "a client that does not trust the gateway cannot be sent credentials"
    );
}
