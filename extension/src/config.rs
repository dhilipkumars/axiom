//! Pure, Postgres-independent parsing/validation of the bgworker's settings.
//!
//! The impure side (reading GUCs) lives in `bgworker.rs`; this module only sees
//! already-extracted raw values so it is unit-testable with plain `#[test]`s.

use std::fmt;
use std::time::Duration;

use crate::transport::{Target, TargetError};

/// Validated background-worker settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Gateway address and TLS verification settings.
    pub target: Target,
    /// Interval between `Ping` round-trips.
    pub ping_interval: Duration,
    /// Per-RPC deadline.
    pub rpc_timeout: Duration,
}

/// Why a raw setting was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `axiom.gateway_endpoint` / `axiom.gateway_ca_cert` are invalid.
    Endpoint(TargetError),
    /// Interval or timeout is not strictly positive.
    NonPositiveDuration(&'static str, i64),
    /// RPC timeout is not shorter than the ping interval.
    TimeoutNotBelowInterval,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Endpoint(e) => write!(f, "axiom.gateway_endpoint: {e}"),
            Self::NonPositiveDuration(name, v) => write!(f, "{name} must be > 0 (got {v})"),
            Self::TimeoutNotBelowInterval => {
                write!(
                    f,
                    "axiom.rpc_timeout_secs must be smaller than axiom.ping_interval_secs"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Settings {
    /// Builds validated settings from raw GUC values.
    ///
    /// Rejects everything [`Target::parse`] rejects, a non-positive
    /// interval/timeout, and a timeout that is not strictly smaller than the
    /// ping interval (otherwise one slow RPC overlaps the next tick).
    pub fn from_raw(
        endpoint: Option<&str>,
        ca_cert_path: Option<&str>,
        ping_interval_secs: i64,
        rpc_timeout_secs: i64,
    ) -> Result<Self, ConfigError> {
        let target = Target::parse(endpoint, ca_cert_path).map_err(ConfigError::Endpoint)?;
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
        #[allow(clippy::cast_sign_loss, reason = "guarded by the > 0 checks above")]
        let (interval, timeout) = (ping_interval_secs as u64, rpc_timeout_secs as u64);
        Ok(Self {
            target,
            ping_interval: Duration::from_secs(interval),
            rpc_timeout: Duration::from_secs(timeout),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid() {
        let s = Settings::from_raw(Some("https://gateway:8443"), None, 10, 5).expect("valid");
        assert_eq!(s.target.endpoint, "https://gateway:8443");
        assert_eq!(s.ping_interval, Duration::from_secs(10));
        assert_eq!(s.rpc_timeout, Duration::from_secs(5));
    }

    #[test]
    fn wraps_endpoint_errors() {
        assert_eq!(
            Settings::from_raw(None, None, 10, 5),
            Err(ConfigError::Endpoint(TargetError::Missing))
        );
        assert_eq!(
            Settings::from_raw(Some("http://gw"), None, 10, 5),
            Err(ConfigError::Endpoint(TargetError::Plaintext("http".into())))
        );
        assert!(ConfigError::Endpoint(TargetError::Missing)
            .to_string()
            .starts_with("axiom.gateway_endpoint:"));
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
}
