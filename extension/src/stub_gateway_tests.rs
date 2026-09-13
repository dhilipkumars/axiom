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
use crate::proto::v1::subscribe_response::Type as EvType;
use crate::proto::v1::{
    ColumnSchema, DiscoverSchemaRequest, DiscoverSchemaResponse, GroupVersionKind, KindSchema,
    ListKindsRequest, ListKindsResponse, SqlType, StatsRequest, StatsResponse,
};
use crate::proto::v1::{
    CreateRequest, CreateResponse, DeleteRequest, DeleteResponse, GetRequest, GetResponse,
    ListRequest, ListResponse, Object, PingRequest, PingResponse, UpdateRequest, UpdateResponse,
};
use crate::proto::v1::{SubscribeRequest, SubscribeResponse};

/// In-memory "cluster": `ConfigMaps` keyed by (namespace, name), plus counters
/// and a switch to force the next Update to conflict (as if something changed
/// the object out-of-band between our read and our write).
struct Cluster {
    configmaps: Mutex<BTreeMap<(String, String), Value>>,
    pods: Mutex<Vec<Value>>,
    next_rv: AtomicUsize,
    list_calls: AtomicUsize,
    subscribe_calls: AtomicUsize,
    force_conflict_once: AtomicBool,
    /// While set, Subscribe is refused with UNAVAILABLE (gateway "down").
    refuse_subscribe: AtomicBool,
    /// Live watch feed: every Subscribe stream forwards these after its initial listing.
    events: tokio::sync::broadcast::Sender<SubscribeResponse>,
    /// Every emitted event with its resourceVersion, so a resume replays what
    /// a subscriber missed (as the API server does within its history window).
    event_log: Mutex<Vec<(u64, SubscribeResponse)>>,
    /// Bumping this ends every open Subscribe stream (simulates losing the gateway).
    stream_generation: tokio::sync::watch::Sender<u64>,
}

impl Default for Cluster {
    fn default() -> Self {
        let (events, _) = tokio::sync::broadcast::channel(256);
        let (stream_generation, _) = tokio::sync::watch::channel(0);
        Self {
            configmaps: Mutex::new(BTreeMap::new()),
            pods: Mutex::new(vec![
                pod("shop", "web-0", "Running", "n1"),
                pod("shop", "web-1", "Pending", ""),
                pod("other", "db", "Running", "n2"),
            ]),
            next_rv: AtomicUsize::new(0),
            list_calls: AtomicUsize::new(0),
            subscribe_calls: AtomicUsize::new(0),
            force_conflict_once: AtomicBool::new(false),
            refuse_subscribe: AtomicBool::new(false),
            events,
            event_log: Mutex::new(Vec::new()),
            stream_generation,
        }
    }
}

impl Cluster {
    fn rv(&self) -> String {
        (self.next_rv.fetch_add(1, Ordering::SeqCst) + 1).to_string()
    }

