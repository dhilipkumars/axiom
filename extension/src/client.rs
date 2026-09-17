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
use tonic::Code;

use crate::options::ServerOptions;
use crate::proto::v1::gateway_service_client::GatewayServiceClient;
use crate::proto::v1::{
    CreateRequest, DeleteRequest, GroupVersionKind, KindSchema, ListKindsRequest, ListRequest,
    StatsRequest, StatsResponse, UpdateRequest,
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
    /// Whether this is a deadline, from either side of the call.
    ///
    /// Two different things produce one: the gateway (or the API server behind
    /// it) exceeding a deadline, and this client's own
    /// `tokio::time::timeout`, which reports `DeadlineExceeded: no reply
    /// within Ns`. Matching on the message would miss the second, whose text
    /// contains neither "timeout" nor "timed out", and that is the common case
    /// -- it is what an `rpc_timeout_secs` that is too short actually looks
    /// like.
    pub fn is_deadline(&self) -> bool {
        match self {
            Self::Rpc(s) => {
                s.code() == tonic::Code::DeadlineExceeded
                    || (s.code() == tonic::Code::Cancelled
                        && s.message().to_lowercase().contains("timeout"))
            }
            _ => false,
        }
    }

    /// Classifies the error for SQLSTATE selection.
    pub fn class(&self) -> ErrorClass {
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

/// Whether a call may be sent a second time on a fresh connection.
///
/// Reads may: repeating one costs a round trip and nothing else. Writes may
/// not, and the reason is that a transport error does not say whether the
/// server saw the request. A replayed `create` answers `AlreadyExists` and a
/// replayed `update` answers `ABORTED` on the stale resourceVersion, so those
/// are at least detectable -- but a replayed `delete` answers `NotFound`,
/// which is indistinguishable from "it was never there" and would report a
/// successful deletion as a failure. Failing a write once, honestly, beats
/// guessing which of those happened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Replay {
    Safe,
    Unsafe,
}

/// Returns a channel for `server`, and whether it came from the cache.
///
/// The caller needs to know: a *reused* channel may have been closed by the
/// gateway while this backend sat idle, which is not a failure of the request
/// that discovers it. A freshly built one that fails has failed for real.
fn channel_for(
    rt: &tokio::runtime::Runtime,
    server: &ServerOptions,
) -> Result<(Channel, bool), ClientError> {
    let key = (server.target.clone(), server.rpc_timeout);
    CHANNELS.with(|cell| {
        let mut map = cell.borrow_mut();
        if let Some(ch) = map.get(&key) {
            return Ok((ch.clone(), true));
        }
        let ch = {
            let _guard = rt.enter();
            build_channel(&server.target, server.rpc_timeout).map_err(ClientError::Channel)?
        };
        map.insert(key, ch.clone());
        Ok((ch, false))
    })
}

/// Whether a failed call should be tried once more on a rebuilt channel.
///
/// Narrow on purpose. The condition being recovered from is a cached
/// connection the gateway has already closed, so all three must hold: the
/// channel was reused, the call is replay-safe, and the status is one that a
/// closed connection actually produces.
///
/// `DeadlineExceeded` is excluded even though it is connection-class. It means
/// the caller's own `rpc_timeout_secs` ran out, so the request may well be in
/// flight and being worked on; repeating it would double the load and spend
/// the budget twice. `Cancelled` is excluded for the same reason -- this
/// module raises it for its own local timeout.
fn should_retry(err: &ClientError, reused: bool, replay: Replay) -> bool {
    if !reused || replay != Replay::Safe {
        return false;
    }
    match err {
        // A channel that failed to *build* is not a stale connection, and
        // rebuilding it again would fail the same way.
        ClientError::Channel(_) | ClientError::Runtime(_) => false,
        ClientError::Rpc(s) => matches!(s.code(), Code::Unavailable | Code::Unknown),
    }
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
fn call<T, F, Fut>(server: &ServerOptions, replay: Replay, f: F) -> Result<T, ClientError>
where
    F: Fn(GatewayServiceClient<Channel>) -> Fut,
    Fut: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    let timeout = server.rpc_timeout;
    with_runtime(|rt| {
        let (out, reused) = attempt(rt, server, timeout, &f)?;
        // A connection-class failure may mean the channel itself is no longer
        // usable -- most often a rotated certificate, which no amount of
        // retrying on the same channel will recover from.
        let Err(e) = &out else { return out };
        if e.class() != ErrorClass::Connection {
            return out;
        }
        forget_channel(server);

        // The gateway pings its peers every 30s and drops them 10s later
        // (gateway/internal/server/keepalive.go). A unary channel here is
        // built `WhileActive` on a current-thread runtime, so between queries
        // nothing polls the connection and the ping goes unanswered: after
        // ~40s idle the gateway closes a connection this backend still has
        // cached. Without this, discovering that costs a user's query -- and
        // inside a transaction, their transaction.
        if !should_retry(e, reused, replay) {
            return out;
        }
        let (retried, _) = attempt(rt, server, timeout, &f)?;
        if let Err(e) = &retried {
            if e.class() == ErrorClass::Connection {
                forget_channel(server);
            }
        }
        retried
    })?
}

/// One attempt, reporting whether the channel it used was a cached one.
fn attempt<T, F, Fut>(
    rt: &tokio::runtime::Runtime,
    server: &ServerOptions,
    timeout: std::time::Duration,
    f: &F,
) -> Result<(Result<T, ClientError>, bool), ClientError>
where
    F: Fn(GatewayServiceClient<Channel>) -> Fut,
    Fut: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    let (ch, reused) = channel_for(rt, server)?;
    let client = GatewayServiceClient::new(ch)
        .max_decoding_message_size(crate::transport::MAX_MESSAGE_BYTES)
        .max_encoding_message_size(crate::transport::MAX_MESSAGE_BYTES);
    let out: Result<T, ClientError> = rt.block_on(async {
        match tokio::time::timeout(timeout, f(client)).await {
            Ok(Ok(resp)) => Ok(resp.into_inner()),
            Ok(Err(status)) => Err(ClientError::Rpc(Box::new(status))),
            Err(_elapsed) => Err(ClientError::Rpc(Box::new(
                tonic::Status::deadline_exceeded(format!("no reply within {}s", timeout.as_secs())),
            ))),
        }
    });
    Ok((out, reused))
}

