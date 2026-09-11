//! Integration tests: the extension in a real Postgres against an in-process
//! **stub gateway** (tonic server over TLS) with no Kubernetes cluster. This is
//! the "extension + real Postgres + stub gateway" tier from `docs/RULES.md` §2:
//! it pins down FDW executor behaviour (end-of-scan, RETURNING, SQLSTATE
//! mapping) that pure unit tests cannot reach and that the kind-based E2E
//! only reports as "the whole thing broke".
//!
//! Mechanics: `#[test]` functions run in the `cargo pgrx test` harness
//! process. We start the stub on 127.0.0.1 with a self-signed certificate
//! whose CA is written to a temp file, then drive SQL through the harness's
//! libpq client. The Postgres server reads the CA file from the same machine.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use postgres::error::SqlState;
use serde_json::{json, Value};
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

use crate::proto::v1::gateway_service_server::{GatewayService, GatewayServiceServer};
use crate::proto::v1::{
    CreateRequest, CreateResponse, DeleteRequest, DeleteResponse, GetRequest, GetResponse,
    ListRequest, ListResponse, Object, PingRequest, PingResponse, UpdateRequest, UpdateResponse,
};

/// In-memory "cluster": `ConfigMaps` keyed by (namespace, name), plus counters
/// and a switch to force the next Update to conflict (as if something changed
/// the object out-of-band between our read and our write).
#[derive(Default)]
struct Cluster {
    configmaps: Mutex<BTreeMap<(String, String), Value>>,
    next_rv: AtomicUsize,
    list_calls: AtomicUsize,
    force_conflict_once: AtomicBool,
}

impl Cluster {
    fn rv(&self) -> String {
        (self.next_rv.fetch_add(1, Ordering::SeqCst) + 1).to_string()
    }
}

fn pod(ns: &str, name: &str, phase: &str, node: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name,"namespace":ns,"resourceVersion":"100"},
           "spec":{"nodeName":node},"status":{"phase":phase}})
}

