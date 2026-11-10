use std::{
    convert::Infallible,
    io,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use http::{Request, Response, StatusCode, Uri, header};
use http_body_util::Either;
use hyper::{
    body::Incoming, client::conn::http1::SendRequest, service::service_fn, upgrade::OnUpgrade,
};
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SerialNumber, date_time_ymd,
};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    crypto::ring,
    pki_types::{PrivateKeyDer, ServerName},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf, copy_bidirectional},
    net::TcpStream,
    sync::{Mutex, Notify, watch},
};
use tokio_rustls::{TlsAcceptor, TlsConnector, client::TlsStream};

use crate::egress::{EgressPolicy, Host, REQUEST_HEAD_TIMEOUT};

const CA_KEY: &str = "egress-ca.key";
const CA_CERTIFICATE: &str = "egress-ca.pem";
const CA_NAME: &str = "machine-slop egress gateway";
const HTTP1: &[u8] = b"http/1.1";

type Body = Either<Incoming, String>;
type PendingUpgrade = std::sync::Mutex<Option<(OnUpgrade, OnUpgrade)>>;

pub struct Gateway {
    certificate_pem: String,
    issuer: Issuer<'static, KeyPair>,
    leaf_key: KeyPair,
    upstream: TlsConnector,
}

impl Gateway {
    pub async fn open(state_dir: &Path) -> io::Result<Arc<Self>> {
        let key_path = state_dir.join(CA_KEY);
        let certificate_path = state_dir.join(CA_CERTIFICATE);
        let recorded = match (
            tokio::fs::read_to_string(&key_path).await,
            tokio::fs::read_to_string(&certificate_path).await,
        ) {
            (Ok(key), Ok(certificate)) => Some((key, certificate)),
            (Err(error), _) | (_, Err(error)) if error.kind() == io::ErrorKind::NotFound => None,
            (Err(error), _) | (_, Err(error)) => return Err(error),
        };
        let (key, certificate_pem) = match recorded {
            Some((key, certificate)) => (
                KeyPair::from_pem(&key).map_err(io::Error::other)?,
                certificate,
            ),
            None => {
                let key = KeyPair::generate().map_err(io::Error::other)?;
                let certificate = authority_params()
                    .self_signed(&key)
                    .map_err(io::Error::other)?
                    .pem();
                write_private(&key_path, &key.serialize_pem()).await?;
                write_private(&certificate_path, &certificate).await?;
                (key, certificate)
            }
        };
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut upstream = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        upstream.alpn_protocols = vec![HTTP1.to_vec()];
        Ok(Arc::new(Self {
            certificate_pem,
            issuer: Issuer::new(authority_params(), key),
            leaf_key: KeyPair::generate().map_err(io::Error::other)?,
            upstream: TlsConnector::from(Arc::new(upstream)),
        }))
    }

    pub fn certificate_pem(&self) -> &str {
        &self.certificate_pem
    }

    pub(crate) async fn connect_upstream(
        &self,
        name: &str,
        remote: TcpStream,
    ) -> io::Result<TlsStream<TcpStream>> {
        let server_name = ServerName::try_from(name.to_owned()).map_err(io::Error::other)?;
        self.upstream.connect(server_name, remote).await
    }

    fn acceptor_for(&self, name: &str) -> Result<TlsAcceptor, Box<dyn std::error::Error>> {
        let mut params = CertificateParams::new(vec![name.to_owned()])?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, name);
        params.serial_number = Some(SerialNumber::from_slice(uuid::Uuid::new_v4().as_bytes()));
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        set_validity(&mut params);
        let certificate = params.signed_by(&self.leaf_key, &self.issuer)?;
        let mut config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                PrivateKeyDer::Pkcs8(self.leaf_key.serialize_der().into()),
            )?;
        config.alpn_protocols = vec![HTTP1.to_vec()];
        Ok(TlsAcceptor::from(Arc::new(config)))
    }

    pub(crate) async fn serve<G: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        &self,
        guest: G,
        upstream: TlsStream<TcpStream>,
        target: &Host,
        policy: watch::Receiver<EgressPolicy>,
    ) {
        let name = target.to_string();
        let acceptor = match self.acceptor_for(&name) {
            Ok(acceptor) => acceptor,
            Err(error) => {
                tracing::error!(host = %name, %error, "gateway certificate failed");
                return;
            }
        };
        let Ok(Ok(guest)) =
            tokio::time::timeout(REQUEST_HEAD_TIMEOUT, acceptor.accept(guest)).await
        else {
            return;
        };
        let Ok((sender, upstream_connection)) =
            hyper::client::conn::http1::handshake(TokioIo::new(upstream)).await
        else {
            return;
        };
        let sender = Arc::new(Mutex::new(sender));
        let pending_upgrade = Arc::new(PendingUpgrade::default());
        let target = Arc::new(target.clone());
        let service = {
            let pending_upgrade = pending_upgrade.clone();
            service_fn(move |request| {
                forward(
                    request,
                    target.clone(),
                    sender.clone(),
                    policy.clone(),
                    pending_upgrade.clone(),
                )
            })
        };
        let server = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(guest), service)
            .with_upgrades();
        let upstream_closed = Notify::new();
        let upstream = async {
            let _ = upstream_connection.with_upgrades().await;
            upstream_closed.notify_one();
        };
        let guest = async {
            tokio::pin!(server);
            tokio::select! {
                biased;
                _ = &mut server => {}
                () = upstream_closed.notified() => {
                    if !is_upgrading(&pending_upgrade) {
                        server.as_mut().graceful_shutdown();
                    }
                    let _ = server.await;
                }
            }
        };
        tokio::join!(upstream, guest);
        let upgrade = pending_upgrade
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((guest, upstream)) = upgrade
            && let (Ok(guest), Ok(upstream)) = tokio::join!(guest, upstream)
        {
            let _ = copy_bidirectional(&mut TokioIo::new(guest), &mut TokioIo::new(upstream)).await;
        }
    }
}

