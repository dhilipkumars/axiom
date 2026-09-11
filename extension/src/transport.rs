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
pub fn build_channel(target: &Target, timeout: Duration) -> Result<Channel, ChannelError> {
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
        .timeout(timeout);
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