/// One page of a listing: the objects' raw JSON, and where to resume.
pub struct Page {
    /// Each object's raw JSON, in the order the API server returned them.
    pub objects: Vec<Vec<u8>>,
    /// Empty when this was the last page; otherwise pass it back to continue.
    pub continue_token: String,
}

/// Lists one page of objects of `kind` matching `filter`.
///
/// `continue_token` is empty to start a listing, or the previous page's token
/// to resume. Never called when `filter.impossible` (the caller
/// short-circuits). A filter that matches nothing is an empty page.
///
/// A scan reads a collection over several of these rather than one response,
/// because a whole collection does not fit a gRPC message on any cluster of
/// size: a few hundred Pods carrying managedFields exceeds the 4 MiB default.
pub fn list_page(
    server: &ServerOptions,
    resource: &Resource,
    filter: &Filter,
    continue_token: &str,
) -> Result<Page, ClientError> {
    let req = ListRequest {
        gvk: Some(gvk_of(resource)),
        namespace: filter.namespace.clone().unwrap_or_default(),
        name: filter.name.clone().unwrap_or_default(),
        // Zero: the gateway picks, and clamps anything a caller asks for. The
        // extension has no basis for a better number than the side that knows
        // how big the objects are.
        limit: 0,
        continue_token: continue_token.to_owned(),
    };
    call(server, Replay::Safe, |mut c| {
        let req = req.clone();
        async move { c.list(req).await }
    })
    .map(|resp| Page {
        objects: resp.objects.into_iter().map(|o| o.json).collect(),
        continue_token: resp.continue_token,
    })
}

/// Lists every object of `kind` matching `filter`, following continuations.
///
/// For callers that need the whole collection in hand rather than a cursor,
/// such as the import path. A scan uses [`list_page`] so it can return rows to
/// the executor without buffering everything first.
pub fn list(
    server: &ServerOptions,
    resource: &Resource,
    filter: &Filter,
) -> Result<Vec<Vec<u8>>, ClientError> {
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let page = list_page(server, resource, filter, &token)?;
        out.extend(page.objects);
        if page.continue_token.is_empty() {
            return Ok(out);
        }
        token = page.continue_token;
    }
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
    call(server, Replay::Unsafe, |mut c| {
        let req = req.clone();
        async move { c.create(req).await }
    })
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
    call(server, Replay::Unsafe, |mut c| {
        let req = req.clone();
        async move { c.update(req).await }
    })
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
    call(server, Replay::Safe, |mut c| {
        let req = req.clone();
        async move { c.list_kinds(req).await }
    })
    .map(|resp| resp.kinds)
}

