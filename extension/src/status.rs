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
