//! Per-backend gRPC client for on-demand FDW scans.
//!
//! Fork-safety: a Postgres backend is forked from the postmaster, so nothing
//! here may be initialised at library load. The tokio runtime and channels
//! are created lazily, in the backend, on first use, and live in
//! thread-locals (a backend is single-threaded). The runtime is
//! `current_thread`, so no helper threads are ever spawned inside a backend.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use tonic::transport::Channel;

use crate::options::ServerOptions;
use crate::proto::v1::gateway_service_client::GatewayServiceClient;
use crate::proto::v1::{GroupVersionKind, ListRequest};
use crate::quals::PodFilter;
use crate::transport::{build_channel, ChannelError, Target};

thread_local! {
    static RUNTIME: RefCell<Option<tokio::runtime::Runtime>> = const { RefCell::new(None) };
    static CHANNELS: RefCell<HashMap<(Target, Duration), Channel>> = RefCell::new(HashMap::new());
}

/// Why a gateway call failed, from the FDW's point of view.
#[derive(Debug)]
pub enum ClientError {
    /// The tokio runtime could not be created.
    Runtime(std::io::Error),
    /// The channel could not be configured (bad CA path etc.).
    Channel(ChannelError),
    /// The RPC itself failed. Boxed: `Status` is large and this is the cold path.
    Rpc(Box<tonic::Status>),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "cannot create async runtime: {e}"),
            Self::Channel(e) => write!(f, "{e}"),
            Self::Rpc(s) => write!(f, "gateway returned {:?}: {}", s.code(), s.message()),
        }
    }
}

impl std::error::Error for ClientError {}

/// Coarse classification used to pick a SQLSTATE. Pure; unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Gateway unreachable, timed out, or channel misconfigured.
    Connection,
    /// The gateway's RBAC identity may not read the resource.
    Permission,
    /// We sent something the gateway rejected (should not happen after local validation).
    InvalidRequest,
    /// Gateway is up but has no cluster configured.
    GatewayUnconfigured,
    /// Anything else.
    Internal,
}

impl ClientError {
    /// Classifies the error for SQLSTATE selection.
    pub fn class(&self) -> ErrorClass {
        use tonic::Code;
        match self {
            Self::Runtime(_) => ErrorClass::Internal,
            Self::Channel(_) => ErrorClass::Connection,
            Self::Rpc(s) => match s.code() {
                Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Unknown => {
                    ErrorClass::Connection
                }
                Code::PermissionDenied | Code::Unauthenticated => ErrorClass::Permission,
                Code::InvalidArgument | Code::NotFound => ErrorClass::InvalidRequest,
                Code::FailedPrecondition => ErrorClass::GatewayUnconfigured,
                _ => ErrorClass::Internal,
            },
        }
    }
}

/// Runs `f` inside this backend's runtime, creating it on first use.
fn with_runtime<T>(f: impl FnOnce(&tokio::runtime::Runtime) -> T) -> Result<T, ClientError> {
    RUNTIME.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(ClientError::Runtime)?;
            *slot = Some(rt);
        }
        // `slot` was just filled if it was empty; the `?` above returned otherwise.
        match slot.as_ref() {
            Some(rt) => Ok(f(rt)),
            None => Err(ClientError::Runtime(std::io::Error::other(
                "runtime slot unexpectedly empty",
            ))),
        }
    })
}

/// Returns a channel for `server`, reusing one already open in this backend.
fn channel_for(
    rt: &tokio::runtime::Runtime,
    server: &ServerOptions,
) -> Result<Channel, ClientError> {
    let key = (server.target.clone(), server.rpc_timeout);
    CHANNELS.with(|cell| {
        let mut map = cell.borrow_mut();
        if let Some(ch) = map.get(&key) {
            return Ok(ch.clone());
        }
        let ch = {
            let _guard = rt.enter();
            build_channel(&server.target, server.rpc_timeout).map_err(ClientError::Channel)?
        };
        map.insert(key, ch.clone());
        Ok(ch)
    })
}

