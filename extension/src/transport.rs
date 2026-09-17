//! Gateway addressing and TLS channel construction, shared by the background
//! worker and per-backend FDW scans. Pure except for reading the CA file and
//! spawning tonic's lazy connector.

use std::fmt;
use std::time::Duration;

use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

/// Where a gateway is and how to verify it. Built from operator configuration
/// (GUCs or `CREATE SERVER` options); never carries credentials.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    /// `https://host[:port]`, normalised (no path/query/userinfo).
    pub endpoint: String,
    /// Host name the gateway's TLS certificate must be valid for.
    pub tls_server_name: String,
    /// PEM CA bundle path used to verify the gateway. `None` = webpki roots.
    pub ca_cert_path: Option<String>,
}

/// Why an endpoint string was rejected. Messages never include secrets: the
/// endpoint is operator configuration, and embedded userinfo is rejected
/// without being echoed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// Endpoint unset or empty.
    Missing,
    /// Not a parseable URL.
    Invalid(String),
    /// Scheme other than `https`.
    Plaintext(String),
    /// URL has no host component.
    NoHost(String),
    /// URL carries `user:password@`.
    UserInfo,
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => write!(f, "gateway endpoint is not set"),
            Self::Invalid(e) => write!(f, "gateway endpoint is not a valid URL: {e}"),
            Self::Plaintext(s) => write!(
                f,
                "gateway endpoint must use https (got scheme {s:?}); plaintext gateway connections are not supported"
            ),
            Self::NoHost(u) => write!(f, "gateway endpoint {u:?} has no host"),
            Self::UserInfo => write!(
                f,
                "gateway endpoint must not embed credentials (user:password@); credentials are configured separately"
            ),
        }
    }
}

impl std::error::Error for TargetError {}

impl Target {
    /// Parses and validates an endpoint. Rejects anything but `https://` with
    /// a host, and any embedded userinfo (`docs/RULES.md` §3). A blank CA path
    /// is treated as unset.
    pub fn parse(endpoint: Option<&str>, ca_cert_path: Option<&str>) -> Result<Self, TargetError> {
        let raw = endpoint
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(TargetError::Missing)?;
        let url = url::Url::parse(raw).map_err(|e| TargetError::Invalid(e.to_string()))?;
        if url.scheme() != "https" {
            return Err(TargetError::Plaintext(url.scheme().to_owned()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(TargetError::UserInfo);
        }
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| TargetError::NoHost(raw.to_owned()))?
            .to_owned();
        Ok(Self {
            endpoint: format!(
                "https://{host}{}",
                url.port().map(|p| format!(":{p}")).unwrap_or_default()
            ),
            tls_server_name: host,
            ca_cert_path: ca_cert_path
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        })
    }
}

/// Errors while turning a [`Target`] into a lazily-connecting gRPC channel.
#[derive(Debug)]
pub enum ChannelError {
    /// The CA file could not be read.
    ReadCa(String, std::io::Error),
    /// tonic rejected the endpoint or TLS configuration.
    Transport(tonic::transport::Error),
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadCa(path, e) => write!(f, "cannot read gateway CA certificate {path:?}: {e}"),
            Self::Transport(e) => write!(f, "cannot configure gateway channel: {e}"),
        }
    }
}

impl std::error::Error for ChannelError {}

/// Builds a lazily-connecting TLS channel for `target` with `timeout` applied
/// to both connect and each request. No connection is attempted here, but the
/// caller must be inside a tokio runtime context (`Runtime::enter`) because
/// tonic spawns its connector task.
/// How often an idle connection sends an HTTP/2 PING, and how long it waits
/// for the acknowledgement before treating the connection as dead.
///
/// These exist because Postgres is deliberately outside the cluster
/// (docs/DESIGN.md), so every connection crosses NAT, stateful firewalls and
/// cloud load balancers. All of those drop an idle flow without sending a FIN
/// or an RST: the socket stays open on both sides and nothing is ever
/// delivered. A watch stream is idle whenever the cluster is quiet, which is
/// most of the time, and is exactly what such a device reaps.
///
/// Without a keepalive that failure surfaces only at OS-level TCP timeout,
/// measured in hours. Throughout, the subscription still reports ACTIVE and
/// scans keep serving the cache as current: stale data presented as fresh,
/// which docs/RULES.md §1 forbids. With one, the connection breaks in under a
/// minute, the stream errors, and the subscription goes DEGRADED, which is the
/// honest state and already announces itself on every scan.
///
/// The interval must stay comfortably above the gateway's minimum enforced
/// interval (see `keepalive.EnforcementPolicy` in `gateway/cmd/gateway/main.go`)
/// or the server answers pings with GOAWAY and kills the connection this is
/// meant to protect.
/// Largest gRPC message in either direction, matching `MaxMessageBytes` in
/// `gateway/internal/server/read.go`.
///
/// Both ends must agree. Raising only the server's limit lets it send a
/// message the client then refuses with `RESOURCE_EXHAUSTED`, which is a
/// confusing way to discover a size problem: the send succeeded.
///
/// tonic defaults to 4 MiB in each direction, which is the ceiling paging
/// exists to stay under, so this is a backstop. The gateway's per-page byte
/// budget is deliberately well below it, because a page's JSON is not the
/// whole message: protobuf framing and each object's namespace, name and
/// resourceVersion ride along on top.
pub const MAX_MESSAGE_BYTES: usize = 16 << 20;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether a channel should keep pinging while it has no active stream.
///
/// This is the difference between the two callers, and it matters because they
/// have opposite shapes.
///
/// A **subscription** channel carries one long-lived stream that is idle
/// whenever the cluster is quiet, which is most of the time. That idle stream
/// is exactly what a middlebox reaps, and there is no next request to discover
/// the loss, so it must ping while idle.
///
/// A **unary** channel is cached per Postgres backend for the life of that
/// backend (see `channel_for` in client.rs). Pinging those while idle would
/// mean every backend that ever ran one FDW query holds a connection open and
/// sends a PING every 30 seconds for as long as the session lasts, scaling
/// with backend count rather than with work. And it would not help: the
/// per-backend runtime is `current_thread`, so between queries nothing polls
/// the connection and no keepalive would be sent however this is configured.
///
/// This comment used to argue that the loss is simply "discovered by the next
/// request, which is the only thing that cares". Issue #51 falsified that. The
/// gateway does not neglect an idle connection, it deliberately closes one
/// whose peer stops answering, so the next request did not discover the loss
/// so much as become it -- and inside a transaction it took the transaction
/// with it. `client.rs` now retires a cached channel on age before offering
/// it, which is where that problem is solved; see `MAX_IDLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keepalive {
    /// Ping only while a request or stream is in flight.
    WhileActive,
    /// Ping even with no active stream. For long-lived subscriptions.
    WhileIdle,
}