fn is_upgrading(pending_upgrade: &PendingUpgrade) -> bool {
    pending_upgrade
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

async fn forward(
    mut request: Request<Incoming>,
    target: Arc<Host>,
    sender: Arc<Mutex<SendRequest<Incoming>>>,
    policy: watch::Receiver<EgressPolicy>,
    pending_upgrade: Arc<PendingUpgrade>,
) -> Result<Response<Body>, Infallible> {
    if !is_addressed_to(&request, &target) {
        return Ok(plain(
            StatusCode::MISDIRECTED_REQUEST,
            format!("this connection reaches only {target}"),
        ));
    }
    let credential = policy.borrow().credential_headers(&target).cloned();
    for (name, value) in credential.iter().flatten() {
        request.headers_mut().insert(name, value.clone());
    }
    let origin_form = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .parse::<Uri>();
    match origin_form {
        Ok(uri) => *request.uri_mut() = uri,
        Err(_) => {
            return Ok(plain(
                StatusCode::BAD_REQUEST,
                "malformed request target".into(),
            ));
        }
    }
    let guest_upgrade = hyper::upgrade::on(&mut request);
    let mut sender = sender.lock().await;
    if sender.ready().await.is_err() {
        return Ok(plain(
            StatusCode::BAD_GATEWAY,
            format!("{target} closed the connection"),
        ));
    }
    match sender.send_request(request).await {
        Ok(mut response) => {
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                let upstream_upgrade = hyper::upgrade::on(&mut response);
                *pending_upgrade
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((guest_upgrade, upstream_upgrade));
            }
            Ok(response.map(Either::Left))
        }
        Err(error) => Ok(plain(
            StatusCode::BAD_GATEWAY,
            format!("{target} failed: {error}"),
        )),
    }
}

fn is_addressed_to(request: &Request<Incoming>, target: &Host) -> bool {
    let names_target = |authority: &str| {
        let host = authority.strip_suffix(":443").unwrap_or(authority);
        Host::parse(host).as_ref() == Some(target)
    };
    let host_header = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    host_header.is_some_and(names_target)
        && request
            .uri()
            .authority()
            .is_none_or(|authority| names_target(authority.as_str()))
}

fn plain(status: StatusCode, message: String) -> Response<Body> {
    let mut response = Response::new(Either::Right(format!("{message}\n")));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain"),
    );
    response
}

fn authority_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, CA_NAME);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    set_validity(&mut params);
    params
}

fn set_validity(params: &mut CertificateParams) {
    params.not_before = date_time_ymd(2000, 1, 1);
    params.not_after = date_time_ymd(2100, 1, 1);
}

async fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let staged = path.with_extension("tmp");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staged)?;
    let mut file = tokio::fs::File::from_std(file);
    tokio::io::AsyncWriteExt::write_all(&mut file, contents.as_bytes()).await?;
    file.sync_all().await?;
    tokio::fs::rename(&staged, path).await?;
    match path.parent() {
        Some(dir) => crate::files::sync(dir).await,
        None => Ok(()),
    }
}

pub struct Prefixed<S> {
    prefix: Vec<u8>,
    consumed: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            consumed: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let unread = &this.prefix[this.consumed..];
        if unread.is_empty() {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        let count = unread.len().min(buf.remaining());
        buf.put_slice(&unread[..count]);
        this.consumed += count;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