/// The Pod kind as the gateway names it.
fn pod_gvk() -> GroupVersionKind {
    GroupVersionKind {
        group: String::new(),
        version: "v1".into(),
        kind: "Pod".into(),
    }
}

/// Lists Pods matching `filter` via one `List` RPC and returns each object's
/// raw JSON. Never called when `filter.impossible` (the caller short-circuits).
///
/// Errors: see [`ClientError`]; a filter that matches nothing is `Ok(vec![])`.
pub fn list_pods(server: &ServerOptions, filter: &PodFilter) -> Result<Vec<Vec<u8>>, ClientError> {
    let timeout = server.rpc_timeout;
    let req = ListRequest {
        gvk: Some(pod_gvk()),
        namespace: filter.namespace.clone().unwrap_or_default(),
        name: filter.name.clone().unwrap_or_default(),
    };
    with_runtime(|rt| {
        let ch = channel_for(rt, server)?;
        let mut client = GatewayServiceClient::new(ch);
        rt.block_on(async {
            match tokio::time::timeout(timeout, client.list(req)).await {
                Ok(Ok(resp)) => Ok(resp
                    .into_inner()
                    .objects
                    .into_iter()
                    .map(|o| o.json)
                    .collect()),
                Ok(Err(status)) => Err(ClientError::Rpc(Box::new(status))),
                Err(_elapsed) => Err(ClientError::Rpc(Box::new(
                    tonic::Status::deadline_exceeded(format!(
                        "no reply within {}s",
                        timeout.as_secs()
                    )),
                ))),
            }
        })
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::{Code, Status};

    #[test]
    fn classifies_rpc_codes() {
        let cases = [
            (Code::Unavailable, ErrorClass::Connection),
            (Code::DeadlineExceeded, ErrorClass::Connection),
            (Code::PermissionDenied, ErrorClass::Permission),
            (Code::Unauthenticated, ErrorClass::Permission),
            (Code::InvalidArgument, ErrorClass::InvalidRequest),
            (Code::FailedPrecondition, ErrorClass::GatewayUnconfigured),
            (Code::Internal, ErrorClass::Internal),
        ];
        for (code, want) in cases {
            assert_eq!(
                ClientError::Rpc(Box::new(Status::new(code, "x"))).class(),
                want,
                "{code:?}"
            );
        }
        let t = Target::parse(Some("https://gw"), Some("/nope")).expect("valid");
        let ch = ClientError::Channel(ChannelError::ReadCa(
            "/nope".into(),
            std::io::Error::other("x"),
        ));
        assert_eq!(ch.class(), ErrorClass::Connection);
        assert!(ch.to_string().contains("/nope"));
        drop(t);
    }

    #[test]
    fn list_against_unreachable_gateway_is_connection_error() {
        let server = ServerOptions::parse(&[
            ("endpoint".to_owned(), "https://127.0.0.1:1".to_owned()),
            ("rpc_timeout_secs".to_owned(), "1".to_owned()),
        ])
        .expect("valid");
        let err = list_pods(&server, &PodFilter::default()).expect_err("nothing listens on port 1");
        assert_eq!(err.class(), ErrorClass::Connection, "{err}");
        // Second call reuses the cached channel and fails the same way.
        let err = list_pods(&server, &PodFilter::default()).expect_err("still unreachable");
        assert_eq!(err.class(), ErrorClass::Connection, "{err}");
    }

    #[test]
    fn missing_ca_file_is_connection_error() {
        let server = ServerOptions::parse(&[
            ("endpoint".to_owned(), "https://127.0.0.1:1".to_owned()),
            ("ca_cert".to_owned(), "/definitely/missing.pem".to_owned()),
        ])
        .expect("valid");
        let err = list_pods(&server, &PodFilter::default()).expect_err("ca missing");
        assert!(
            matches!(err, ClientError::Channel(ChannelError::ReadCa(..))),
            "{err:?}"
        );
        assert_eq!(err.class(), ErrorClass::Connection);
    }
}
