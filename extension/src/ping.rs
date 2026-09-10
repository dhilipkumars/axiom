//! Pure classification of a `Ping` round-trip result into a log line and a
//! success/failure verdict. Kept separate from the tonic call so it is
//! unit-testable without a network.

use crate::proto::v1::PingResponse;

/// Result of evaluating a `Ping` reply against the request that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PingOutcome {
    /// Reply received and nonce matched.
    Ok {
        /// Version string reported by the gateway.
        gateway_version: String,
    },
    /// Reply received but the nonce did not match: treat as a failure.
    NonceMismatch {
        /// Nonce we sent.
        sent: u64,
        /// Nonce the gateway echoed.
        got: u64,
    },
    /// RPC failed. `code` is the gRPC status code name, `message` the status
    /// message as sent by the peer (already sanitised: tonic strips headers).
    RpcError {
        /// gRPC status code, e.g. `Unavailable`.
        code: String,
        /// Status message.
        message: String,
    },
}

impl PingOutcome {
    /// Classifies a tonic result for the given `sent` nonce.
    pub fn classify(sent: u64, result: Result<PingResponse, tonic::Status>) -> Self {
        match result {
            Ok(resp) if resp.nonce == sent => Self::Ok {
                gateway_version: resp.gateway_version,
            },
            Ok(resp) => Self::NonceMismatch {
                sent,
                got: resp.nonce,
            },
            Err(status) => Self::RpcError {
                code: format!("{:?}", status.code()),
                message: status.message().to_owned(),
            },
        }
    }

    /// `true` only for [`PingOutcome::Ok`].
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    /// One-line, stable-prefix log message. The E2E test greps for the
    /// `axiom bgworker: ping ok` prefix, so keep it stable.
    pub fn log_line(&self, endpoint: &str) -> String {
        match self {
            Self::Ok { gateway_version } => {
                format!(
                    "axiom bgworker: ping ok endpoint={endpoint} gateway_version={gateway_version}"
                )
            }
            Self::NonceMismatch { sent, got } => {
                format!("axiom bgworker: ping failed endpoint={endpoint} reason=nonce_mismatch sent={sent} got={got}")
            }
            Self::RpcError { code, message } => {
                format!("axiom bgworker: ping failed endpoint={endpoint} reason=rpc_error code={code} message={message:?}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(nonce: u64) -> PingResponse {
        PingResponse {
            nonce,
            gateway_version: "1.2.3".into(),
            server_time: None,
        }
    }

    #[test]
    fn ok_when_nonce_matches() {
        let o = PingOutcome::classify(7, Ok(resp(7)));
        assert_eq!(
            o,
            PingOutcome::Ok {
                gateway_version: "1.2.3".into()
            }
        );
        assert!(o.is_success());
        assert_eq!(
            o.log_line("https://gw:8443"),
            "axiom bgworker: ping ok endpoint=https://gw:8443 gateway_version=1.2.3"
        );
    }

    #[test]
    fn mismatch_is_failure() {
        let o = PingOutcome::classify(7, Ok(resp(8)));
        assert_eq!(o, PingOutcome::NonceMismatch { sent: 7, got: 8 });
        assert!(!o.is_success());
        assert!(o
            .log_line("e")
            .contains("reason=nonce_mismatch sent=7 got=8"));
    }

    #[test]
    fn rpc_error_is_failure_with_code() {
        let o = PingOutcome::classify(1, Err(tonic::Status::unavailable("connection refused")));
        assert_eq!(
            o,
            PingOutcome::RpcError {
                code: "Unavailable".into(),
                message: "connection refused".into()
            }
        );
        assert!(!o.is_success());
        let line = o.log_line("e");
        assert!(line.starts_with("axiom bgworker: ping failed"), "{line}");
        assert!(line.contains("code=Unavailable"), "{line}");
    }
}
