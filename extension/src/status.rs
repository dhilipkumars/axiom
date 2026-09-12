//! SQL-visible view of the watch subscriptions (consistency tiers,
//! docs/DESIGN.md §5.3).

use pgrx::prelude::*;

use crate::shmem::{self, ShmemError};

/// One row per watch subscription: which gateway/kind/namespace it covers,
/// its state (`REQUESTED`, `RESYNCING`, `ACTIVE`, `DEGRADED`), how many live
/// objects the cache holds, the resume bookmark, seconds since the last stream
/// event and since the state was entered, and a reason. Returns no rows when
/// the extension is not preloaded (the cache does not exist then).
#[pg_extern]
#[allow(clippy::type_complexity, reason = "pgrx TableIterator column tuple")]
fn axiom_watch_status() -> TableIterator<
    'static,
    (
        name!(server, String),
        name!(resource, String),
        name!(namespace, String),
        name!(state, String),
        name!(objects, i64),
        name!(resource_version, String),
        name!(last_event_age_secs, Option<f64>),
        name!(state_age_secs, f64),
        name!(reason, String),
    ),
> {
    let rows = match shmem::status() {
        Ok(rows) => rows,
        Err(ShmemError::NotAvailable) => Vec::new(),
        Err(e) => pgrx::error!("axiom_watch_status: {e}"),
    };
    // SAFETY: plain clock read.
    let now = unsafe { pg_sys::GetCurrentTimestamp() };
    #[allow(
        clippy::cast_precision_loss,
        reason = "ages in seconds; sub-microsecond precision is irrelevant"
    )]
    let age = move |ts: i64| (now.saturating_sub(ts)) as f64 / 1_000_000.0;
    TableIterator::new(rows.into_iter().map(move |r| {
        (
            r.endpoint,
            r.resource,
            r.namespace,
            r.state.name().to_owned(),
            i64::from(r.objects),
            r.resource_version,
            if r.last_event_us == 0 {
                None
            } else {
                Some(age(r.last_event_us))
            },
            age(r.state_since_us),
            r.reason,
        )
    }))
}

/// Counters for one gateway process: how much work it has done since it
/// started, and when that was.
///
/// Takes a foreign server name and asks that gateway directly, so it reflects
/// the process serving that server right now. Every counter resets when the
/// gateway restarts; `started_at_unix_seconds` is how a caller tells a restart
/// from a quiet period, and it is what makes "since you came back, how many
/// full listings have you done" a question SQL can ask. Wrap it in
/// `to_timestamp()` for a readable value.
///
/// Raises a SQL error if the server does not exist, its options are invalid, or
/// the gateway cannot be reached — never returns zeros for an unreachable
/// gateway, which would read as an idle one (docs/RULES.md §1).
#[pg_extern]
#[allow(clippy::type_complexity, reason = "pgrx TableIterator column tuple")]
fn axiom_gateway_stats(
    server: &str,
) -> TableIterator<
    'static,
    (
        name!(started_at_unix_seconds, i64),
        name!(get_calls, i64),
        name!(list_calls, i64),
        name!(create_calls, i64),
        name!(update_calls, i64),
        name!(delete_calls, i64),
        name!(subscribe_calls, i64),
        name!(subscribe_list_calls, i64),
        name!(openapi_fetches, i64),
        name!(openapi_group_versions, i64),
        name!(access_reviews, i64),
    ),
> {
    let opts = match crate::fdw::server_options_by_name(server) {
        Ok(o) => o,
        Err(e) => pgrx::error!("axiom_gateway_stats: {e}"),
    };
    let s = match crate::client::stats(&opts) {
        Ok(s) => s,
        Err(e) => pgrx::error!("axiom_gateway_stats: {}: {e}", opts.target.endpoint),
    };
    // Counters are u64 on the wire and i64 in SQL. Saturating rather than
    // wrapping: a count past i64::MAX is not reachable in practice, and a
    // wrapped negative would be a far more confusing thing to show.
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    TableIterator::once((
        s.started_at_unix_seconds,
        n(s.get_calls),
        n(s.list_calls),
        n(s.create_calls),
        n(s.update_calls),
        n(s.delete_calls),
        n(s.subscribe_calls),
        n(s.subscribe_list_calls),
        n(s.openapi_fetches),
        n(s.openapi_group_versions),
        n(s.access_reviews),
    ))
}
