//! Axiom — a Postgres foreign data wrapper for Kubernetes.
//!
//! Phase 0 (see `docs/PLAN.md`) registers configuration GUCs and a background
//! worker that pings the Go gateway over TLS on a timer. Phase 1 adds the
//! `axiom_fdw` foreign data wrapper with read-only, on-demand scans of Pods
//! (`fdw.rs`), backed by per-backend unary RPCs (`client.rs`).
//!
//! The background worker is only started when the library is listed in
//! `shared_preload_libraries`; `CREATE EXTENSION axiom` alone installs the SQL
//! objects and GUC definitions but cannot register a static worker (Postgres
//! restriction), and says so loudly rather than silently doing nothing.

use pgrx::prelude::*;

pub mod backoff;
pub mod bgworker;
pub mod client;
pub mod config;
pub mod fdw;
pub mod options;
pub mod ping;
pub mod pods;
pub mod proto;
pub mod quals;
pub mod transport;

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

    // --- Phase 1: FDW DDL and scan behaviour without a real gateway ------------

    /// A server pointing at a port nothing listens on, with a short timeout.
    const UNREACHABLE_SERVER: &str = "CREATE SERVER gw FOREIGN DATA WRAPPER axiom_fdw \
        OPTIONS (endpoint 'https://127.0.0.1:1', rpc_timeout_secs '1')";
    const PODS_TABLE: &str = "CREATE FOREIGN TABLE k8s_pods (name text, namespace text, phase text, node text, raw jsonb) \
        SERVER gw OPTIONS (resource 'pods')";

    #[pg_test]
    fn fdw_is_installed() {
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_foreign_data_wrapper WHERE fdwname = 'axiom_fdw'"
            ),
            Ok(Some(1))
        );
    }

    #[pg_test]
    fn server_and_table_ddl_accepts_valid_options() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(PODS_TABLE).expect("table");
        Spi::run("CREATE FOREIGN TABLE pods_min (name text) SERVER gw OPTIONS (resource 'pods')")
            .expect("subset of columns");
    }

    #[pg_test(
        error = "invalid option \"bogus\" for Server: valid options are endpoint, ca_cert, rpc_timeout_secs"
    )]
    fn server_rejects_unknown_option() {
        Spi::run("CREATE SERVER gw FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint 'https://gw', bogus '1')").expect("should fail");
    }

    #[pg_test(
        error = "option \"endpoint\": gateway endpoint must use https (got scheme \"http\"); plaintext gateway connections are not supported"
    )]
    fn server_rejects_plaintext_endpoint() {
        Spi::run(
            "CREATE SERVER gw FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint 'http://gw:8080')",
        )
        .expect("should fail");
    }

    #[pg_test(error = "required option \"endpoint\" is missing")]
    fn server_requires_endpoint() {
        Spi::run("CREATE SERVER gw FOREIGN DATA WRAPPER axiom_fdw").expect("should fail");
    }

    #[pg_test(error = "option \"resource\" \"deployments\" is not supported; valid values: pods")]
    fn table_rejects_unknown_resource() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run("CREATE FOREIGN TABLE t (name text) SERVER gw OPTIONS (resource 'deployments')")
            .expect("should fail");
    }

    #[pg_test(error = "invalid option \"password\" for UserMapping: no options are accepted")]
    fn user_mapping_rejects_options_in_phase1() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run("CREATE USER MAPPING FOR CURRENT_USER SERVER gw OPTIONS (password 'x')")
            .expect("should fail");
    }

    /// Gateway down must be a SQL error with the FDW connection SQLSTATE and a
    /// stable message prefix, not a crash and not an empty result. Caught in
    /// PL/pgSQL because the transport detail after the prefix varies.
    #[pg_test]
    fn scan_surfaces_unreachable_gateway_as_sql_error() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(PODS_TABLE).expect("table");
        Spi::run(
            "DO $$ BEGIN PERFORM count(*) FROM k8s_pods; RAISE EXCEPTION 'scan unexpectedly succeeded'; \
             EXCEPTION WHEN fdw_unable_to_establish_connection THEN \
               CREATE TEMP TABLE caught AS SELECT SQLSTATE AS s, SQLERRM AS m; END $$",
        )
        .expect("error must be catchable as fdw_unable_to_establish_connection");
        assert_eq!(
            Spi::get_one::<String>("SELECT s FROM caught"),
            Ok(Some("HV00N".to_owned()))
        );
        let msg = Spi::get_one::<String>("SELECT m FROM caught")
            .expect("spi")
            .expect("row");
        assert!(
            msg.starts_with("axiom: cannot reach gateway https://127.0.0.1:1: "),
            "{msg}"
        );
    }

    /// An impossible filter never contacts the gateway (which here would fail).
    #[pg_test]
    fn impossible_filter_returns_no_rows_without_rpc() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(PODS_TABLE).expect("table");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM k8s_pods WHERE name = 'Not A Valid Name'"),
            Ok(Some(0))
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM k8s_pods WHERE namespace = 'a' AND namespace = 'b'"
            ),
            Ok(Some(0))
        );
    }

    /// Planning alone must not contact the gateway.
    #[pg_test]
    fn explain_does_not_contact_gateway() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(PODS_TABLE).expect("table");
        let plan = Spi::get_one::<String>(
            "EXPLAIN (FORMAT TEXT) SELECT name FROM k8s_pods WHERE namespace = 'x'",
        );
        assert!(plan
            .expect("explain")
            .expect("row")
            .contains("Foreign Scan on k8s_pods"));
    }

    #[pg_test(
        error = "column \"labels\" is not a Pod column; supported columns: name, namespace, phase, node, raw"
    )]
    fn unknown_column_is_rejected_at_scan() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE t (name text, labels text) SERVER gw OPTIONS (resource 'pods')",
        )
        .expect("table");
        let _ = Spi::get_one::<i64>("SELECT count(*) FROM t WHERE name = 'impossible name'");
    }

    #[pg_test(error = "column \"phase\" must be of type text")]
    fn wrong_column_type_is_rejected_at_scan() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE t (name text, phase int) SERVER gw OPTIONS (resource 'pods')",
        )
        .expect("table");
        let _ = Spi::get_one::<i64>("SELECT count(*) FROM t WHERE name = 'impossible name'");
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
