//! Pure, Postgres-independent parsing/validation of the bgworker's settings.
//!
//! The impure side (reading GUCs) lives in `bgworker.rs`; this module only sees
//! already-extracted raw values so it is unit-testable with plain `#[test]`s.

use std::fmt;
use std::time::Duration;

/// Validated background-worker settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `https://host:port` of the gateway. Scheme is always `https`.
    pub endpoint: String,
    /// Host name the gateway's TLS certificate must be valid for.
    pub tls_server_name: String,
    /// Path to a PEM CA bundle used to verify the gateway. `None` = system roots.
    pub ca_cert_path: Option<String>,
    /// Interval between `Ping` round-trips.
    pub ping_interval: Duration,
    /// Per-RPC deadline.
    pub rpc_timeout: Duration,
}

/// Why a raw setting was rejected. Messages never include secrets: the
/// endpoint URL is operator configuration, not a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `axiom.gateway_endpoint` is unset or empty.
    MissingEndpoint,
    /// `axiom.gateway_endpoint` is not a parseable URL.
    InvalidEndpoint(String),
    /// `axiom.gateway_endpoint` uses a scheme other than `https`.
    PlaintextEndpoint(String),
    /// URL has no host component.
    EndpointWithoutHost(String),
    /// URL carries user:password@; credentials never travel in the endpoint.
    EndpointWithUserInfo,
    /// Interval or timeout is not strictly positive.
    NonPositiveDuration(&'static str, i64),
    /// RPC timeout is not shorter than the ping interval.
    TimeoutNotBelowInterval,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEndpoint => write!(f, "axiom.gateway_endpoint is not set"),
            Self::InvalidEndpoint(e) => write!(f, "axiom.gateway_endpoint is not a valid URL: {e}"),
            Self::PlaintextEndpoint(s) => write!(
                f,
                "axiom.gateway_endpoint must use https (got scheme {s:?}); plaintext gateway connections are not supported"
            ),
            Self::EndpointWithoutHost(u) => write!(f, "axiom.gateway_endpoint {u:?} has no host"),
            Self::EndpointWithUserInfo => write!(
                f,
                "axiom.gateway_endpoint must not embed credentials (user:password@); credentials are configured separately"
            ),
            Self::NonPositiveDuration(name, v) => write!(f, "{name} must be > 0 (got {v})"),
            Self::TimeoutNotBelowInterval => write!(
                f,
                "axiom.rpc_timeout_secs must be smaller than axiom.ping_interval_secs"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Settings {
    /// Builds validated settings from raw GUC values.
    ///
    /// Rejects: missing/unparseable endpoint, any scheme other than `https`
    /// (no plaintext, ever — `docs/RULES.md` §3), embedded user-info, a
    /// non-positive interval/timeout, and a timeout that is not strictly
    /// smaller than the ping interval (otherwise one slow RPC overlaps the next
    /// tick).
    pub fn from_raw(
        endpoint: Option<&str>,
        ca_cert_path: Option<&str>,
        ping_interval_secs: i64,
        rpc_timeout_secs: i64,
    ) -> Result<Self, ConfigError> {
        let raw = endpoint
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(ConfigError::MissingEndpoint)?;
        let url = url::Url::parse(raw).map_err(|e| ConfigError::InvalidEndpoint(e.to_string()))?;
        if url.scheme() != "https" {
            return Err(ConfigError::PlaintextEndpoint(url.scheme().to_owned()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ConfigError::EndpointWithUserInfo);
        }
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| ConfigError::EndpointWithoutHost(raw.to_owned()))?
            .to_owned();
        if ping_interval_secs <= 0 {
            return Err(ConfigError::NonPositiveDuration(
                "axiom.ping_interval_secs",
                ping_interval_secs,
            ));
        }
        if rpc_timeout_secs <= 0 {
            return Err(ConfigError::NonPositiveDuration(
                "axiom.rpc_timeout_secs",
                rpc_timeout_secs,
            ));
        }
        if rpc_timeout_secs >= ping_interval_secs {
            return Err(ConfigError::TimeoutNotBelowInterval);
        }
        // Both values are > 0 here, so the casts cannot wrap.
        #[allow(clippy::cast_sign_loss, reason = "guarded by the > 0 checks above")]
        let (interval, timeout) = (ping_interval_secs as u64, rpc_timeout_secs as u64);

        Ok(Self {
            // Normalise: drop any path/query, keep scheme://host[:port].
            endpoint: format!(
                "https://{host}{}",
                url.port().map(|p| format!(":{p}")).unwrap_or_default()
            ),
            tls_server_name: host,
            ca_cert_path: ca_cert_path
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            ping_interval: Duration::from_secs(interval),
            rpc_timeout: Duration::from_secs(timeout),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(endpoint: &str) -> Settings {
        Settings::from_raw(Some(endpoint), None, 10, 5).expect("valid")
    }

    #[test]
    fn accepts_https_with_port() {
        let s = ok("https://gateway:8443");
        assert_eq!(s.endpoint, "https://gateway:8443");
        assert_eq!(s.tls_server_name, "gateway");
        assert_eq!(s.ping_interval, Duration::from_secs(10));
        assert_eq!(s.rpc_timeout, Duration::from_secs(5));
        assert_eq!(s.ca_cert_path, None);
    }

    #[test]
    fn strips_path_and_query() {
        assert_eq!(
            ok("https://gw.example.com/foo?x=1").endpoint,
            "https://gw.example.com"
        );
    }

    #[test]
    fn ca_path_is_trimmed_and_blank_means_none() {
        let s =
            Settings::from_raw(Some("https://gw"), Some("  /etc/ca.pem "), 10, 5).expect("valid");
        assert_eq!(s.ca_cert_path.as_deref(), Some("/etc/ca.pem"));
        let s = Settings::from_raw(Some("https://gw"), Some("   "), 10, 5).expect("valid");
        assert_eq!(s.ca_cert_path, None);
    }

    #[test]
    fn rejects_missing_endpoint() {
        assert_eq!(
            Settings::from_raw(None, None, 10, 5),
            Err(ConfigError::MissingEndpoint)
        );
        assert_eq!(
            Settings::from_raw(Some("   "), None, 10, 5),
            Err(ConfigError::MissingEndpoint)
        );
    }

    #[test]
    fn rejects_plaintext() {
        assert_eq!(
            Settings::from_raw(Some("http://gw:80"), None, 10, 5),
            Err(ConfigError::PlaintextEndpoint("http".into()))
        );
        assert_eq!(
            Settings::from_raw(Some("grpc://gw"), None, 10, 5),
            Err(ConfigError::PlaintextEndpoint("grpc".into()))
        );
    }

    #[test]
    fn rejects_garbage_and_hostless() {
        assert!(matches!(
            Settings::from_raw(Some("not a url"), None, 10, 5),
            Err(ConfigError::InvalidEndpoint(_))
        ));
        assert!(matches!(
            Settings::from_raw(Some("https://"), None, 10, 5),
            Err(ConfigError::InvalidEndpoint(_) | ConfigError::EndpointWithoutHost(_))
        ));
        // The url crate normalises `https:///h` to host `h`; make sure that still yields a host.
        assert_eq!(ok("https:///nohost").tls_server_name, "nohost");
    }

    #[test]
    fn rejects_embedded_credentials() {
        assert_eq!(
            Settings::from_raw(Some("https://user:pw@gw"), None, 10, 5),
            Err(ConfigError::EndpointWithUserInfo)
        );
        assert_eq!(
            Settings::from_raw(Some("https://user@gw"), None, 10, 5),
            Err(ConfigError::EndpointWithUserInfo)
        );
    }

    #[test]
    fn rejects_bad_durations() {
        assert_eq!(
            Settings::from_raw(Some("https://gw"), None, 0, 5),
            Err(ConfigError::NonPositiveDuration(
                "axiom.ping_interval_secs",
                0
            ))
        );
        assert_eq!(
            Settings::from_raw(Some("https://gw"), None, 10, -1),
            Err(ConfigError::NonPositiveDuration(
                "axiom.rpc_timeout_secs",
                -1
            ))
        );
        assert_eq!(
            Settings::from_raw(Some("https://gw"), None, 10, 10),
            Err(ConfigError::TimeoutNotBelowInterval)
        );
        assert_eq!(
            Settings::from_raw(Some("https://gw"), None, 5, 10),
            Err(ConfigError::TimeoutNotBelowInterval)
        );
    }

    #[test]
    fn error_messages_do_not_leak_userinfo() {
        let msg = ConfigError::EndpointWithUserInfo.to_string();
        assert!(!msg.contains("pw"), "{msg}");
    }
}
