//! Per-backend gRPC client for on-demand FDW scans and writes.
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
use crate::proto::v1::{
    CreateRequest, DeleteRequest, GroupVersionKind, KindSchema, ListKindsRequest, ListRequest,
    UpdateRequest,
};
use crate::quals::Filter;
use crate::resource::Resource;
use crate::table::Identity;
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
    /// Optimistic-concurrency conflict: the object changed since it was read.
    Conflict,
    /// INSERT of an object that already exists.
    AlreadyExists,
    /// UPDATE/DELETE of an object that no longer exists.
    NotFound,
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
                Code::InvalidArgument => ErrorClass::InvalidRequest,
                Code::Aborted => ErrorClass::Conflict,
                Code::AlreadyExists => ErrorClass::AlreadyExists,
                Code::NotFound => ErrorClass::NotFound,
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

/// Number of channels currently cached in this process. Test-only
/// observability for the eviction path: without it a test cannot tell a
/// rebuilt channel from a reused one, since both fail identically against an
/// unreachable gateway.
#[cfg(test)]
pub fn cached_channel_count() -> usize {
    CHANNELS.with(|cell| cell.borrow().len())
}

/// Drops any cached channel for `server`, so the next call rebuilds one.
///
/// A channel carries the TLS trust configuration read when it was built, which
/// means a cached channel outlives the certificate it trusts. When the gateway's
/// certificate is rotated -- routine renewal in a real deployment, and every
/// `compose up` in the local stack -- every call on the stale channel fails with
/// `invalid peer certificate: BadSignature` and keeps failing, because nothing
/// ever rebuilds it. Evicting on a connection-class failure makes the next
/// attempt re-read the CA and recover on its own.
fn forget_channel(server: &ServerOptions) {
    let key = (server.target.clone(), server.rpc_timeout);
    CHANNELS.with(|cell| {
        cell.borrow_mut().remove(&key);
    });
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

/// Builds the wire GVK for a resolved resource. The gateway resolves this to
/// a REST resource through its own discovery, so the plural name is not sent.
fn gvk_of(resource: &Resource) -> GroupVersionKind {
    GroupVersionKind {
        group: resource.group.to_string(),
        version: resource.version.to_string(),
        kind: resource.kind.to_string(),
    }
}

/// Runs one RPC against `server` with the configured deadline, mapping a
/// local timeout to `DeadlineExceeded`.
fn call<T, F, Fut>(server: &ServerOptions, f: F) -> Result<T, ClientError>
where
    F: FnOnce(GatewayServiceClient<Channel>) -> Fut,
    Fut: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    let timeout = server.rpc_timeout;
    with_runtime(|rt| {
        let ch = channel_for(rt, server)?;
        let client = GatewayServiceClient::new(ch);
        let out: Result<T, ClientError> = rt.block_on(async {
            match tokio::time::timeout(timeout, f(client)).await {
                Ok(Ok(resp)) => Ok(resp.into_inner()),
                Ok(Err(status)) => Err(ClientError::Rpc(Box::new(status))),
                Err(_elapsed) => Err(ClientError::Rpc(Box::new(
                    tonic::Status::deadline_exceeded(format!(
                        "no reply within {}s",
                        timeout.as_secs()
                    )),
                ))),
            }
        });
        // A connection-class failure may mean the channel itself is no longer
        // usable -- most often a rotated certificate, which no amount of
        // retrying on the same channel will recover from.
        if let Err(e) = &out {
            if e.class() == ErrorClass::Connection {
                forget_channel(server);
            }
        }
        out
    })?
}

/// Lists objects of `kind` matching `filter` via one `List` RPC and returns
/// each object's raw JSON. Never called when `filter.impossible` (the caller
/// short-circuits). A filter that matches nothing is `Ok(vec![])`.
pub fn list(
    server: &ServerOptions,
    resource: &Resource,
    filter: &Filter,
) -> Result<Vec<Vec<u8>>, ClientError> {
    let req = ListRequest {
        gvk: Some(gvk_of(resource)),
        namespace: filter.namespace.clone().unwrap_or_default(),
        name: filter.name.clone().unwrap_or_default(),
    };
    call(server, |mut c| async move { c.list(req).await })
        .map(|resp| resp.objects.into_iter().map(|o| o.json).collect())
}

/// Creates one object; returns the stored object's JSON (for RETURNING).
pub fn create(
    server: &ServerOptions,
    resource: &Resource,
    id: &Identity,
    body: &serde_json::Value,
) -> Result<Vec<u8>, ClientError> {
    let req = CreateRequest {
        gvk: Some(gvk_of(resource)),
        namespace: id.namespace.clone(),
        name: id.name.clone(),
        json: body.to_string().into_bytes(),
    };
    call(server, |mut c| async move { c.create(req).await })
        .map(|resp| resp.object.map(|o| o.json).unwrap_or_default())
}

/// Replaces one object, sending `id.resource_version` as the concurrency
/// token; a stale token is [`ErrorClass::Conflict`]. Returns the stored JSON.
pub fn update(
    server: &ServerOptions,
    resource: &Resource,
    id: &Identity,
    body: &serde_json::Value,
) -> Result<Vec<u8>, ClientError> {
    let req = UpdateRequest {
        gvk: Some(gvk_of(resource)),
        namespace: id.namespace.clone(),
        name: id.name.clone(),
        resource_version: id.resource_version.clone(),
        json: body.to_string().into_bytes(),
    };
    call(server, |mut c| async move { c.update(req).await })
        .map(|resp| resp.object.map(|o| o.json).unwrap_or_default())
}