fn to_object(v: &Value) -> Object {
    Object {
        namespace: v["metadata"]["namespace"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        name: v["metadata"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        resource_version: v["metadata"]["resourceVersion"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        json: v.to_string().into_bytes(),
    }
}

struct Stub(Arc<Cluster>);

#[tonic::async_trait]
impl GatewayService for Stub {
    async fn ping(&self, req: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        Ok(Response::new(PingResponse {
            nonce: req.into_inner().nonce,
            gateway_version: "stub".into(),
            server_time: None,
        }))
    }

    async fn get(&self, _req: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        Err(Status::unimplemented(
            "stub: Get is not used by the extension",
        ))
    }

    async fn list(&self, req: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        self.0.list_calls.fetch_add(1, Ordering::SeqCst);
        let req = req.into_inner();
        let kind = req.gvk.map(|g| g.kind).unwrap_or_default();
        let matches = |ns: &str, name: &str| {
            (req.namespace.is_empty() || req.namespace == ns)
                && (req.name.is_empty() || req.name == name)
        };
        let objects: Vec<Object> = match kind.as_str() {
            "Pod" => [
                pod("shop", "web-0", "Running", "n1"),
                pod("shop", "web-1", "Pending", ""),
                pod("other", "db", "Running", "n2"),
            ]
            .iter()
            .filter(|p| {
                matches(
                    p["metadata"]["namespace"].as_str().unwrap_or(""),
                    p["metadata"]["name"].as_str().unwrap_or(""),
                )
            })
            .map(to_object)
            .collect(),
            "ConfigMap" => {
                let cms = self.0.configmaps.lock().expect("lock");
                cms.iter()
                    .filter(|((ns, n), _)| matches(ns, n))
                    .map(|(_, v)| to_object(v))
                    .collect()
            }
            other => {
                return Err(Status::invalid_argument(format!(
                    "unsupported kind {other}"
                )))
            }
        };
        Ok(Response::new(ListResponse {
            objects,
            resource_version: "1".into(),
        }))
    }

    async fn create(
        &self,
        req: Request<CreateRequest>,
    ) -> Result<Response<CreateResponse>, Status> {
        let req = req.into_inner();
        let mut body: Value = serde_json::from_slice(&req.json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let key = (req.namespace.clone(), req.name.clone());
        let mut cms = self.0.configmaps.lock().expect("lock");
        if cms.contains_key(&key) {
            return Err(Status::already_exists(format!(
                "configmaps \"{}\" already exists",
                req.name
            )));
        }
        body["metadata"]["resourceVersion"] = Value::String(self.0.rv());
        body["metadata"]["uid"] = Value::String(format!("uid-{}", req.name));
        cms.insert(key, body.clone());
        Ok(Response::new(CreateResponse {
            object: Some(to_object(&body)),
        }))
    }

    async fn update(
        &self,
        req: Request<UpdateRequest>,
    ) -> Result<Response<UpdateResponse>, Status> {
        let req = req.into_inner();
        let mut body: Value = serde_json::from_slice(&req.json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let key = (req.namespace.clone(), req.name.clone());
        let mut cms = self.0.configmaps.lock().expect("lock");
        let Some(current) = cms.get_mut(&key) else {
            return Err(Status::not_found(format!(
                "configmaps \"{}\" not found",
                req.name
            )));
        };
        if self.0.force_conflict_once.swap(false, Ordering::SeqCst) {
            // Simulate an out-of-band writer that bumped the object after our read.
            current["metadata"]["resourceVersion"] = Value::String(self.0.rv());
        }
        if current["metadata"]["resourceVersion"].as_str() != Some(req.resource_version.as_str()) {
            return Err(Status::aborted(format!(
                "Operation cannot be fulfilled on configmaps \"{}\": the object has been modified",
                req.name
            )));
        }
        body["metadata"]["resourceVersion"] = Value::String(self.0.rv());
        *current = body.clone();
        Ok(Response::new(UpdateResponse {
            object: Some(to_object(&body)),
        }))
    }

    async fn delete(
        &self,
        req: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let req = req.into_inner();
        let mut cms = self.0.configmaps.lock().expect("lock");
        if cms.remove(&(req.namespace, req.name.clone())).is_none() {
            return Err(Status::not_found(format!(
                "configmaps \"{}\" not found",
                req.name
            )));
        }
        Ok(Response::new(DeleteResponse {}))
    }
}

/// A running stub: its address, the CA file path, and the shared state.
struct RunningStub {
    addr: SocketAddr,
    ca_path: std::path::PathBuf,
    cluster: Arc<Cluster>,
    _rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

fn start_stub() -> RunningStub {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("self-signed cert");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();
    let dir = tempfile::Builder::new()
        .prefix("axiom-stub")
        .tempdir()
        .expect("tempdir");
    let ca_path = dir.path().join("ca.crt");
    std::fs::write(&ca_path, &cert_pem).expect("write ca");
    // The Postgres server process must be able to read it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod dir");
        std::fs::set_permissions(&ca_path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod ca");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("rt");
    let cluster = Arc::new(Cluster::default());
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let svc = GatewayServiceServer::new(Stub(Arc::clone(&cluster)));
    let tls = ServerTlsConfig::new().identity(Identity::from_pem(cert_pem, key_pem));
    rt.spawn(async move {
        Server::builder()
            .tls_config(tls)
            .expect("tls config")
            .add_service(svc)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("serve");
    });
    RunningStub {
        addr,
        ca_path,
        cluster,
        _rt: rt,
        _dir: dir,
    }
}

/// Connects to the harness Postgres, making sure the framework (server +
/// extension) is initialised first by running one trivial `pg_test`.
fn pg_client() -> postgres::Client {
    pgrx_tests::run_test(
        "version_matches_crate",
        None,
        crate::pg_test::postgresql_conf_options(),
    )
    .expect("test framework must initialise");
    pgrx_tests::client().expect("client").0
}

/// The server-side message (the crate's `Display` is just "db error").
fn message(e: &postgres::Error) -> String {
    e.as_db_error()
        .map_or_else(|| e.to_string(), |d| d.message().to_owned())
}

fn sqlstate(e: &postgres::Error) -> String {
    e.as_db_error().map_or_else(
        || format!("<non-db error: {e}>"),
        |d| d.code().code().to_owned(),
    )
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear scenario; splitting would hide the read→write ordering under test"
)]
fn stub_gateway_scan_and_dml_round_trip() {
    let stub = start_stub();
    let mut pg = pg_client();
    let ca = stub.ca_path.display();
    let mut tx = pg.transaction().expect("begin");
    tx.batch_execute(&format!(
        "CREATE SERVER stub FOREIGN DATA WRAPPER axiom_fdw \
           OPTIONS (endpoint 'https://localhost:{}', ca_cert '{ca}', rpc_timeout_secs '5');
         CREATE FOREIGN TABLE stub_pods (name text, namespace text, phase text, node text, raw jsonb)
           SERVER stub OPTIONS (resource 'pods');
         CREATE FOREIGN TABLE stub_cms (name text, namespace text, data jsonb, raw jsonb)
           SERVER stub OPTIONS (resource 'configmaps');",
        stub.addr.port()
    ))
    .expect("ddl");

    // --- scans: exact row counts (regression for a non-cleared end-of-scan slot) ---
    let rows = tx
        .query(
            "SELECT name, phase, node FROM stub_pods WHERE namespace = 'shop' ORDER BY name",
            &[],
        )
        .expect("scan");
    let got: Vec<(String, Option<String>, Option<String>)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(
        got,
        vec![
            ("web-0".into(), Some("Running".into()), Some("n1".into())),
            ("web-1".into(), Some("Pending".into()), Some(String::new()))
        ]
    );
    let n: i64 = tx
        .query_one("SELECT count(*) FROM stub_pods", &[])
        .expect("count")
        .get(0);
    assert_eq!(n, 3);
    let uid: String = tx
        .query_one("SELECT raw->'metadata'->>'resourceVersion' FROM stub_pods WHERE namespace = 'other' AND name = 'db'", &[])
        .expect("point get")
        .get(0);
    assert_eq!(uid, "100");
    assert_eq!(
        stub.cluster.list_calls.load(Ordering::SeqCst),
        3,
        "one List RPC per scan, no re-fetch"
    );

    // --- INSERT ... RETURNING reflects server-assigned metadata ---
    let row = tx
        .query_one(
            "INSERT INTO stub_cms (name, namespace, data) VALUES ('app', 'shop', '{\"LOG_LEVEL\":\"info\"}')
             RETURNING raw->'metadata'->>'uid', raw->'metadata'->>'resourceVersion', data->>'LOG_LEVEL'",
            &[],
        )
        .expect("insert");
    assert_eq!(row.get::<_, String>(0), "uid-app");
    assert_eq!(row.get::<_, String>(1), "1");
    assert_eq!(row.get::<_, String>(2), "info");
    let err = tx
        .execute(
            "INSERT INTO stub_cms (name, namespace) VALUES ('app', 'shop')",
            &[],
        )
        .expect_err("duplicate");
    assert_eq!(sqlstate(&err), SqlState::UNIQUE_VIOLATION.code());
    // A failed statement aborts the transaction; use a savepoint-free approach: new transaction.
    tx.rollback().expect("rollback");

    let mut tx = pg.transaction().expect("begin");
    tx.batch_execute(&format!(
        "CREATE SERVER stub FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint 'https://localhost:{}', ca_cert '{ca}');
         CREATE FOREIGN TABLE stub_cms (name text, namespace text, data jsonb, raw jsonb) SERVER stub OPTIONS (resource 'configmaps');",
        stub.addr.port()
    ))
    .expect("ddl");

    // --- UPDATE sends the read resourceVersion; RETURNING shows the new one ---
    let row = tx
        .query_one(
            "UPDATE stub_cms SET data = data || '{\"LOG_LEVEL\":\"debug\"}' WHERE namespace = 'shop' AND name = 'app'
             RETURNING data->>'LOG_LEVEL', raw->'metadata'->>'resourceVersion'",
            &[],
        )
        .expect("update");
    assert_eq!(row.get::<_, String>(0), "debug");
    assert_eq!(row.get::<_, String>(1), "2");
    {
        let cms = stub.cluster.configmaps.lock().expect("lock");
        assert_eq!(
            cms[&("shop".to_owned(), "app".to_owned())]["data"]["LOG_LEVEL"],
            "debug"
        );
        assert_eq!(
            cms[&("shop".to_owned(), "app".to_owned())]["metadata"]["uid"],
            "uid-app",
            "metadata preserved"
        );
    }

    // --- review follow-ups: raw-only change keeps its data edit; NULL semantics ---
    let row = tx
        .query_one(
            "UPDATE stub_cms SET raw = jsonb_set(raw, '{data,VIA_RAW}', '\"1\"') WHERE namespace = 'shop' AND name = 'app'
             RETURNING data::text",
            &[],
        )
        .expect("update via raw");
    let data: serde_json::Value = serde_json::from_str(row.get::<_, &str>(0)).expect("json");
    assert_eq!(
        data,
        json!({"LOG_LEVEL":"debug","VIA_RAW":"1"}),
        "typed data must not clobber a raw edit"
    );
    let row = tx
        .query_one("UPDATE stub_cms SET data = NULL WHERE namespace = 'shop' AND name = 'app' RETURNING data::text", &[])
        .expect("clear data");
    assert_eq!(row.get::<_, &str>(0), "{}");
    let err = tx
        .execute(
            "UPDATE stub_cms SET name = NULL WHERE namespace = 'shop' AND name = 'app'",
            &[],
        )
        .expect_err("null name");
    assert_eq!(
        sqlstate(&err),
        SqlState::NOT_NULL_VIOLATION.code(),
        "{}",
        message(&err)
    );
    tx.rollback().expect("rollback");
    let mut tx = pg.transaction().expect("begin");
    tx.batch_execute(&format!(
        "CREATE SERVER stub FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint 'https://localhost:{}', ca_cert '{ca}');
         CREATE FOREIGN TABLE stub_cms (name text, namespace text, data jsonb, raw jsonb) SERVER stub OPTIONS (resource 'configmaps');",
        stub.addr.port()
    ))
    .expect("ddl");
    tx.execute("UPDATE stub_cms SET data = '{\"LOG_LEVEL\":\"debug\"}' WHERE namespace = 'shop' AND name = 'app'", &[])
        .expect("restore data");

    // --- concurrent modification → 40001, and the write did not land ---
    stub.cluster
        .force_conflict_once
        .store(true, Ordering::SeqCst);
    let err = tx
        .execute(
            "UPDATE stub_cms SET data = '{\"X\":\"1\"}' WHERE namespace = 'shop' AND name = 'app'",
            &[],
        )
        .expect_err("conflict");
    assert_eq!(
        sqlstate(&err),
        SqlState::T_R_SERIALIZATION_FAILURE.code(),
        "{}",
        message(&err)
    );
    assert!(
        message(&err).contains("modified concurrently"),
        "{}",
        message(&err)
    );
    if let Err(e) = tx.rollback() {
        panic!(
            "rollback after conflict failed: {e} / {:?}",
            e.as_db_error()
        );
    }
    {
        let cms = stub.cluster.configmaps.lock().expect("lock");
        assert_eq!(
            cms[&("shop".to_owned(), "app".to_owned())]["data"]["LOG_LEVEL"],
            "debug",
            "stale write must not land"
        );
    }

    // --- retry after re-read succeeds; DELETE removes; scan is empty ---
    let mut tx = pg.transaction().expect("begin");
    tx.batch_execute(&format!(
        "CREATE SERVER stub FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint 'https://localhost:{}', ca_cert '{ca}');
         CREATE FOREIGN TABLE stub_cms (name text, namespace text, data jsonb, raw jsonb) SERVER stub OPTIONS (resource 'configmaps');",
        stub.addr.port()
    ))
    .expect("ddl");
    let n = tx
        .execute(
            "UPDATE stub_cms SET data = '{\"X\":\"1\"}' WHERE namespace = 'shop' AND name = 'app'",
            &[],
        )
        .expect("retry");
    assert_eq!(n, 1);
    let deleted: String = tx
        .query_one(
            "DELETE FROM stub_cms WHERE namespace = 'shop' AND name = 'app' RETURNING name",
            &[],
        )
        .expect("delete")
        .get(0);
    assert_eq!(deleted, "app");
    let n: i64 = tx
        .query_one("SELECT count(*) FROM stub_cms", &[])
        .expect("count")
        .get(0);
    assert_eq!(n, 0);
    assert!(stub.cluster.configmaps.lock().expect("lock").is_empty());
    tx.rollback().expect("rollback");
}