pub fn build_channel(target: &Target, timeout: Duration) -> Result<Channel, ChannelError> {
    build_channel_with(target, timeout, Keepalive::WhileActive)
}

/// As [`build_channel`], choosing whether the connection is probed while idle.
pub fn build_channel_with(
    target: &Target,
    timeout: Duration,
    keepalive: Keepalive,
) -> Result<Channel, ChannelError> {
    let mut tls = ClientTlsConfig::new().domain_name(target.tls_server_name.clone());
    tls = match &target.ca_cert_path {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|e| ChannelError::ReadCa(path.clone(), e))?;
            tls.ca_certificate(Certificate::from_pem(pem))
        }
        None => tls.with_webpki_roots(),
    };
    let endpoint = Endpoint::from_shared(target.endpoint.clone())
        .map_err(ChannelError::Transport)?
        .tls_config(tls)
        .map_err(ChannelError::Transport)?
        .connect_timeout(timeout)
        .timeout(timeout)
        .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
        .keep_alive_timeout(KEEPALIVE_TIMEOUT)
        .keep_alive_while_idle(keepalive == Keepalive::WhileIdle);
    Ok(endpoint.connect_lazy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https_with_port() {
        let t = Target::parse(Some("https://gateway:8443"), None).expect("valid");
        assert_eq!(t.endpoint, "https://gateway:8443");
        assert_eq!(t.tls_server_name, "gateway");
        assert_eq!(t.ca_cert_path, None);
    }

    #[test]
    fn strips_path_and_query_and_trims_ca() {
        let t = Target::parse(
            Some(" https://gw.example.com/foo?x=1 "),
            Some(" /etc/ca.pem "),
        )
        .expect("valid");
        assert_eq!(t.endpoint, "https://gw.example.com");
        assert_eq!(t.ca_cert_path.as_deref(), Some("/etc/ca.pem"));
        assert_eq!(
            Target::parse(Some("https://gw"), Some("  "))
                .expect("valid")
                .ca_cert_path,
            None
        );
    }

    #[test]
    fn rejects_missing_plaintext_hostless_userinfo() {
        assert_eq!(Target::parse(None, None), Err(TargetError::Missing));
        assert_eq!(Target::parse(Some("  "), None), Err(TargetError::Missing));
        assert_eq!(
            Target::parse(Some("http://gw"), None),
            Err(TargetError::Plaintext("http".into()))
        );
        assert!(matches!(
            Target::parse(Some("not a url"), None),
            Err(TargetError::Invalid(_))
        ));
        assert!(matches!(
            Target::parse(Some("https://"), None),
            Err(TargetError::Invalid(_) | TargetError::NoHost(_))
        ));
        assert_eq!(
            Target::parse(Some("https://u:p@gw"), None),
            Err(TargetError::UserInfo)
        );
        assert!(!TargetError::UserInfo.to_string().contains("u:p"));
    }

    #[test]
    fn build_channel_reports_missing_ca_file() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _g = rt.enter();
        let t = Target::parse(Some("https://gw"), Some("/definitely/not/here.pem")).expect("valid");
        match build_channel(&t, Duration::from_secs(1)) {
            Err(ChannelError::ReadCa(p, _)) => assert_eq!(p, "/definitely/not/here.pem"),
            other => panic!("expected ReadCa, got {other:?}"),
        }
        let t = Target::parse(Some("https://gw"), None).expect("valid");
        assert!(build_channel(&t, Duration::from_secs(1)).is_ok());
    }
}
