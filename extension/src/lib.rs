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
pub mod import;
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
pub extern "C-unwind" fn _PG_init() {
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

    // --- shared-memory cache: model-based property tests -----------------------
    //
    // The harness calls these through schema "tests", which pgrx-tests
    // hard-codes, so they live here rather than in a module of their own
    // beside the code they exercise.

    use crate::cache::TOMBSTONE_GRACE_US;
    use crate::resource::Resource;
    use crate::shmem::{clear, lookup_or_request, scan, sweep, tombstone, upsert};
    use crate::transport::Target;
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    /// A tiny deterministic generator.
    ///
    /// Not `rand`: a failing sequence has to be reproducible from the seed
    /// printed in the failure, and a dependency whose algorithm may change
    /// between versions cannot promise that. xorshift64* is a few lines and
    /// good enough to shuffle operations.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % n as u64).unwrap_or(0)
        }
    }

    /// Registers a subscription to operate on, returning its (slot, id).
    ///
    /// Each test uses a distinct namespace so slots do not collide: identity is
    /// (endpoint, CA, kind, namespace), and the harness runs tests in parallel
    /// against one Postgres.
    fn slot_for(namespace: &str) -> (usize, u32) {
        let target = Target::parse(Some("https://shmem-test:8443"), None).expect("target");
        let resource = Resource::new("", "v1", "Pod", "pods", true).expect("resource");
        let (slot, id, _) =
            lookup_or_request(&target, Duration::from_secs(5), &resource, namespace)
                .expect("a free subscription slot");
        (slot, id)
    }

    /// What the cache is expected to hold: the live objects, and the keys
    /// currently tombstoned.
    #[derive(Default)]
    struct Model {
        live: HashMap<(String, String), Vec<u8>>,
        tombstoned: HashSet<(String, String)>,
    }

    /// Checks the cache against the model. `scan` is the real observation: it
    /// is what a SQL query actually reads, so agreeing with it is the property
    /// that matters, not the internal counters.
    fn assert_matches(slot: usize, id: u32, ns: &str, model: &Model, step: usize, seed: u64) {
        let mut got = scan(slot, id, ns, "").expect("scan");
        got.sort_unstable();
        let mut want: Vec<Vec<u8>> = model.live.values().cloned().collect();
        want.sort_unstable();
        assert_eq!(
            got.len(),
            want.len(),
            "step {step} (seed {seed}): scan returned {} objects, model has {}",
            got.len(),
            want.len()
        );
        assert_eq!(
            got, want,
            "step {step} (seed {seed}): scan disagrees with the model"
        );

        // Every tombstoned key must be invisible to a scan, which is the whole
        // point of a tombstone: present for the grace period, never returned.
        for (ns_key, name) in &model.tombstoned {
            let hits = scan(slot, id, ns_key, name).expect("scan by key");
            assert!(
                hits.is_empty(),
                "step {step} (seed {seed}): tombstoned {ns_key}/{name} was returned by a scan"
            );
        }

        // A point lookup must find exactly the live objects, which exercises
        // the hash chains rather than the full walk.
        for ((ns_key, name), json) in &model.live {
            let hits = scan(slot, id, ns_key, name).expect("scan by key");
            assert_eq!(
                hits.len(),
                1,
                "step {step} (seed {seed}): {ns_key}/{name} should be found exactly once"
            );
            assert_eq!(
                &hits[0], json,
                "step {step} (seed {seed}): stale value for {ns_key}/{name}"
            );
        }
    }

    /// Randomised upsert/tombstone/clear sequences must leave the cache
    /// agreeing with a plain `HashMap`.
    ///
    /// This module is ~950 lines with 25 `unsafe` blocks doing its own hashing,
    /// chaining and allocation inside a DSA area, and until now nothing tested
    /// it directly: a corruption bug surfaced as "a scan returned the wrong
    /// rows" somewhere else entirely. The key count is deliberately small
    /// relative to the operation count so the same keys are hit repeatedly,
    /// which is what exercises replacement, resurrection and chain edits
    /// rather than just insertion.
    #[pg_test]
    fn cache_agrees_with_a_reference_map_under_random_operations() {
        const SEED: u64 = 0x5EED_1234_ABCD_0001;
        const OPS: usize = 400;
        const KEYS: usize = 24;

        let ns = "prop-random";
        let (slot, id) = slot_for(ns);
        clear(slot, id).expect("start from empty");

        let mut rng = Rng(SEED);
        let mut model = Model::default();

        for step in 0..OPS {
            let k = rng.below(KEYS);
            let name = format!("obj-{k:02}");
            let key = (ns.to_owned(), name.clone());

            match rng.below(100) {
                // Upsert dominates, as a real watch stream does.
                0..=59 => {
                    let json = format!(r#"{{"name":"{name}","v":{step}}}"#).into_bytes();
                    upsert(slot, id, ns, &name, &step.to_string(), &json).expect("upsert");
                    model.live.insert(key.clone(), json);
                    model.tombstoned.remove(&key);
                }
                60..=84 => {
                    tombstone(slot, id, ns, &name).expect("tombstone");
                    // Only a live object becomes a tombstone; tombstoning an
                    // absent or already-deleted key is a no-op by contract.
                    if model.live.remove(&key).is_some() {
                        model.tombstoned.insert(key);
                    }
                }
                85..=97 => {
                    // A scan of a key that may or may not exist: the result is
                    // checked below like every other step.
                }
                _ => {
                    clear(slot, id).expect("clear");
                    model.live.clear();
                    model.tombstoned.clear();
                }
            }

            assert_matches(slot, id, ns, &model, step, SEED);
        }

        // The run must actually have built something, or the assertions above
        // were checking an empty cache 400 times.
        assert!(
            !model.live.is_empty(),
            "the generated sequence left nothing live; it is not exercising the cache"
        );
    }

    /// Growing past the load factor rehashes, and must not lose or duplicate a
    /// key while doing it.
    ///
    /// Buckets start at 64 and double past 75% load, so inserting several
    /// hundred keys crosses the threshold more than once. A rehash that
    /// dropped a chain would show up as a short scan; one that relinked an
    /// entry twice as a duplicate.
    #[pg_test]
    fn rehashing_preserves_every_key() {
        const N: usize = 500;
        let ns = "prop-rehash";
        let (slot, id) = slot_for(ns);
        clear(slot, id).expect("start from empty");

        for i in 0..N {
            let name = format!("k-{i:04}");
            let json = format!(r#"{{"i":{i}}}"#).into_bytes();
            upsert(slot, id, ns, &name, "1", &json).expect("upsert");
        }

        let got = scan(slot, id, ns, "").expect("scan");
        assert_eq!(got.len(), N, "rehashing lost or duplicated entries");

        // Every key individually reachable, which a broken chain would fail
        // even when the total happens to come out right.
        for i in 0..N {
            let name = format!("k-{i:04}");
            let hits = scan(slot, id, ns, &name).expect("scan by key");
            assert_eq!(
                hits.len(),
                1,
                "{name} not found exactly once after rehashing"
            );
        }
        clear(slot, id).expect("clear");
    }

    /// Returns the tombstone count for one slot, which is the only way to see
    /// that a tombstone exists at all: a scan never returns one.
    fn tombstones_for(ns: &str) -> u32 {
        crate::shmem::status()
            .expect("status")
            .into_iter()
            .find(|r| r.namespace == ns && r.endpoint == "https://shmem-test:8443")
            .map_or(0, |r| r.tombstones)
    }

    /// A tombstone survives until the grace period, and `sweep` then removes
    /// it.
    ///
    /// Asserted on the tombstone count rather than on scan results, because a
    /// scan cannot see the difference: a tombstone is invisible whether or not
    /// it has been swept. An earlier version of this test checked scans and
    /// passed even with the expiry check removed entirely, which is how the
    /// count came to be exposed in the first place.
    ///
    /// The grace period exists so a scan that began before a delete does not
    /// watch an object vanish, so sweeping early is as wrong as never
    /// sweeping.
    #[pg_test]
    fn sweep_removes_expired_tombstones_and_spares_fresh_ones() {
        let ns = "prop-sweep";
        let (slot, id) = slot_for(ns);
        clear(slot, id).expect("start from empty");

        for i in 0..6 {
            let name = format!("s-{i}");
            upsert(
                slot,
                id,
                ns,
                &name,
                "1",
                format!(r#"{{"i":{i}}}"#).as_bytes(),
            )
            .expect("upsert");
        }
        for i in 0..4 {
            tombstone(slot, id, ns, &format!("s-{i}")).expect("tombstone");
        }
        assert_eq!(tombstones_for(ns), 4, "four objects were deleted");
        assert_eq!(
            scan(slot, id, ns, "").expect("scan").len(),
            2,
            "tombstones must not be visible to a scan"
        );

        // Fresh tombstones survive a sweep. This is the assertion the earlier
        // version could not make.
        sweep().expect("sweep");
        assert_eq!(
            tombstones_for(ns),
            4,
            "sweep removed tombstones that are still inside the grace period"
        );
        assert_eq!(
            scan(slot, id, ns, "").expect("scan").len(),
            2,
            "sweeping must not drop live objects"
        );

        // Past the grace period they go, and the live objects stay.
        std::thread::sleep(Duration::from_micros(
            u64::try_from(TOMBSTONE_GRACE_US).unwrap_or(2_000_000) + 250_000,
        ));
        sweep().expect("sweep");
        assert_eq!(
            tombstones_for(ns),
            0,
            "sweep left tombstones that are past the grace period"
        );
        assert_eq!(
            scan(slot, id, ns, "").expect("scan").len(),
            2,
            "sweeping expired tombstones must leave the live objects alone"
        );
        clear(slot, id).expect("clear");
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
