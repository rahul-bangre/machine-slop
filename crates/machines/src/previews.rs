use std::{
    future::Future,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::sync::watch;

use crate::{contract::PreviewInfo, machine::MachineUse};

pub const DEFAULT_PREVIEW_TTL_SECS: u32 = 3600;
pub const MAX_PREVIEW_TTL_SECS: u32 = 7 * 24 * 3600;
pub const MAX_PREVIEWS_PER_MACHINE: usize = 64;
pub const MAX_PREVIEW_CONNECTIONS: usize = 1024;
pub const PREVIEW_FILES_PER_CONNECTION: u64 = 2;

#[derive(Clone, Debug)]
pub struct PreviewOrigin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl PreviewOrigin {
    pub fn parse(url: &str) -> Result<Self, String> {
        let invalid = || {
            format!(
                "preview URL {url:?} must be http:// or https:// followed by a hostname and optional port"
            )
        };
        let (scheme, rest) = url.split_once("://").ok_or_else(invalid)?;
        if !matches!(scheme, "http" | "https") {
            return Err(invalid());
        }
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port.parse().map_err(|_| invalid())?)),
            None => (authority, None),
        };
        match crate::egress::Host::parse(host) {
            Some(crate::egress::Host::Name(host)) => Ok(Self {
                scheme: scheme.to_owned(),
                host,
                port,
            }),
            _ => Err(invalid()),
        }
    }

    fn url_for(&self, id: &str) -> String {
        let port = self.port.map(|port| format!(":{port}")).unwrap_or_default();
        format!("{}://{id}.{}{port}/", self.scheme, self.host)
    }

    pub fn preview_id<'a>(&self, host_header: &'a str) -> Option<&'a str> {
        let host = match host_header.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => host,
            _ => host_header,
        };
        let host = host.strip_suffix('.').unwrap_or(host);
        let (id, domain) = host.split_once('.')?;
        domain.eq_ignore_ascii_case(&self.host).then_some(id)
    }
}

pub(crate) struct Preview {
    pub(crate) machine_id: String,
    port: u16,
    expires_unix_ms: u64,
    ends: watch::Sender<()>,
}

impl Preview {
    pub(crate) fn new(machine_id: String, port: u16, ttl_secs: u32) -> Self {
        Self {
            machine_id,
            port,
            expires_unix_ms: now_unix_ms().saturating_add(u64::from(ttl_secs) * 1000),
            ends: watch::Sender::new(()),
        }
    }

    pub(crate) fn has_expired(&self) -> bool {
        now_unix_ms() >= self.expires_unix_ms
    }

    pub(crate) fn info(&self, id: &str, origin: &PreviewOrigin) -> PreviewInfo {
        PreviewInfo {
            id: id.to_owned(),
            machine_id: self.machine_id.clone(),
            port: self.port,
            url: origin.url_for(id),
            expires_unix_ms: self.expires_unix_ms,
        }
    }

    pub(crate) fn begin_use(&self, machine: MachineUse) -> PreviewUse {
        let remaining = self.expires_unix_ms.saturating_sub(now_unix_ms());
        PreviewUse {
            machine,
            port: self.port,
            deadline: tokio::time::Instant::now() + Duration::from_millis(remaining),
            ends: self.ends.subscribe(),
        }
    }
}

pub struct PreviewUse {
    pub machine: MachineUse,
    pub port: u16,
    deadline: tokio::time::Instant,
    ends: watch::Receiver<()>,
}

impl PreviewUse {
    pub fn ended(&self) -> impl Future<Output = ()> + Send + 'static {
        let deadline = self.deadline;
        let mut ends = self.ends.clone();
        async move {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {}
                () = async { while ends.changed().await.is_ok() {} } => {}
            }
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_id_is_the_first_label_under_the_origin_host() {
        let origin = PreviewOrigin::parse("https://Preview.Example.com:8443/").expect("origin");
        assert_eq!(
            origin.url_for("abc"),
            "https://abc.preview.example.com:8443/"
        );
        assert_eq!(origin.preview_id("abc.preview.example.com"), Some("abc"));
        assert_eq!(
            origin.preview_id("abc.PREVIEW.example.com:8443"),
            Some("abc")
        );
        assert_eq!(origin.preview_id("preview.example.com"), None);
        assert_eq!(origin.preview_id("abc.other.example.com"), None);
        assert_eq!(origin.preview_id("abc.def.preview.example.com"), None);
    }

    #[test]
    fn preview_origins_must_be_http_urls_with_a_hostname() {
        for url in [
            "ftp://example.com",
            "example.com",
            "https://1.2.3.4",
            "https://example.com:x",
        ] {
            assert!(PreviewOrigin::parse(url).is_err(), "{url}");
        }
    }
}
