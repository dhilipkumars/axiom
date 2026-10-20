//! `axiom_pod_log`: one container's log, from SQL (#105).
//!
//! A function rather than a foreign table. A log exists only per pod, so a
//! table would need mandatory quals and parameterized paths before a join
//! could reach it, while a function already joins through `LATERAL`:
//!
//! ```sql
//! SELECT p.name, l.line
//!   FROM k8s.core_pods p,
//!        LATERAL axiom_pod_log('k8s', p.namespace, p.name, previous => true) l
//!  WHERE p.phase = 'Failed';
//! ```
//!
//! Nothing here is cached; every call is one live read through the gateway.

use pgrx::prelude::*;

use crate::client;
use crate::fdw::{client_sqlstate, raise};
use crate::proto::v1::PodLogRequest;
use crate::timestamp;

/// The most bytes of log text one call may ask for. The gateway enforces the
/// same ceiling (`MaxLogBytes` in `gateway/internal/server/podlog.go`); it is
/// checked here too so a bad argument fails before a connection is opened.
pub const MAX_LOG_BYTES: i32 = 8 << 20;

/// The most lines one call may ask for, as `maxLogTailLines` in the gateway.
pub const MAX_TAIL_LINES: i32 = 100_000;

/// Reads the newest lines of one container's log.
///
/// `container` NULL reads the container `kubectl logs` would: the one the
/// `kubectl.kubernetes.io/default-container` annotation names, otherwise the
/// first. `since` keeps only lines at or after that time, to the second.
/// `previous` reads the instance before the container's last restart, which is
/// where a crash's output is.
///
/// At most `tail_lines` lines and `max_bytes` bytes of text come back. When the
/// window holds more, the newest bytes are kept and every row has `truncated`
/// set, so a query can tell a cut result from a complete one. A container that
/// has not started, a `previous` read of one that never restarted, or an
/// instance Kubernetes has already removed returns no rows rather than an
/// error, so one such pod does not fail a query over many.
///
/// Requires `USAGE` on the server, like `axiom_gateway_stats`, and is not
/// executable by `PUBLIC`: logs often hold secrets. A role is given scoped
/// access through a `SECURITY DEFINER` wrapper that fixes the server and the
/// namespace; see the agent-access guide.
///
/// Raises if the server does not exist or its gateway does not serve logs
/// (`--serve-pod-logs`), the pod does not exist, or the gateway cannot be
/// reached.
#[pg_extern]
#[allow(clippy::type_complexity, reason = "pgrx TableIterator column tuple")]
#[allow(
    clippy::too_many_arguments,
    reason = "a SQL function's parameters, each with a default"
)]
#[allow(
    clippy::needless_pass_by_value,
    reason = "pgrx passes SQL arguments by value"
)]
fn axiom_pod_log(
    // Every argument is an Option, though only `container` and `since` mean
    // anything by NULL. The function has defaults, so it is not STRICT, and a
    // NULL reaching a bare Rust type would fail inside pgrx's glue with a
    // message that names nothing. `required` says which argument it was.
    server: Option<&str>,
    namespace: Option<&str>,
    pod: Option<&str>,
    container: default!(Option<&str>, "NULL"),
    tail_lines: default!(Option<i32>, 100),
    since: default!(Option<TimestampWithTimeZone>, "NULL"),
    previous: default!(Option<bool>, false),
    max_bytes: default!(Option<i32>, 1_048_576),
) -> TableIterator<
    'static,
    (
        name!(container_name, String),
        name!(line_no, i32),
        name!(ts, Option<TimestampWithTimeZone>),
        name!(line, String),
        name!(truncated, bool),
    ),
> {
    let server = required("server", server);
    let namespace = required("namespace", namespace);
    let pod = required("pod", pod);
    let tail_lines = required("tail_lines", tail_lines);
    let previous = required("previous", previous);
    let max_bytes = required("max_bytes", max_bytes);
    if !(1..=MAX_TAIL_LINES).contains(&tail_lines) {
        raise(
            PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            format!("axiom_pod_log: tail_lines must be between 1 and {MAX_TAIL_LINES}, got {tail_lines}"),
        );
    }
    if !(1..=MAX_LOG_BYTES).contains(&max_bytes) {
        raise(
            PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            format!(
                "axiom_pod_log: max_bytes must be between 1 and {MAX_LOG_BYTES}, got {max_bytes}"
            ),
        );
    }
    let since_time = since.map(|t| {
        let Some((seconds, nanos)) = timestamp::to_unix(t.into()) else {
            raise(
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
                "axiom_pod_log: since must be a point in time, not infinity".to_owned(),
            )
        };
        prost_types::Timestamp { seconds, nanos }
    });

    let opts = match crate::fdw::server_options_by_name(server) {
        Ok(o) => o,
        Err(e) => pgrx::error!("axiom_pod_log: {e}"),
    };
    let req = PodLogRequest {
        namespace: namespace.to_owned(),
        pod: pod.to_owned(),
        container: container.unwrap_or_default().to_owned(),
        tail_lines: i64::from(tail_lines),
        since_time,
        previous,
        max_bytes: i64::from(max_bytes),
    };
    let resp = match client::pod_log(&opts, req) {
        Ok(r) => r,
        Err(e) => raise(
            client_sqlstate(&e),
            format!("axiom_pod_log: {}: {e}", opts.target.endpoint),
        ),
    };

    let container_name = resp.container;
    let truncated = resp.truncated;
    TableIterator::new(resp.lines.into_iter().enumerate().map(move |(i, l)| {
        let ts = l
            .time
            .and_then(|t| timestamp::from_unix(t.seconds, t.nanos))
            .and_then(|micros| TimestampWithTimeZone::try_from(micros).ok());
        // The gateway already drops NULs; a text value cannot hold one, so a
        // gateway that did not is not trusted to have (docs/RULES.md §3).
        let line = if l.text.contains('\0') {
            l.text.replace('\0', "")
        } else {
            l.text
        };
        (
            container_name.clone(),
            i32::try_from(i + 1).unwrap_or(i32::MAX),
            ts,
            line,
            truncated,
        )
    }))
}

/// Unwraps an argument that has no meaning as NULL, or raises naming it.
fn required<T>(name: &str, v: Option<T>) -> T {
    match v {
        Some(v) => v,
        None => raise(
            PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
            format!("axiom_pod_log: {name} must not be NULL"),
        ),
    }
}

// Not callable by every role: logs often hold secrets, and the function reads
// any namespace the gateway can. A DBA grants it to a trusted role, which
// wraps it per tenant (docs/guides/agent-access.md).
extension_sql!(
    "REVOKE EXECUTE ON FUNCTION axiom_pod_log(text, text, text, text, integer, timestamp with time zone, boolean, integer) FROM PUBLIC;",
    name = "axiom_pod_log_not_public",
    requires = [axiom_pod_log],
);
