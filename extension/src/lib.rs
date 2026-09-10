//! Axiom — a Postgres foreign data wrapper for Kubernetes.
//!
//! Phase 0 (see `docs/PLAN.md`): no FDW callbacks yet. The extension registers
//! configuration GUCs and a background worker that pings the Go gateway over
//! TLS on a timer, proving the Postgres↔gateway gRPC plumbing end to end.
//!
//! The background worker is only started when the library is listed in
//! `shared_preload_libraries`; `CREATE EXTENSION axiom` alone installs the SQL
//! objects and GUC definitions but cannot register a static worker (Postgres
//! restriction), and says so loudly rather than silently doing nothing.

use pgrx::prelude::*;

pub mod backoff;
pub mod bgworker;
pub mod config;
pub mod ping;
pub mod proto;

::pgrx::pg_module_magic!();

/// Returns the extension's crate version. Exists so `CREATE EXTENSION axiom`
/// has a callable object to smoke-test against.
#[pg_extern]
fn axiom_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Postgres calls this once per process when the library is loaded.
///
/// Side effects: defines the `axiom.*` GUCs (always) and registers the gateway
/// pinger background worker (only under `shared_preload_libraries`, otherwise
/// a WARNING is emitted explaining why the worker was not started).
#[allow(non_snake_case)]
#[pg_guard]
pub extern "C" fn _PG_init() {
    bgworker::define_gucs();
    // SAFETY: reading a plain `bool` global that Postgres sets before calling
    // `_PG_init` and never mutates concurrently with it.
    let preloading = unsafe { pg_sys::process_shared_preload_libraries_in_progress };
    if preloading {
        bgworker::register();
    } else {
        warning!(
            "axiom: not loaded via shared_preload_libraries; the gateway background worker \
             will not run. Add `shared_preload_libraries = 'axiom'` to postgresql.conf and restart."
        );
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn version_matches_crate() {
        let v = Spi::get_one::<&str>("SELECT axiom_version()");
        assert_eq!(v, Ok(Some(env!("CARGO_PKG_VERSION"))));
    }

    #[pg_test]
    fn gucs_are_defined_with_test_values() {
        assert_eq!(
            Spi::get_one::<&str>("SHOW axiom.gateway_endpoint"),
            Ok(Some("https://127.0.0.1:1"))
        );
        assert_eq!(
            Spi::get_one::<&str>("SHOW axiom.ping_interval_secs"),
            Ok(Some("2"))
        );
        assert_eq!(
            Spi::get_one::<&str>("SHOW axiom.rpc_timeout_secs"),
            Ok(Some("1"))
        );
    }

    /// The test harness preloads the library, so the static worker must be
    /// registered and visible to the stats collector.
    #[pg_test]
    fn bgworker_is_running() {
        let n = Spi::get_one::<i64>(
            "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'axiom gateway pinger'",
        );
        assert_eq!(n, Ok(Some(1)));
    }
}

/// Configuration for the `cargo pgrx test` harness.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![
            "shared_preload_libraries = 'axiom'",
            // Nothing listens here; the worker must keep running (and logging
            // failures) rather than exiting, which `bgworker_is_running` checks.
            "axiom.gateway_endpoint = 'https://127.0.0.1:1'",
            "axiom.ping_interval_secs = 2",
            "axiom.rpc_timeout_secs = 1",
        ]
    }
}