/// Enumerates the kinds the gateway serves, for `IMPORT FOREIGN SCHEMA`.
///
/// `group` narrows to one API group when `Some` (the empty string being the
/// core group, which is why this is an `Option` rather than a bare `&str`).
/// `plurals` narrows to an explicit set; a name matching nothing is simply
/// absent from the result rather than an error.
///
/// This is the only RPC the extension makes outside a scan or a write, and it
/// happens at DDL time only: the generated tables carry their identity in
/// options, so no later query depends on discovery.
pub fn list_kinds(
    server: &ServerOptions,
    group: Option<&str>,
    plurals: &[String],
) -> Result<Vec<KindSchema>, ClientError> {
    let req = ListKindsRequest {
        group: group.map(ToOwned::to_owned),
        plurals: plurals.to_vec(),
    };
    call(server, |mut c| async move { c.list_kinds(req).await }).map(|resp| resp.kinds)
}

/// Deletes one object by identity.
pub fn delete(
    server: &ServerOptions,
    resource: &Resource,
    id: &Identity,
) -> Result<(), ClientError> {
    let req = DeleteRequest {
        gvk: Some(gvk_of(resource)),
        namespace: id.namespace.clone(),
        name: id.name.clone(),
    };
    call(server, |mut c| async move { c.delete(req).await }).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pods() -> Resource {
        Resource::new("", "v1", "Pod", "pods", true).expect("valid")
    }
    fn configmaps() -> Resource {
        Resource::new("", "v1", "ConfigMap", "configmaps", true).expect("valid")
    }
    use tonic::{Code, Status};

    #[test]
    fn classifies_rpc_codes() {
        let cases = [
            (Code::Unavailable, ErrorClass::Connection),
            (Code::DeadlineExceeded, ErrorClass::Connection),
            (Code::PermissionDenied, ErrorClass::Permission),
            (Code::Unauthenticated, ErrorClass::Permission),
            (Code::InvalidArgument, ErrorClass::InvalidRequest),
            (Code::Aborted, ErrorClass::Conflict),
            (Code::AlreadyExists, ErrorClass::AlreadyExists),
            (Code::NotFound, ErrorClass::NotFound),
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

    /// The failure this eviction exists for is a rotated certificate: the
    /// channel is a fine socket, but its trust material no longer matches what
    /// the gateway serves, and retrying on it never recovers. Staging a real
    /// rotation needs a live TLS peer, so this covers the mechanism that
    /// recovery depends on -- a connection-class failure leaves no channel
    /// behind, so the next call is guaranteed to rebuild against the current
    /// CA. Removing the eviction makes this fail.
    #[test]
    fn a_connection_failure_leaves_no_channel_to_reuse() {
        let server = ServerOptions::parse(&[
            ("endpoint".to_owned(), "https://127.0.0.1:1".to_owned()),
            ("rpc_timeout_secs".to_owned(), "1".to_owned()),
        ])
        .expect("valid");

        for attempt in 1..=3 {
            let err =
                list(&server, &pods(), &Filter::default()).expect_err("nothing listens on port 1");
            assert_eq!(
                err.class(),
                ErrorClass::Connection,
                "attempt {attempt}: {err}"
            );
            assert_eq!(
                cached_channel_count(),
                0,
                "attempt {attempt}: the channel must not survive a connection failure, \
                 or a rotated certificate would keep failing until the backend restarts"
            );
        }
    }

    #[test]
    fn list_against_unreachable_gateway_is_connection_error() {
        let server = ServerOptions::parse(&[
            ("endpoint".to_owned(), "https://127.0.0.1:1".to_owned()),
            ("rpc_timeout_secs".to_owned(), "1".to_owned()),
        ])
        .expect("valid");
        let err =
            list(&server, &pods(), &Filter::default()).expect_err("nothing listens on port 1");
        assert_eq!(err.class(), ErrorClass::Connection, "{err}");
        // The failure evicted the channel, so the next call rebuilds rather
        // than retrying on one that may hold stale trust material.
        assert_eq!(
            cached_channel_count(),
            0,
            "a connection failure must evict the cached channel"
        );
        let err = list(&server, &pods(), &Filter::default()).expect_err("still unreachable");
        assert_eq!(err.class(), ErrorClass::Connection, "{err}");
        // Writes go through the same path and never succeed silently.
        let id = Identity {
            namespace: "d".into(),
            name: "a".into(),
            resource_version: "1".into(),
        };
        assert_eq!(
            create(&server, &configmaps(), &id, &serde_json::json!({}))
                .expect_err("down")
                .class(),
            ErrorClass::Connection
        );
        assert_eq!(
            update(&server, &configmaps(), &id, &serde_json::json!({}))
                .expect_err("down")
                .class(),
            ErrorClass::Connection
        );
        assert_eq!(
            delete(&server, &configmaps(), &id)
                .expect_err("down")
                .class(),
            ErrorClass::Connection
        );
    }

    #[test]
    fn missing_ca_file_is_connection_error() {
        let server = ServerOptions::parse(&[
            ("endpoint".to_owned(), "https://127.0.0.1:1".to_owned()),
            ("ca_cert".to_owned(), "/definitely/missing.pem".to_owned()),
        ])
        .expect("valid");
        let err = list(&server, &pods(), &Filter::default()).expect_err("ca missing");
        assert!(
            matches!(err, ClientError::Channel(ChannelError::ReadCa(..))),
            "{err:?}"
        );
        assert_eq!(err.class(), ErrorClass::Connection);
    }
}
