use std::{
    fmt, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};

use http::{HeaderMap, HeaderName, HeaderValue, header};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpSocket, TcpStream, UnixListener, UnixStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};
use tokio_rustls::client::TlsStream;

use crate::{
    client_hello::{self, ACCESS_DENIED_ALERT},
    contract::EgressCredential,
    dns,
    gateway::{Gateway, Prefixed},
};

pub const EGRESS_PORT: u32 = 1080;
pub const DNS_PORT: u32 = 1053;
pub const TUNNEL_PORT: u16 = 443;
pub const CERTIFICATE_AUTHORITY_PATH: &str = "/egress-ca.pem";
const MAX_MACHINE_TUNNELS: usize = 256;
const MAX_MACHINE_DNS_CONNECTIONS: usize = 16;
const MAX_MACHINE_RESOLUTIONS: usize = 4;
pub const MAX_MACHINE_EGRESS_FILES: u64 =
    (2 * MAX_MACHINE_TUNNELS + MAX_MACHINE_DNS_CONNECTIONS + MAX_MACHINE_RESOLUTIONS) as u64;
const MAX_REQUEST_HEAD_BYTES: usize = 8192;
const MAX_REQUEST_HEADERS: usize = 64;
pub(crate) const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HOSTNAME_BYTES: usize = 253;
const MAX_LABEL_BYTES: usize = 63;
const MAX_CREDENTIALS: usize = 64;
const MAX_CREDENTIAL_HEADERS: usize = 16;
const MAX_CREDENTIAL_VALUE_BYTES: usize = 8192;
const ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection established\r\n\r\n";
const HEADERS_THE_GATEWAY_OWNS: [HeaderName; 9] = [
    header::HOST,
    header::CONNECTION,
    header::CONTENT_LENGTH,
    header::TRANSFER_ENCODING,
    header::TE,
    header::TRAILER,
    header::UPGRADE,
    header::PROXY_AUTHORIZATION,
    HeaderName::from_static("keep-alive"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Host {
    Name(String),
    Ip(IpAddr),
}

impl Host {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let lowered = text.strip_suffix('.').unwrap_or(text).to_ascii_lowercase();
        match lowered.parse() {
            Ok(ip) => Some(Self::Ip(ip)),
            Err(_) => is_hostname(&lowered).then_some(Self::Name(lowered)),
        }
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(name) => f.write_str(name),
            Self::Ip(ip) => write!(f, "{ip}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HostPattern {
    Any,
    Exact(Host),
    Subdomains(String),
}

impl HostPattern {
    fn matches(&self, host: &Host) -> bool {
        match (self, host) {
            (Self::Any, _) => true,
            (Self::Exact(allowed), host) => allowed == host,
            (Self::Subdomains(suffix), Host::Name(name)) => name
                .strip_suffix(suffix.as_str())
                .is_some_and(|label| label.ends_with('.') && label.len() > 1),
            (Self::Subdomains(_), Host::Ip(_)) => false,
        }
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::Exact(host) => write!(f, "{host}"),
            Self::Subdomains(suffix) => write!(f, "*.{suffix}"),
        }
    }
}

#[derive(Clone, Debug)]
struct Credential {
    host: HostPattern,
    headers: HeaderMap,
}

#[derive(Clone, Debug, Default)]
pub struct EgressPolicy {
    allowed: Vec<HostPattern>,
    credentials: Vec<Credential>,
    source: Option<IpAddr>,
}

impl EgressPolicy {
    pub fn new(
        allowed_hosts: &[String],
        credentials: &[EgressCredential],
        source: Option<IpAddr>,
    ) -> Result<Self, String> {
        Self {
            source,
            ..Self::default()
        }
        .with_allowed_hosts(allowed_hosts)?
        .with_credentials(credentials)
    }

    pub fn parse(allowed_hosts: &[String]) -> Result<Self, String> {
        Self::new(allowed_hosts, &[], None)
    }

    pub fn with_allowed_hosts(&self, allowed_hosts: &[String]) -> Result<Self, String> {
        let allowed = allowed_hosts
            .iter()
            .map(|pattern| parse_pattern(pattern))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            allowed,
            ..self.clone()
        })
    }

    pub fn with_credentials(&self, credentials: &[EgressCredential]) -> Result<Self, String> {
        if credentials.len() > MAX_CREDENTIALS {
            return Err(format!(
                "at most {MAX_CREDENTIALS} egress credentials are allowed"
            ));
        }
        let mut parsed: Vec<Credential> = Vec::with_capacity(credentials.len());
        for credential in credentials {
            let credential = parse_credential(credential)?;
            if parsed.iter().any(|earlier| earlier.host == credential.host) {
                return Err(format!(
                    "egress credential host {} is listed twice",
                    credential.host
                ));
            }
            parsed.push(credential);
        }
        Ok(Self {
            credentials: parsed,
            ..self.clone()
        })
    }

    pub fn allows(&self, host: &str) -> bool {
        Host::parse(host).is_some_and(|host| self.permits(&host))
    }

    pub(crate) fn permits(&self, host: &Host) -> bool {
        self.allowed.iter().any(|pattern| pattern.matches(host))
    }

    pub fn allowed_hosts(&self) -> Vec<String> {
        self.allowed.iter().map(ToString::to_string).collect()
    }

    pub fn credential_hosts(&self) -> Vec<String> {
        self.credentials
            .iter()
            .map(|credential| credential.host.to_string())
            .collect()
    }

    pub fn credentials(&self) -> Vec<EgressCredential> {
        self.credentials
            .iter()
            .map(|credential| EgressCredential {
                host: credential.host.to_string(),
                headers: credential
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.to_string(),
                            value.to_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn source(&self) -> Option<IpAddr> {
        self.source
    }

    pub(crate) fn credential_headers(&self, host: &Host) -> Option<&HeaderMap> {
        self.credentials
            .iter()
            .find(|credential| credential.host.matches(host))
            .map(|credential| &credential.headers)
    }
}

fn parse_pattern(pattern: &str) -> Result<HostPattern, String> {
    let parsed = match pattern {
        "*" => Some(HostPattern::Any),
        _ => match pattern.strip_prefix("*.") {
            Some(suffix) => match Host::parse(suffix) {
                Some(Host::Name(suffix)) => Some(HostPattern::Subdomains(suffix)),
                _ => None,
            },
            None => Host::parse(pattern).map(HostPattern::Exact),
        },
    };
    parsed.ok_or_else(|| {
        format!(
            "allowed host {pattern:?} must be \"*\", a hostname, \"*.\" followed by a hostname, or an IP address"
        )
    })
}

fn parse_credential(credential: &EgressCredential) -> Result<Credential, String> {
    let host = match parse_pattern(&credential.host) {
        Ok(host @ (HostPattern::Exact(Host::Name(_)) | HostPattern::Subdomains(_))) => host,
        _ => {
            return Err(format!(
                "egress credential host {:?} must be a hostname or \"*.\" followed by a hostname",
                credential.host
            ));
        }
    };
    if credential.headers.is_empty() || credential.headers.len() > MAX_CREDENTIAL_HEADERS {
        return Err(format!(
            "egress credential for {host} must set between 1 and {MAX_CREDENTIAL_HEADERS} headers"
        ));
    }
    let mut headers = HeaderMap::new();
    for (name, value) in &credential.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("egress credential header name {name:?} is not valid"))?;
        if HEADERS_THE_GATEWAY_OWNS.contains(&name) {
            return Err(format!(
                "egress credential header {name} is managed by the connection and cannot be set"
            ));
        }
        let mut value = Some(value.as_str())
            .filter(|value| value.is_ascii() && value.len() <= MAX_CREDENTIAL_VALUE_BYTES)
            .and_then(|value| HeaderValue::from_str(value).ok())
            .ok_or_else(|| {
                format!(
                    "egress credential header {name} must be printable ASCII of at most {MAX_CREDENTIAL_VALUE_BYTES} bytes"
                )
            })?;
        value.set_sensitive(true);
        if headers.insert(name.clone(), value).is_some() {
            return Err(format!(
                "egress credential header {name} is listed twice for {host}"
            ));
        }
    }
    Ok(Credential { host, headers })
}

fn is_hostname(name: &str) -> bool {
    name.len() <= MAX_HOSTNAME_BYTES
        && !name.rsplit('.').next().is_some_and(reads_as_number)
        && name.split('.').all(|label| {
            (1..=MAX_LABEL_BYTES).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn reads_as_number(label: &str) -> bool {
    match label.strip_prefix("0x") {
        Some(hex) => hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
        None => label.bytes().all(|byte| byte.is_ascii_digit()),
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map_or_else(|| is_public_v6(ip), is_public_v4),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    let this_network = first == 0;
    let shared_address_space = first == 100 && (64..128).contains(&second);
    let protocol_assignments = first == 192 && second == 0 && third == 0;
    let benchmarking = first == 198 && (18..20).contains(&second);
    let reserved = first >= 240;
    !(this_network
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || shared_address_space
        || protocol_assignments
        || benchmarking
        || ip.is_documentation()
        || ip.is_multicast()
        || reserved)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let [first, second, ..] = ip.segments();
    let global_unicast = first & 0xe000 == 0x2000;
    let special_purpose = first == 0x2001 && second < 0x0200;
    let documentation = (first == 0x2001 && second == 0x0db8) || first & 0xfff0 == 0x3ff0;
    let six_to_four = first == 0x2002;
    global_unicast && !special_purpose && !documentation && !six_to_four
}

pub(crate) struct Egress {
    policy: watch::Sender<EgressPolicy>,
    gateway: Arc<Gateway>,
}

impl Egress {
    pub(crate) fn new(policy: EgressPolicy, gateway: Arc<Gateway>) -> Self {
        Self {
            policy: watch::Sender::new(policy),
            gateway,
        }
    }

    pub(crate) fn policy(&self) -> EgressPolicy {
        self.policy.borrow().clone()
    }

    pub(crate) fn replace(&self, policy: EgressPolicy) {
        self.policy.send_replace(policy);
    }

    pub(crate) fn route(&self) -> MachineEgress {
        MachineEgress::new(self.policy.subscribe(), self.gateway.clone())
    }
}

pub(crate) struct MachineEgress {
    pub(crate) policy: watch::Receiver<EgressPolicy>,
    pub(crate) resolutions: Arc<Semaphore>,
    pub(crate) gateway: Arc<Gateway>,
}

impl MachineEgress {
    fn new(policy: watch::Receiver<EgressPolicy>, gateway: Arc<Gateway>) -> Self {
        Self {
            policy,
            resolutions: Arc::new(Semaphore::new(MAX_MACHINE_RESOLUTIONS)),
            gateway,
        }
    }
}

pub async fn serve(
    proxy: UnixListener,
    dns: UnixListener,
    policy: watch::Receiver<EgressPolicy>,
    gateway: Arc<Gateway>,
) {
    serve_machine(proxy, dns, MachineEgress::new(policy, gateway)).await;
}

pub(crate) async fn serve_machine(proxy: UnixListener, dns: UnixListener, machine: MachineEgress) {
    let machine = Arc::new(machine);
    let mut tunnels = JoinSet::new();
    let mut lookups = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = tunnels.join_next(), if !tunnels.is_empty() => {}
            Some(_) = lookups.join_next(), if !lookups.is_empty() => {}
            accepted = proxy.accept() => match accepted {
                Ok((guest, _)) if tunnels.len() < MAX_MACHINE_TUNNELS => {
                    let machine = machine.clone();
                    tunnels.spawn(async move { tunnel(guest, &machine).await });
                }
                Ok(_over_bound) => {}
                Err(error) => {
                    tracing::error!(%error, "egress accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            accepted = dns.accept(), if lookups.len() < MAX_MACHINE_DNS_CONNECTIONS => match accepted {
                Ok((guest, _)) => {
                    let machine = machine.clone();
                    lookups.spawn(async move { dns::serve_connection(guest, &machine).await });
                }
                Err(error) => {
                    tracing::error!(%error, "dns accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
        }
    }
}

pub(crate) struct Target {
    host: Host,
    port: u16,
}

enum GuestRequest {
    Connect(Target),
    CertificateAuthority,
}

pub(crate) enum Refusal {
    Malformed(&'static str),
    NotConnect,
    PortNotAllowed(u16),
    NotAllowed(Host),
    NoPublicAddress(Host),
    NoAddressFromSource(Host, IpAddr),
    ThisHost(Host),
    Unresolvable(Host),
    Unreachable(Host),
    Untrusted(Host, String),
    TimedOut(Host),
}

impl Refusal {
    fn response(&self) -> String {
        let (status, message) = match self {
            Self::Malformed(reason) => ("400 Bad Request", (*reason).to_owned()),
            Self::NotConnect => (
                "405 Method Not Allowed",
                "egress supports only CONNECT tunnels; use HTTPS".to_owned(),
            ),
            Self::PortNotAllowed(port) => (
                "403 Forbidden",
                format!("egress tunnels only to port {TUNNEL_PORT}, not {port}"),
            ),
            Self::NotAllowed(host) => (
                "403 Forbidden",
                format!("egress to {host} is not allowed for this machine"),
            ),
            Self::NoPublicAddress(host) => (
                "403 Forbidden",
                format!("{host} has no public address; egress reaches only public addresses"),
            ),
            Self::NoAddressFromSource(host, source) => (
                "502 Bad Gateway",
                format!(
                    "{host} has no public address of the same family as this machine's egress address {source}"
                ),
            ),
            Self::ThisHost(host) => (
                "403 Forbidden",
                format!("{host} is the machine's own host; egress reaches only other hosts"),
            ),
            Self::Unresolvable(host) => ("502 Bad Gateway", format!("{host} did not resolve")),
            Self::Unreachable(host) => (
                "502 Bad Gateway",
                format!("{host} refused or was unreachable"),
            ),
            Self::Untrusted(host, error) => (
                "502 Bad Gateway",
                format!("{host} did not present a trusted TLS certificate: {error}"),
            ),
            Self::TimedOut(host) => (
                "504 Gateway Timeout",
                format!("{host} did not answer within {}s", OPEN_TIMEOUT.as_secs()),
            ),
        };
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}\n",
            message.len() + 1
        )
    }
}

enum Upstream {
    Tunnelled(TcpStream),
    Intercepted(Box<TlsStream<TcpStream>>),
}

async fn tunnel(mut guest: UnixStream, machine: &MachineEgress) {
    let Ok(Ok(Some((request, early_data)))) =
        timeout(REQUEST_HEAD_TIMEOUT, read_request_head(&mut guest)).await
    else {
        return;
    };
    let target = match request {
        Ok(GuestRequest::Connect(target)) => target,
        Ok(GuestRequest::CertificateAuthority) => {
            let pem = machine.gateway.certificate_pem();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-pem-file\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{pem}",
                pem.len()
            );
            let _ = guest.write_all(response.as_bytes()).await;
            return;
        }
        Err(refusal) => return refuse(&mut guest, refusal).await,
    };
    if target.port != TUNNEL_PORT {
        return refuse(&mut guest, Refusal::PortNotAllowed(target.port)).await;
    }
    let mut policy = machine.policy.clone();
    let opened = open_upstream(&target, machine);
    let opened = tokio::select! {
        biased;
        _ = policy.wait_for(|policy| !policy.permits(&target.host)) => {
            Err(Refusal::NotAllowed(target.host.clone()))
        }
        opened = opened => opened,
    };
    let upstream = match opened {
        Ok(upstream) => upstream,
        Err(refusal) => return refuse(&mut guest, refusal).await,
    };
    let session = relay(guest, early_data, upstream, &target.host, machine);
    tokio::select! {
        biased;
        _ = policy.wait_for(|policy| !policy.permits(&target.host)) => {}
        () = session => {}
    }
}

async fn open_upstream(target: &Target, machine: &MachineEgress) -> Result<Upstream, Refusal> {
    let opening = async {
        let policy = machine.policy.borrow().clone();
        let remote = connect_to_public_address(target, policy.source, &machine.resolutions).await?;
        match &target.host {
            Host::Name(name) if policy.credential_headers(&target.host).is_some() => machine
                .gateway
                .connect_upstream(name, remote)
                .await
                .map(|tls| Upstream::Intercepted(Box::new(tls)))
                .map_err(|error| Refusal::Untrusted(target.host.clone(), error.to_string())),
            _ => Ok(Upstream::Tunnelled(remote)),
        }
    };
    timeout(OPEN_TIMEOUT, opening)
        .await
        .unwrap_or_else(|_| Err(Refusal::TimedOut(target.host.clone())))
}

async fn relay(
    mut guest: UnixStream,
    early_data: Vec<u8>,
    upstream: Upstream,
    target: &Host,
    machine: &MachineEgress,
) {
    if guest.write_all(ESTABLISHED).await.is_err() {
        return;
    }
    let Ok(Some(hello)) = timeout(
        REQUEST_HEAD_TIMEOUT,
        client_hello::read(&mut guest, early_data),
    )
    .await
    else {
        return;
    };
    let names_target = match (target, hello.server_name.as_deref()) {
        (Host::Name(name), Some(server_name)) => name == server_name,
        (Host::Name(_), None) => false,
        (Host::Ip(_), None) => true,
        (Host::Ip(_), Some(server_name)) => machine.policy.borrow().allows(server_name),
    };
    if !names_target {
        let _ = guest.write_all(&ACCESS_DENIED_ALERT).await;
        return;
    }
    match upstream {
        Upstream::Tunnelled(mut remote) => {
            if remote.write_all(&hello.bytes).await.is_ok() {
                let _ = copy_bidirectional(&mut guest, &mut remote).await;
            }
        }
        Upstream::Intercepted(remote) => {
            let guest = Prefixed::new(hello.bytes, guest);
            machine
                .gateway
                .serve(guest, *remote, target, machine.policy.clone())
                .await;
        }
    }
}

async fn refuse(guest: &mut UnixStream, refusal: Refusal) {
    let _ = guest.write_all(refusal.response().as_bytes()).await;
}

type RequestHead = (Result<GuestRequest, Refusal>, Vec<u8>);

async fn read_request_head(guest: &mut UnixStream) -> io::Result<Option<RequestHead>> {
    let mut buffer = Vec::with_capacity(MAX_REQUEST_HEAD_BYTES);
    loop {
        if buffer.len() == MAX_REQUEST_HEAD_BYTES {
            return Ok(Some((
                Err(Refusal::Malformed("request head is too large")),
                Vec::new(),
            )));
        }
        let mut chunk = [0; 1024];
        let room = (MAX_REQUEST_HEAD_BYTES - buffer.len()).min(chunk.len());
        let read = guest.read(&mut chunk[..room]).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        let mut headers = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
        let mut request = httparse::Request::new(&mut headers);
        match request.parse(&buffer) {
            Ok(httparse::Status::Partial) => {}
            Ok(httparse::Status::Complete(head_bytes)) => {
                let parsed = match (request.method, request.path) {
                    (Some("CONNECT"), path) => path
                        .and_then(parse_authority)
                        .map(GuestRequest::Connect)
                        .ok_or(Refusal::Malformed("CONNECT target must be host:port")),
                    (Some("GET"), Some(CERTIFICATE_AUTHORITY_PATH)) => {
                        Ok(GuestRequest::CertificateAuthority)
                    }
                    _ => Err(Refusal::NotConnect),
                };
                return Ok(Some((parsed, buffer.split_off(head_bytes))));
            }
            Err(_) => {
                return Ok(Some((
                    Err(Refusal::Malformed("malformed request head")),
                    Vec::new(),
                )));
            }
        }
    }
}

fn parse_authority(authority: &str) -> Option<Target> {
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split_once("]:")?,
        None => authority.rsplit_once(':')?,
    };
    let host = Host::parse(host)?;
    let port = port.parse().ok().filter(|port| *port != 0)?;
    Some(Target { host, port })
}

pub(crate) enum NoAddress {
    Unresolvable,
    NotPublic,
    NoneFromSource(IpAddr),
}

impl NoAddress {
    fn refusal(self, host: &Host) -> Refusal {
        let host = host.clone();
        match self {
            Self::Unresolvable => Refusal::Unresolvable(host),
            Self::NotPublic => Refusal::NoPublicAddress(host),
            Self::NoneFromSource(source) => Refusal::NoAddressFromSource(host, source),
        }
    }
}

pub(crate) async fn public_addresses(
    host: &Host,
    port: u16,
    source: Option<IpAddr>,
    resolutions: &Arc<Semaphore>,
) -> Result<Vec<SocketAddr>, NoAddress> {
    let addresses = match host {
        Host::Ip(ip) => vec![SocketAddr::new(*ip, port)],
        Host::Name(name) => resolve(name, port, resolutions)
            .await
            .map_err(|_| NoAddress::Unresolvable)?,
    };
    let mut public: Vec<_> = addresses
        .into_iter()
        .filter(|address| is_public(address.ip()))
        .collect();
    if public.is_empty() {
        return Err(NoAddress::NotPublic);
    }
    let Some(source) = source else {
        return Ok(public);
    };
    public.retain(|address| address.is_ipv4() == source.is_ipv4());
    if public.is_empty() {
        return Err(NoAddress::NoneFromSource(source));
    }
    Ok(public)
}

async fn connect_to_public_address(
    target: &Target,
    source: Option<IpAddr>,
    resolutions: &Arc<Semaphore>,
) -> Result<TcpStream, Refusal> {
    let host = &target.host;
    let addresses = public_addresses(host, target.port, source, resolutions)
        .await
        .map_err(|no_address| no_address.refusal(host))?;
    for address in addresses {
        if let Ok(Ok(remote)) =
            timeout(CONNECT_ATTEMPT_TIMEOUT, connect_from(source, address)).await
        {
            return if leads_elsewhere(&remote) {
                Ok(remote)
            } else {
                Err(Refusal::ThisHost(host.clone()))
            };
        }
    }
    Err(Refusal::Unreachable(host.clone()))
}

async fn connect_from(source: Option<IpAddr>, address: SocketAddr) -> io::Result<TcpStream> {
    let socket = match address {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    if let Some(source) = source {
        socket.bind(SocketAddr::new(source, 0))?;
    }
    socket.connect(address).await
}

pub fn can_send_from(source: IpAddr) -> io::Result<()> {
    let socket = match source {
        IpAddr::V4(_) => TcpSocket::new_v4()?,
        IpAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.bind(SocketAddr::new(source, 0))
}

async fn resolve(
    name: &str,
    port: u16,
    resolutions: &Arc<Semaphore>,
) -> io::Result<Vec<SocketAddr>> {
    let resolution = resolutions
        .clone()
        .acquire_owned()
        .await
        .map_err(io::Error::other)?;
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || {
        let addresses = (name.as_str(), port)
            .to_socket_addrs()
            .map(Iterator::collect);
        drop(resolution);
        addresses
    })
    .await
    .map_err(io::Error::other)?
}

fn leads_elsewhere(remote: &TcpStream) -> bool {
    matches!(
        (remote.local_addr(), remote.peer_addr()),
        (Ok(local), Ok(peer)) if local.ip() != peer.ip()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn credential(host: &str, headers: &[(&str, &str)]) -> EgressCredential {
        EgressCredential {
            host: host.to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn credentials_apply_to_the_first_matching_host_and_round_trip() {
        let policy = EgressPolicy::new(
            &["*".into()],
            &[
                credential("API.example.com", &[("Authorization", "Bearer one")]),
                credential("*.example.com", &[("x-api-key", "two")]),
            ],
            None,
        )
        .expect("valid credentials");
        let headers = |host: &str| {
            policy
                .credential_headers(&Host::parse(host).expect("host"))
                .map(|headers| {
                    headers
                        .iter()
                        .map(|(n, v)| format!("{n}={}", v.to_str().unwrap_or_default()))
                        .collect::<Vec<_>>()
                })
        };
        assert_eq!(
            headers("api.example.com"),
            Some(vec!["authorization=Bearer one".to_owned()])
        );
        assert_eq!(
            headers("other.example.com"),
            Some(vec!["x-api-key=two".to_owned()])
        );
        assert_eq!(headers("example.com"), None);
        assert_eq!(
            policy.credential_hosts(),
            vec!["api.example.com", "*.example.com"]
        );
        let printed = format!("{policy:?} {:?}", policy.credentials());
        assert!(
            !printed.contains("Bearer one") && !printed.contains("two\""),
            "debug output hides credential values: {printed}"
        );
        let restored = EgressPolicy::new(&policy.allowed_hosts(), &policy.credentials(), None)
            .expect("recorded credentials parse");
        assert_eq!(restored.credentials(), policy.credentials());
    }

    #[test]
    fn credentials_that_could_not_be_injected_safely_are_rejected() {
        let rejected = [
            credential("*", &[("authorization", "x")]),
            credential("93.184.215.14", &[("authorization", "x")]),
            credential("example.com", &[]),
            credential("example.com", &[("host", "elsewhere.com")]),
            credential("example.com", &[("Content-Length", "0")]),
            credential("example.com", &[("bad header", "x")]),
            credential("example.com", &[("authorization", "line\r\nbreak")]),
            credential("example.com", &[("authorization", "caf\u{e9}")]),
        ];
        for credential in rejected {
            assert!(
                EgressPolicy::new(&[], std::slice::from_ref(&credential), None).is_err(),
                "{credential:?} should be rejected"
            );
        }
        let twice = [
            credential("example.com", &[("a", "1")]),
            credential("Example.com", &[("b", "2")]),
        ];
        assert!(EgressPolicy::new(&[], &twice, None).is_err());
    }
}
