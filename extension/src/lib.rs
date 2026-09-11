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
pub mod cache;
pub mod client;
pub mod config;
pub mod fdw;
pub mod options;
pub mod ping;
pub mod proto;
pub mod quals;
pub mod resource;
pub mod schema;
pub mod shmem;
pub mod status;
#[cfg(test)]
mod stub_gateway_tests;
pub mod table;
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
        shmem::init();
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

    #[pg_test(
        error = "option \"resource\" \"deployments\" needs options \"version\" and \"kind\" (and \"group\" for a non-core API group) to identify it; only pods, configmaps are known without them. IMPORT FOREIGN SCHEMA writes these for you"
    )]
    fn table_rejects_an_unidentified_resource() {
        // A kind that is not built in must spell out its identity; the
        // extension never asks the gateway at DDL time.
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run("CREATE FOREIGN TABLE t (name text) SERVER gw OPTIONS (resource 'deployments')")
            .expect("should fail");
    }

    #[pg_test]
    fn table_accepts_a_crd_identified_by_group_version_kind() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE w (name text, namespace text, spec jsonb, raw jsonb) \
             SERVER gw OPTIONS (resource 'widgets', group 'example.com', version 'v1', kind 'Widget')",
        )
        .expect("a CRD table should be definable without contacting the gateway");
    }

    #[pg_test(
        error = "option \"writable\" cannot be true for pods: the extension keeps this kind read-only because SQL UPDATE has no sane meaning for it"
    )]
    fn writable_cannot_be_forced_on_a_read_only_kind() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE t (name text) SERVER gw \
             OPTIONS (resource 'pods', writable 'true')",
        )
        .expect("should fail");
    }

    #[pg_test(error = "option \"namespaced\" must be true or false (got \"maybe\")")]
    fn table_rejects_a_malformed_boolean_option() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE t (name text) SERVER gw \
             OPTIONS (resource 'widgets', version 'v1', kind 'Widget', namespaced 'maybe')",
        )
        .expect("should fail");
    }

    #[pg_test(
        error = "option \"group\" contains a character that is not allowed in a Kubernetes name"
    )]
    fn table_rejects_an_identity_that_could_be_a_path() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE t (name text) SERVER gw \
             OPTIONS (resource 'widgets', version 'v1', kind 'Widget', group '../secrets')",
        )
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

    #[pg_test(error = "column \"labels\" must be of type jsonb for pods")]
    fn wrong_type_on_a_metadata_column_is_rejected_at_scan() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE t (name text, labels text) SERVER gw OPTIONS (resource 'pods')",
        )
        .expect("table");
        let _ = Spi::get_one::<i64>("SELECT count(*) FROM t WHERE name = 'impossible name'");
    }

    #[pg_test(error = "column \"phase\" must be of type text for pods")]
    fn wrong_column_type_is_rejected_at_scan() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE t (name text, phase int) SERVER gw OPTIONS (resource 'pods')",
        )
        .expect("table");
        let _ = Spi::get_one::<i64>("SELECT count(*) FROM t WHERE name = 'impossible name'");
    }

    #[pg_test]
    fn a_column_the_kind_lacks_is_a_top_level_lookup_not_an_error() {
        // Phases 1-3 rejected any name outside a fixed per-kind list. A CRD's
        // fields are not known without discovery, and a scan never discovers,
        // so an unrecognised name is now a top-level lookup that reads NULL.
        // The type is still checked, which is what catches real mistakes.
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE t (name text, nonesuch jsonb) SERVER gw OPTIONS (resource 'pods')",
        )
        .expect("table");
        let n = Spi::get_one::<i64>("SELECT count(*) FROM t WHERE name = 'impossible name'")
            .expect("query")
            .expect("count");
        assert_eq!(
            n, 0,
            "the impossible-name short circuit still avoids an RPC"
        );
    }

    // --- Phase 3: watch cache plumbing without a real gateway ----------------------

    #[pg_test]
    fn watch_status_is_queryable() {
        // The harness preloads the library, so the function returns a (possibly empty) set.
        let n = Spi::get_one::<i64>("SELECT count(*) FROM axiom_watch_status()");
        assert!(n.expect("spi").expect("row") >= 0);
    }

    #[pg_test(
        error = "option \"cache_mode\" \"bogus\" is not supported; valid values: on_demand, watch"
    )]
    fn table_rejects_unknown_cache_mode() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run("CREATE FOREIGN TABLE t (name text) SERVER gw OPTIONS (resource 'pods', cache_mode 'bogus')").expect("should fail");
    }

    /// A watch table with no active subscription falls through to the RPC path,
    /// so an unreachable gateway is still a loud connection error.
    #[pg_test]
    fn watch_table_falls_through_to_rpc_before_sync() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(
            "CREATE FOREIGN TABLE live (name text, namespace text, phase text, node text, raw jsonb) \
             SERVER gw OPTIONS (resource 'pods', cache_mode 'watch')",
        )
        .expect("table");
        Spi::run(
            "DO $$ BEGIN PERFORM count(*) FROM live WHERE namespace = 'x'; RAISE EXCEPTION 'unexpected success'; \
             EXCEPTION WHEN fdw_unable_to_establish_connection THEN \
               CREATE TEMP TABLE caught AS SELECT SQLSTATE AS s; END $$",
        )
        .expect("HV00N");
        assert_eq!(
            Spi::get_one::<String>("SELECT s FROM caught"),
            Ok(Some("HV00N".to_owned()))
        );
        // The scan requested a subscription; it is visible and not ACTIVE.
        let state = Spi::get_one::<String>(
            "SELECT state FROM axiom_watch_status() WHERE server = 'https://127.0.0.1:1' AND namespace = 'x'",
        )
        .expect("spi");
        assert!(
            matches!(state.as_deref(), Some("REQUESTED" | "RESYNCING")),
            "{state:?}"
        );
    }

    // --- Phase 2: write path without a real gateway ------------------------------

    const CM_TABLE: &str =
        "CREATE FOREIGN TABLE k8s_configmaps (name text, namespace text, data jsonb, raw jsonb) \
        SERVER gw OPTIONS (resource 'configmaps')";

    /// Writes always go to the gateway: with it unreachable, DML must fail with
    /// the connection SQLSTATE rather than succeed or silently no-op.
    #[pg_test]
    fn insert_reaches_gateway_and_surfaces_connection_error() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(CM_TABLE).expect("table");
        Spi::run(
            "DO $$ BEGIN INSERT INTO k8s_configmaps (name, namespace, data) VALUES ('a', 'b', '{\"k\":\"v\"}'); \
             RAISE EXCEPTION 'insert unexpectedly succeeded'; \
             EXCEPTION WHEN fdw_unable_to_establish_connection THEN \
               CREATE TEMP TABLE caught AS SELECT SQLSTATE AS s, SQLERRM AS m; END $$",
        )
        .expect("HV00N must be raised");
        assert_eq!(
            Spi::get_one::<String>("SELECT s FROM caught"),
            Ok(Some("HV00N".to_owned()))
        );
    }

    /// Local validation runs before any RPC: these fail even though the gateway is down.
    #[pg_test(error = "axiom: INSERT: column \"name\" is required and must not be NULL")]
    fn insert_requires_name() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(CM_TABLE).expect("table");
        Spi::run("INSERT INTO k8s_configmaps (namespace) VALUES ('b')").expect("should fail");
    }

    #[pg_test(
        error = "axiom: INSERT: \"Bad Name\" is not a valid Kubernetes name (lowercase DNS-1123 subdomain)"
    )]
    fn insert_rejects_invalid_name() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(CM_TABLE).expect("table");
        Spi::run("INSERT INTO k8s_configmaps (name, namespace) VALUES ('Bad Name', 'b')")
            .expect("should fail");
    }

    #[pg_test(
        error = "axiom: INSERT: data[\"k\"] must be a string (ConfigMap data values are strings)"
    )]
    fn insert_rejects_non_string_data() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(CM_TABLE).expect("table");
        Spi::run(
            "INSERT INTO k8s_configmaps (name, namespace, data) VALUES ('a', 'b', '{\"k\": 1}')",
        )
        .expect("should fail");
    }

    /// Pods tables report no updatable operations, so Postgres rejects DML
    /// before the FDW is even asked.
    #[pg_test(error = "foreign table \"k8s_pods\" does not allow inserts")]
    fn pods_are_read_only() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(PODS_TABLE).expect("table");
        Spi::run("INSERT INTO k8s_pods (name, namespace) VALUES ('a', 'b')").expect("should fail");
    }

    #[pg_test(
        error = "UPDATE/DELETE on an axiom foreign table requires a \"raw jsonb\" column (it carries the object's identity and resourceVersion)"
    )]
    fn update_requires_raw_column() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run("CREATE FOREIGN TABLE cm (name text, namespace text, data jsonb) SERVER gw OPTIONS (resource 'configmaps')").expect("table");
        Spi::run("UPDATE cm SET data = '{}' WHERE name = 'a'").expect("should fail");
    }

    /// An impossible WHERE means no rows to update: no scan RPC, no write RPC, success.
    #[pg_test]
    fn update_with_impossible_filter_touches_nothing() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(CM_TABLE).expect("table");
        Spi::run("UPDATE k8s_configmaps SET data = '{}' WHERE name = 'Not Valid'")
            .expect("no rows, no RPC");
        Spi::run("DELETE FROM k8s_configmaps WHERE namespace = 'a' AND namespace = 'b'")
            .expect("no rows, no RPC");
    }

    #[pg_test]
    fn explain_dml_does_not_contact_gateway() {
        Spi::run(UNREACHABLE_SERVER).expect("server");
        Spi::run(CM_TABLE).expect("table");
        let plan = Spi::get_one::<String>(
            "EXPLAIN (FORMAT TEXT) UPDATE k8s_configmaps SET data = '{}' WHERE name = 'a'",
        );
        assert!(plan
            .expect("explain")
            .expect("row")
            .contains("Update on k8s_configmaps"));
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