/// Fetches one gateway's counters.
///
/// Cheap and side-effect free on the gateway, so it is safe to poll. Counters
/// are per gateway *process* and reset when it restarts, which is what makes
/// "since you came back, how much work have you done" expressible — see the
/// `Stats` contract in axiom.proto.
pub fn stats(server: &ServerOptions) -> Result<StatsResponse, ClientError> {
    call(server, Replay::Safe, |mut c| async move {
        c.stats(StatsRequest {}).await
    })
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
    call(server, Replay::Unsafe, |mut c| {
        let req = req.clone();
        async move { c.delete(req).await }
    })
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(code: Code) -> ClientError {
        ClientError::Rpc(Box::new(tonic::Status::new(code, "x")))
    }

    /// The gateway closes an idle connection this backend still has cached
    /// (issue #51), so the request that discovers it is not the request that
    /// broke. Retrying it once is the difference between a transaction
    /// surviving a 40-second pause and losing its work.
    #[test]
    fn a_reused_channel_closed_while_idle_is_retried() {
        for code in [Code::Unavailable, Code::Unknown] {
            assert!(
                should_retry(&rpc(code), true, Replay::Safe),
                "{code:?} on a reused channel is the idle-close signature"
            );
        }
    }

    /// All three conditions are load-bearing, so each is removed on its own.
    #[test]
    fn nothing_else_is_retried() {
        // A channel built fresh for this call cannot have been reaped while
        // idle, so its failure is real and repeating it just fails twice.
        assert!(!should_retry(&rpc(Code::Unknown), false, Replay::Safe));

        // A write may already have been applied; the transport error does not
        // say. `delete` is the one that cannot even be detected on replay.
        assert!(!should_retry(&rpc(Code::Unknown), true, Replay::Unsafe));

        // The caller's rpc_timeout_secs ran out. The request may be in flight
        // and being worked on, so a second one doubles the load and spends
        // the budget twice.
        assert!(!should_retry(
            &rpc(Code::DeadlineExceeded),
            true,
            Replay::Safe
        ));
        assert!(!should_retry(&rpc(Code::Cancelled), true, Replay::Safe));

        // Not connection-class at all.
        for code in [Code::NotFound, Code::PermissionDenied, Code::Aborted] {
            assert!(!should_retry(&rpc(code), true, Replay::Safe), "{code:?}");
        }

        // A channel that failed to build is not a stale connection, and
        // building it again fails the same way.
        assert!(!should_retry(
            &ClientError::Channel(ChannelError::ReadCa(
                "/nonexistent".into(),
                std::io::Error::from(std::io::ErrorKind::NotFound),
            )),
            true,
            Replay::Safe
        ));
    }

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

    /// Both shapes of deadline are recognised.
    ///
    /// The client's own timeout is the one that matters and the one a message
    /// match would miss: `tokio::time::timeout` produces
    /// `DeadlineExceeded: no reply within Ns`, whose text contains neither
    /// "timeout" nor "timed out". That is what an `rpc_timeout_secs` set too
    /// low actually looks like, so matching on prose would have left the
    /// IMPORT hint firing only for the rarer server-side case.
    #[test]
    fn deadlines_are_recognised_by_code_not_by_wording() {
        let ours = ClientError::Rpc(Box::new(tonic::Status::deadline_exceeded(
            "no reply within 30s",
        )));
        assert!(ours.is_deadline(), "the client's own timeout must count");

        let theirs = ClientError::Rpc(Box::new(tonic::Status::cancelled("Timeout expired")));
        assert!(theirs.is_deadline(), "a cancelled-with-timeout must count");

        let other = ClientError::Rpc(Box::new(tonic::Status::unavailable("dns error")));
        assert!(
            !other.is_deadline(),
            "an unreachable gateway is not a deadline"
        );

        let denied = ClientError::Rpc(Box::new(tonic::Status::permission_denied("nope")));
        assert!(!denied.is_deadline());
    }
}
