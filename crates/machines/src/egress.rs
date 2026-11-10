use std::{
    fmt, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpStream, UnixListener, UnixStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};

pub const EGRESS_PORT: u32 = 1080;
pub const TUNNEL_PORT: u16 = 443;
const MAX_MACHINE_TUNNELS: usize = 256;
pub const MAX_MACHINE_TUNNEL_FILES: u64 = 2 * MAX_MACHINE_TUNNELS as u64;
const MAX_MACHINE_RESOLUTIONS: usize = 4;
const MAX_REQUEST_HEAD_BYTES: usize = 8192;
const MAX_REQUEST_HEADERS: usize = 64;
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HOSTNAME_BYTES: usize = 253;
const MAX_LABEL_BYTES: usize = 63;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Host {
    Name(String),
    Ip(IpAddr),
}

impl Host {
    fn parse(text: &str) -> Option<Self> {
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

#[derive(Clone, Debug)]
enum HostPattern {
    Any,
    Exact(Host),
    Subdomains(String),
}

#[derive(Clone, Debug, Default)]
pub struct EgressPolicy(Vec<HostPattern>);

impl EgressPolicy {
    pub fn parse(patterns: &[String]) -> Result<Self, String> {
        patterns
            .iter()
            .map(|pattern| parse_pattern(pattern))
            .collect::<Result<_, _>>()
            .map(Self)
    }

    pub fn allows(&self, host: &str) -> bool {
        Host::parse(host).is_some_and(|host| self.permits(&host))
    }

    fn permits(&self, host: &Host) -> bool {
        self.0.iter().any(|pattern| match (pattern, host) {
            (HostPattern::Any, _) => true,
            (HostPattern::Exact(allowed), host) => allowed == host,
            (HostPattern::Subdomains(suffix), Host::Name(name)) => name
                .strip_suffix(suffix.as_str())
                .is_some_and(|label| label.ends_with('.') && label.len() > 1),
            (HostPattern::Subdomains(_), Host::Ip(_)) => false,
        })
    }

    pub fn patterns(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|pattern| match pattern {
                HostPattern::Any => "*".to_owned(),
                HostPattern::Exact(host) => host.to_string(),
                HostPattern::Subdomains(suffix) => format!("*.{suffix}"),
            })
            .collect()
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

struct MachineEgress {
    policy: watch::Receiver<EgressPolicy>,
    resolutions: Arc<Semaphore>,
}

pub async fn serve(listener: UnixListener, policy: watch::Receiver<EgressPolicy>) {
    let machine = Arc::new(MachineEgress {
        policy,
        resolutions: Arc::new(Semaphore::new(MAX_MACHINE_RESOLUTIONS)),
    });
    let mut tunnels = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = tunnels.join_next(), if !tunnels.is_empty() => {}
            accepted = listener.accept() => match accepted {
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
        }
    }
}

struct Target {
    host: Host,
    port: u16,
}

enum Refusal {
    Malformed(&'static str),
    NotConnect,
    PortNotAllowed(u16),
    NotAllowed(Host),
    NoPublicAddress(Host),
    ThisHost(Host),
    Unresolvable(Host),
    Unreachable(Host),
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
            Self::ThisHost(host) => (
                "403 Forbidden",
                format!("{host} is the machine's own host; egress reaches only other hosts"),
            ),
            Self::Unresolvable(host) => ("502 Bad Gateway", format!("{host} did not resolve")),
            Self::Unreachable(host) => (
                "502 Bad Gateway",
                format!("{host} refused or was unreachable"),
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

async fn tunnel(mut guest: UnixStream, machine: &MachineEgress) {
    let Ok(Ok(Some((request, early_data)))) =
        timeout(REQUEST_HEAD_TIMEOUT, read_request_head(&mut guest)).await
    else {
        return;
    };
    let target = match request {
        Ok(target) => target,
        Err(refusal) => return refuse(&mut guest, refusal).await,
    };
    if target.port != TUNNEL_PORT {
        return refuse(&mut guest, Refusal::PortNotAllowed(target.port)).await;
    }
    let mut policy = machine.policy.clone();
    let mut established = false;
    let session = async {
        let mut remote = match open(&target, &machine.resolutions).await {
            Ok(remote) => remote,
            Err(refusal) => return refuse(&mut guest, refusal).await,
        };
        established = true;
        let relayed = async {
            guest
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?;
            remote.write_all(&early_data).await?;
            copy_bidirectional(&mut guest, &mut remote).await
        };
        let _ = relayed.await;
    };
    tokio::select! {
        biased;
        _ = policy.wait_for(|policy| !policy.permits(&target.host)) => {}
        () = session => return,
    }
    if !established {
        refuse(&mut guest, Refusal::NotAllowed(target.host)).await;
    }
}

async fn refuse(guest: &mut UnixStream, refusal: Refusal) {
    let _ = guest.write_all(refusal.response().as_bytes()).await;
}

type RequestHead = (Result<Target, Refusal>, Vec<u8>);

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
                let target = if request.method == Some("CONNECT") {
                    request
                        .path
                        .and_then(parse_authority)
                        .ok_or(Refusal::Malformed("CONNECT target must be host:port"))
                } else {
                    Err(Refusal::NotConnect)
                };
                return Ok(Some((target, buffer.split_off(head_bytes))));
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

async fn open(target: &Target, resolutions: &Arc<Semaphore>) -> Result<TcpStream, Refusal> {
    timeout(OPEN_TIMEOUT, connect_to_public_address(target, resolutions))
        .await
        .unwrap_or_else(|_| Err(Refusal::TimedOut(target.host.clone())))
}

async fn connect_to_public_address(
    target: &Target,
    resolutions: &Arc<Semaphore>,
) -> Result<TcpStream, Refusal> {
    let host = &target.host;
    let addresses = match host {
        Host::Ip(ip) => vec![SocketAddr::new(*ip, target.port)],
        Host::Name(name) => resolve(name, target.port, resolutions)
            .await
            .map_err(|_| Refusal::Unresolvable(host.clone()))?,
    };
    let public: Vec<_> = addresses
        .into_iter()
        .filter(|address| is_public(address.ip()))
        .collect();
    if public.is_empty() {
        return Err(Refusal::NoPublicAddress(host.clone()));
    }
    for address in public {
        if let Ok(Ok(remote)) = timeout(CONNECT_ATTEMPT_TIMEOUT, TcpStream::connect(address)).await
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