    /// Emits a live watch event for a pod (also updating the stub's own state).
    fn emit_pod(&self, ty: EvType, mut p: Value) {
        p["metadata"]["resourceVersion"] = Value::String(self.rv());
        {
            let mut pods = self.pods.lock().expect("lock");
            pods.retain(|x| {
                !(x["metadata"]["namespace"] == p["metadata"]["namespace"]
                    && x["metadata"]["name"] == p["metadata"]["name"])
            });
            if ty != EvType::Deleted {
                pods.push(p.clone());
            }
        }
        let rv = p["metadata"]["resourceVersion"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let ev = SubscribeResponse {
            r#type: ty as i32,
            object: Some(to_object(&p)),
            resource_version: rv.clone(),
        };
        self.event_log
            .lock()
            .expect("lock")
            .push((rv.parse().unwrap_or(0), ev.clone()));
        let _ = self.events.send(ev);
    }

    /// Ends every open stream, as if the gateway died.
    fn drop_streams(&self) {
        self.stream_generation.send_modify(|g| *g += 1);
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

    type SubscribeStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<SubscribeResponse, Status>> + Send>,
    >;

    async fn subscribe(
        &self,
        req: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        self.0.subscribe_calls.fetch_add(1, Ordering::SeqCst);
        if self.0.refuse_subscribe.load(Ordering::SeqCst) {
            return Err(Status::unavailable("stub: gateway is down"));
        }
        let req = req.into_inner();
        let kind = req.gvk.map(|g| g.kind).unwrap_or_default();
        if kind != "Pod" {
            return Err(Status::invalid_argument(
                "stub: only Pod subscriptions are supported",
            ));
        }
        let ns = req.namespace.clone();
        let mut initial: Vec<SubscribeResponse> = Vec::new();
        if req.resource_version.is_empty() {
            for p in self
                .0
                .pods
                .lock()
                .expect("lock")
                .iter()
                .filter(|p| ns.is_empty() || p["metadata"]["namespace"] == ns)
            {
                initial.push(SubscribeResponse {
                    r#type: EvType::Added as i32,
                    object: Some(to_object(p)),
                    resource_version: String::new(),
                });
            }
            initial.push(SubscribeResponse {
                r#type: EvType::Synced as i32,
                object: None,
                resource_version: self.0.rv(),
            });
        } else {
            // Resume: replay everything after the caller's bookmark (no listing).
            let since: u64 = req.resource_version.parse().unwrap_or(0);
            for (rv, ev) in self.0.event_log.lock().expect("lock").iter() {
                let keep = ns.is_empty() || ev.object.as_ref().is_some_and(|o| o.namespace == ns);
                if *rv > since && keep {
                    initial.push(ev.clone());
                }
            }
            // As the API server does for a caught-up watcher: a BOOKMARK after the backlog.
            initial.push(SubscribeResponse {
                r#type: EvType::Bookmark as i32,
                object: None,
                resource_version: self.0.rv(),
            });
        }
        let mut live = self.0.events.subscribe();
        let mut generation = self.0.stream_generation.subscribe();
        let start_gen = *generation.borrow();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            for ev in initial {
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            loop {
                tokio::select! {
                    changed = generation.changed() => {
                        // Stream ends: the gateway "died".
                        if changed.is_err() || *generation.borrow() != start_gen { return; }
                    }
                    ev = live.recv() => match ev {
                        Ok(ev) => {
                            let keep = ns.is_empty() || ev.object.as_ref().is_some_and(|o| o.namespace == ns);
                            if keep && tx.send(Ok(ev)).await.is_err() { return; }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => return,
                    },
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
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

    /// The stub serves one hardcoded kind, so discovery answers for that kind
    /// and refuses everything else. The extension's own IMPORT FOREIGN SCHEMA
    /// tests drive this; the column list is what the real gateway's pure
    /// `Columns` rule produces for a `ConfigMap`.
    async fn discover_schema(
        &self,
        req: Request<DiscoverSchemaRequest>,
    ) -> Result<Response<DiscoverSchemaResponse>, Status> {
        let gvk = req.into_inner().gvk.unwrap_or_default();
        let schema = stub_kind(&gvk.kind)
            .ok_or_else(|| Status::invalid_argument(format!("unsupported kind: {}", gvk.kind)))?;
        Ok(Response::new(DiscoverSchemaResponse {
            schema: Some(schema),
        }))
    }

    /// Counters for the stub. It reports the two it actually tracks; the rest
    /// stay zero, which is honest for a stub that never does that work.
    /// `started_at_unix_seconds` is 0 because the stub has no meaningful
    /// process start to report.
    async fn stats(&self, _req: Request<StatsRequest>) -> Result<Response<StatsResponse>, Status> {
        Ok(Response::new(StatsResponse {
            started_at_unix_seconds: 0,
            list_calls: self.0.list_calls.load(Ordering::SeqCst) as u64,
            subscribe_calls: self.0.subscribe_calls.load(Ordering::SeqCst) as u64,
            ..Default::default()
        }))
    }

    async fn list_kinds(
        &self,
        req: Request<ListKindsRequest>,
    ) -> Result<Response<ListKindsResponse>, Status> {
        let req = req.into_inner();
        let kinds = ["Pod", "ConfigMap", "Widget"]
            .into_iter()
            .filter_map(stub_kind)
            .filter(|k| {
                let gvk = k.gvk.clone().unwrap_or_default();
                req.group.as_ref().is_none_or(|g| *g == gvk.group)
                    && (req.plurals.is_empty() || req.plurals.contains(&k.plural))
            })
            .collect();
        Ok(Response::new(ListKindsResponse { kinds }))
    }
}

/// Builds the wire schema for one stub kind, mirroring what the real gateway's
/// discovery would return.
fn stub_kind(kind: &str) -> Option<KindSchema> {
    let text = |name: &str, source: &str| ColumnSchema {
        name: name.to_owned(),
        sql_type: SqlType::Text as i32,
        source: source.to_owned(),
    };
    let jsonb = |name: &str, source: &str| ColumnSchema {
        name: name.to_owned(),
        sql_type: SqlType::Jsonb as i32,
        source: source.to_owned(),
    };
    let meta = || {
        vec![
            text("api_version", "apiVersion"),
            text("kind", "kind"),
            text("name", "metadata.name"),
            text("namespace", "metadata.namespace"),
            text("uid", "metadata.uid"),
            text("resource_version", "metadata.resourceVersion"),
            text("creation_timestamp", "metadata.creationTimestamp"),
            jsonb("labels", "metadata.labels"),
            jsonb("annotations", "metadata.annotations"),
            jsonb("metadata", "metadata"),
        ]
    };
    let (group, plural, writable) = match kind {
        "Pod" => ("", "pods", true),
        "ConfigMap" => ("", "configmaps", true),
        "Widget" => ("example.com", "widgets", true),
        _ => return None,
    };
    let mut columns = meta();
    match kind {
        "Pod" => {
            columns.push(text("phase", "status.phase"));
            columns.push(text("node", "spec.nodeName"));
            columns.push(jsonb("spec", "spec"));
            columns.push(jsonb("status", "status"));
        }
        "ConfigMap" => {
            columns.push(jsonb("binary_data", "binaryData"));
            columns.push(jsonb("data", "data"));
            columns.push(jsonb("immutable", "immutable"));
        }
        _ => {
            columns.push(jsonb("spec", "spec"));
            columns.push(jsonb("status", "status"));
        }
    }
    columns.push(jsonb("raw", "the whole object"));
    Some(KindSchema {
        gvk: Some(GroupVersionKind {
            group: group.to_owned(),
            version: "v1".to_owned(),
            kind: kind.to_owned(),
        }),
        plural: plural.to_owned(),
        namespaced: true,
        columns,
        writable,
        watchable: true,
    })
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

// --- Phase 3: watch-driven cache -----------------------------------------------------

/// Polls `f` until it returns Some, or fails after `secs`.
fn wait_for<T>(secs: u64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn watch_state(pg: &mut postgres::Client, port: u16) -> Option<(String, i64, String)> {
    pg.query_opt(
        "SELECT state, objects, reason FROM axiom_watch_status() WHERE server = $1 AND resource = 'pods' AND namespace = 'shop'",
        &[&format!("https://localhost:{port}")],
    )
    .expect("status")
    .map(|r| (r.get(0), r.get(1), r.get(2)))
}

fn shop_count(pg: &mut postgres::Client) -> i64 {
    pg.query_one(
        "SELECT count(*) FROM live_pods WHERE namespace = 'shop'",
        &[],
    )
    .expect("count")
    .get(0)
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear scenario: warm, live, degrade, resync"
)]
fn stub_gateway_watch_cache_live_degraded_resync() {
    let stub = start_stub();
    let mut pg = pg_client();
    let port = stub.addr.port();
    let ca = stub.ca_path.display();
    // Not in a transaction: the subscription outlives any statement and the
    // background worker must see the table's server while we poll.
    pg.batch_execute(&format!(
        "DROP SERVER IF EXISTS stub_w CASCADE;
         CREATE SERVER stub_w FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint 'https://localhost:{port}', ca_cert '{ca}', rpc_timeout_secs '5');
         CREATE FOREIGN TABLE live_pods (name text, namespace text, phase text, node text, raw jsonb)
           SERVER stub_w OPTIONS (resource 'pods', cache_mode 'watch');"
    ))
    .expect("ddl");

    // 1. First scan: no subscription yet → on-demand RPC, and a watch is requested.
    assert_eq!(shop_count(&mut pg), 2);
    assert_eq!(stub.cluster.list_calls.load(Ordering::SeqCst), 1);
    let (state, _, _) = watch_state(&mut pg, port).expect("subscription requested");
    assert!(
        matches!(state.as_str(), "REQUESTED" | "RESYNCING" | "ACTIVE"),
        "{state}"
    );

    // 2. The worker opens the stream and syncs.
    wait_for(15, "watch ACTIVE", || {
        watch_state(&mut pg, port).filter(|(s, _, _)| s == "ACTIVE")
    });
    assert_eq!(stub.cluster.subscribe_calls.load(Ordering::SeqCst), 1);

    // 3. Cached scans issue no RPCs.
    assert_eq!(shop_count(&mut pg), 2);
    assert_eq!(shop_count(&mut pg), 2);
    assert_eq!(
        stub.cluster.list_calls.load(Ordering::SeqCst),
        1,
        "cached scans must not LIST"
    );

    // 4. Live events flow into the cache.
    stub.cluster
        .emit_pod(EvType::Added, pod("shop", "web-2", "Running", "n3"));
    wait_for(10, "ADDED reflected", || {
        (shop_count(&mut pg) == 3).then_some(())
    });
    let node: String = pg
        .query_one(
            "SELECT node FROM live_pods WHERE namespace = 'shop' AND name = 'web-2'",
            &[],
        )
        .expect("row")
        .get(0);
    assert_eq!(node, "n3");
    stub.cluster
        .emit_pod(EvType::Modified, pod("shop", "web-2", "Running", "n4"));
    wait_for(10, "MODIFIED reflected", || {
        pg.query_one(
            "SELECT node FROM live_pods WHERE namespace = 'shop' AND name = 'web-2'",
            &[],
        )
        .ok()
        .filter(|r| r.get::<_, String>(0) == "n4")
        .map(|_| ())
    });
    stub.cluster
        .emit_pod(EvType::Deleted, pod("shop", "web-1", "Pending", ""));
    wait_for(10, "DELETED reflected", || {
        (shop_count(&mut pg) == 2).then_some(())
    });
    assert_eq!(
        stub.cluster.list_calls.load(Ordering::SeqCst),
        1,
        "still no LIST after live events"
    );

    // 5. Gateway dies: DEGRADED, cache still served (stale), no crash.
    stub.cluster.drop_streams();
    wait_for(15, "watch DEGRADED", || {
        watch_state(&mut pg, port).filter(|(s, _, _)| s == "DEGRADED")
    });
    assert_eq!(shop_count(&mut pg), 2, "stale cache is still served");
    assert_eq!(
        stub.cluster.list_calls.load(Ordering::SeqCst),
        1,
        "stale reads do not LIST either"
    );

    // 5b. Reconnect attempts that fail while the gateway is down must keep the
    //     subscription DEGRADED (servable, bookmark kept), never drop to REQUESTED.
    stub.cluster.refuse_subscribe.store(true, Ordering::SeqCst);
    stub.cluster.drop_streams();
    wait_for(15, "watch DEGRADED (refusing)", || {
        watch_state(&mut pg, port).filter(|(s, _, _)| s == "DEGRADED")
    });
    let calls_before = stub.cluster.subscribe_calls.load(Ordering::SeqCst);
    wait_for(20, "a refused reconnect attempt", || {
        (stub.cluster.subscribe_calls.load(Ordering::SeqCst) > calls_before).then_some(())
    });
    std::thread::sleep(std::time::Duration::from_millis(600));
    let (state, _, _) = watch_state(&mut pg, port).expect("status");
    assert_eq!(
        state, "DEGRADED",
        "a failed reconnect must not demote the subscription"
    );
    assert_eq!(shop_count(&mut pg), 2, "still served from the stale cache");
    stub.cluster.refuse_subscribe.store(false, Ordering::SeqCst);

    // 6. A change while disconnected must be picked up on resume, without a relist.
    stub.cluster
        .emit_pod(EvType::Added, pod("shop", "web-3", "Running", "n5"));
    wait_for(30, "watch ACTIVE again", || {
        watch_state(&mut pg, port).filter(|(s, _, _)| s == "ACTIVE")
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while shop_count(&mut pg) != 3 {
        if std::time::Instant::now() >= deadline {
            let names: Vec<String> = pg
                .query(
                    "SELECT name FROM live_pods WHERE namespace = 'shop' ORDER BY 1",
                    &[],
                )
                .expect("names")
                .iter()
                .map(|r| r.get(0))
                .collect();
            let log: Vec<(u64, i32, Option<String>)> = stub
                .cluster
                .event_log
                .lock()
                .expect("lock")
                .iter()
                .map(|(rv, e)| (*rv, e.r#type, e.object.as_ref().map(|o| o.name.clone())))
                .collect();
            panic!(
                "missed event not replayed: names={names:?} status={:?} subscribe_calls={} log={log:?}",
                watch_state(&mut pg, port),
                stub.cluster.subscribe_calls.load(Ordering::SeqCst),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        stub.cluster.subscribe_calls.load(Ordering::SeqCst) >= 2,
        "reconnected"
    );
    assert_eq!(
        stub.cluster.list_calls.load(Ordering::SeqCst),
        1,
        "resume must not LIST"
    );

    pg.batch_execute("DROP SERVER stub_w CASCADE")
        .expect("cleanup");
}

/// `IMPORT FOREIGN SCHEMA` against the stub: the generated DDL must define
/// usable tables, and the tables it defines must then scan without any further
/// discovery. This is the Phase 4 claim in one test.
#[test]
fn import_foreign_schema_generates_usable_tables() {
    let stub = start_stub();
    let mut pg = pg_client();
    let ca = stub.ca_path.display();
    let port = stub.addr.port();
    let mut tx = pg.transaction().expect("begin");
    tx.batch_execute(&format!(
        "CREATE SERVER imp FOREIGN DATA WRAPPER axiom_fdw \
           OPTIONS (endpoint 'https://localhost:{port}', ca_cert '{ca}', rpc_timeout_secs '5');
         CREATE SCHEMA k8s;
         IMPORT FOREIGN SCHEMA k8s FROM SERVER imp INTO k8s;"
    ))
    .expect("import");

    // Every kind the stub serves became a table, named by its plural.
    let tables: Vec<String> = tx
        .query(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'k8s' AND c.relkind = 'f' ORDER BY 1",
            &[],
        )
        .expect("catalog")
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(tables, vec!["configmaps", "pods", "widgets"]);

    // The CRD's table carries the resolved identity, so a scan needs no discovery.
    let opts: Vec<String> = tx
        .query(
            "SELECT unnest(ftoptions) FROM pg_foreign_table ft
               JOIN pg_class c ON c.oid = ft.ftrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'k8s' AND c.relname = 'widgets' ORDER BY 1",
            &[],
        )
        .expect("options")
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(opts.contains(&"group=example.com".to_owned()), "{opts:?}");
    assert!(opts.contains(&"kind=Widget".to_owned()), "{opts:?}");
    assert!(opts.contains(&"version=v1".to_owned()), "{opts:?}");
    assert!(opts.contains(&"resource=widgets".to_owned()), "{opts:?}");

    // The promoted metadata columns and the kind's own top-level fields are there.
    let cols: Vec<String> = tx
        .query(
            "SELECT a.attname FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'k8s' AND c.relname = 'widgets' AND a.attnum > 0
              ORDER BY a.attnum",
            &[],
        )
        .expect("columns")
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        cols,
        vec![
            "api_version",
            "kind",
            "name",
            "namespace",
            "uid",
            "resource_version",
            "creation_timestamp",
            "labels",
            "annotations",
            "metadata",
            "spec",
            "status",
            "raw"
        ]
    );

    // An imported table scans through the ordinary path. The stub serves pods,
    // so this exercises generated DDL end to end rather than only its text.
    let n: i64 = tx
        .query_one(
            "SELECT count(*) FROM k8s.pods WHERE namespace = 'shop'",
            &[],
        )
        .expect("scan imported table")
        .get(0);
    assert_eq!(
        n, 2,
        "the imported pods table must scan like a hand-written one"
    );

    tx.rollback().expect("rollback");
}

/// LIMIT TO, EXCEPT, and the import options.
#[test]
fn import_foreign_schema_filters_and_options() {
    let stub = start_stub();
    let mut pg = pg_client();
    let ca = stub.ca_path.display();
    let port = stub.addr.port();
    let mut tx = pg.transaction().expect("begin");
    tx.batch_execute(&format!(
        "CREATE SERVER imp2 FOREIGN DATA WRAPPER axiom_fdw \
           OPTIONS (endpoint 'https://localhost:{port}', ca_cert '{ca}', rpc_timeout_secs '5');
         CREATE SCHEMA only_pods;
         CREATE SCHEMA not_pods;
         CREATE SCHEMA prefixed;
         IMPORT FOREIGN SCHEMA k8s LIMIT TO (pods) FROM SERVER imp2 INTO only_pods;
         IMPORT FOREIGN SCHEMA k8s EXCEPT (pods, configmaps) FROM SERVER imp2 INTO not_pods;
         IMPORT FOREIGN SCHEMA k8s FROM SERVER imp2 INTO prefixed
           OPTIONS (prefix 'c1_', cache_mode 'watch');"
    ))
    .expect("imports");

    let names = |tx: &mut postgres::Transaction<'_>, schema: &str| -> Vec<String> {
        tx.query(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND c.relkind = 'f' ORDER BY 1",
            &[&schema],
        )
        .expect("catalog")
        .iter()
        .map(|r| r.get(0))
        .collect()
    };
    assert_eq!(names(&mut tx, "only_pods"), vec!["pods"]);
    assert_eq!(names(&mut tx, "not_pods"), vec!["widgets"]);
    assert_eq!(
        names(&mut tx, "prefixed"),
        vec!["c1_configmaps", "c1_pods", "c1_widgets"],
        "the prefix renames tables so two clusters can share a schema"
    );

    let opts: Vec<String> = tx
        .query(
            "SELECT unnest(ftoptions) FROM pg_foreign_table ft
               JOIN pg_class c ON c.oid = ft.ftrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'prefixed' AND c.relname = 'c1_pods'",
            &[],
        )
        .expect("options")
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(opts.contains(&"cache_mode=watch".to_owned()), "{opts:?}");
    assert!(
        opts.contains(&"resource=pods".to_owned()),
        "the prefix must not leak into the resource option: {opts:?}"
    );

    tx.rollback().expect("rollback");
}

/// An import option that is not understood must fail the statement rather than
/// silently producing tables configured differently than asked.
#[test]
fn import_foreign_schema_rejects_unknown_options() {
    let stub = start_stub();
    let mut pg = pg_client();
    let ca = stub.ca_path.display();
    let port = stub.addr.port();
    let mut tx = pg.transaction().expect("begin");
    tx.batch_execute(&format!(
        "CREATE SERVER imp3 FOREIGN DATA WRAPPER axiom_fdw \
           OPTIONS (endpoint 'https://localhost:{port}', ca_cert '{ca}', rpc_timeout_secs '5');
         CREATE SCHEMA bad;"
    ))
    .expect("ddl");
    let err = tx
        .batch_execute("IMPORT FOREIGN SCHEMA k8s FROM SERVER imp3 INTO bad OPTIONS (nonsense 'x')")
        .expect_err("unknown option should fail");
    let msg = err
        .as_db_error()
        .map_or_else(|| err.to_string(), |e| e.message().to_owned());
    assert!(
        msg.contains("nonsense") && msg.contains("cache_mode, prefix"),
        "the error should name the offending option and the valid ones: {msg}"
    );
}
