//! Pure, per-kind knowledge: which Kubernetes kinds the extension serves, the
//! typed columns each exposes, how to decode an object into a row, and how to
//! build write bodies from SQL-side values. Treats object JSON as untrusted:
//! size-bounded and shape-checked before anything is trusted.
//!
//! Phase 1: Pods (read-only). Phase 2: `ConfigMaps` (read-write).
//! TODO(phase4): drive this from discovered schema instead of hand mappings.

use std::fmt;

/// A served Kubernetes kind (the `resource` table option).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// core/v1 Pod, read-only.
    Pods,
    /// core/v1 `ConfigMap`, read-write.
    ConfigMaps,
}

/// SQL type a column must be declared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    /// `text`
    Text,
    /// `jsonb`
    Jsonb,
}

/// One typed column of a kind's foreign table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnDef {
    /// Column name as declared in `CREATE FOREIGN TABLE`.
    pub name: &'static str,
    /// Required SQL type.
    pub sql_type: SqlType,
}

const fn text(name: &'static str) -> ColumnDef {
    ColumnDef {
        name,
        sql_type: SqlType::Text,
    }
}
const fn jsonb(name: &'static str) -> ColumnDef {
    ColumnDef {
        name,
        sql_type: SqlType::Jsonb,
    }
}

const POD_COLUMNS: &[ColumnDef] = &[
    text("name"),
    text("namespace"),
    text("phase"),
    text("node"),
    jsonb("raw"),
];
const CONFIGMAP_COLUMNS: &[ColumnDef] =
    &[text("name"), text("namespace"), jsonb("data"), jsonb("raw")];

/// Upper bound on one object's JSON. Matches the gRPC default max message size
/// so nothing larger can arrive anyway; enforced here so the FDW never trusts
/// the transport for this. TODO(phase3): tie to the shared-memory value bound.
pub const MAX_OBJECT_BYTES: usize = 4 * 1024 * 1024;

impl Kind {
    /// Parses the `resource` table option.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "pods" => Some(Self::Pods),
            "configmaps" => Some(Self::ConfigMaps),
            _ => None,
        }
    }

    /// Valid `resource` option values, for error messages.
    pub const NAMES: &'static [&'static str] = &["pods", "configmaps"];

    /// `(group, version, kind)` as the gateway names it.
    pub fn gvk(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Pods => ("", "v1", "Pod"),
            Self::ConfigMaps => ("", "v1", "ConfigMap"),
        }
    }

    /// Columns a foreign table of this kind may declare (any subset, by name).
    pub fn columns(self) -> &'static [ColumnDef] {
        match self {
            Self::Pods => POD_COLUMNS,
            Self::ConfigMaps => CONFIGMAP_COLUMNS,
        }
    }

    /// Whether INSERT/UPDATE/DELETE are supported. Pods stay read-only: a Pod
    /// spec is mostly immutable and "UPDATE a pod" has no sane SQL semantics.
    pub fn writable(self) -> bool {
        matches!(self, Self::ConfigMaps)
    }

    /// Index of `name` in [`Kind::columns`], if declared.
    pub fn column_index(self, name: &str) -> Option<usize> {
        self.columns().iter().position(|c| c.name == name)
    }

    /// Decodes one object into a row aligned with [`Kind::columns`].
    pub fn decode(self, bytes: &[u8], max_bytes: usize) -> Result<Row, DecodeError> {
        if bytes.len() > max_bytes {
            return Err(DecodeError::TooLarge {
                bytes: bytes.len(),
                max: max_bytes,
            });
        }
        let raw: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| DecodeError::Json(e.to_string()))?;
        Row::from_value(self, &raw)
    }
}

/// One column value. `None` at the row level means SQL NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// A text column.
    Text(String),
    /// A jsonb column.
    Json(serde_json::Value),
}

/// One decoded object, values aligned with [`Kind::columns`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// `metadata.name`.
    pub name: String,
    /// `metadata.namespace` (empty for cluster-scoped kinds).
    pub namespace: String,
    /// `metadata.resourceVersion`, needed for optimistic-concurrency writes.
    pub resource_version: String,
    /// Column values in [`Kind::columns`] order.
    pub cells: Vec<Option<Cell>>,
}

/// Why an object could not be decoded. Never echoes the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Payload exceeds the configured bound.
    TooLarge { bytes: usize, max: usize },
    /// Not valid JSON.
    Json(String),
    /// JSON is not an object, or `metadata.name` is not a non-empty string.
    Shape(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes, max } => {
                write!(f, "object of {bytes} bytes exceeds the {max} byte limit")
            }
            Self::Json(e) => write!(f, "object is not valid JSON: {e}"),
            Self::Shape(what) => write!(f, "object JSON has unexpected shape: {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn str_at<'a>(v: &'a serde_json::Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(serde_json::Value::as_str)
}

impl Row {
    /// Builds a row from an already-parsed object.
    pub fn from_value(kind: Kind, raw: &serde_json::Value) -> Result<Self, DecodeError> {
        if !raw.is_object() {
            return Err(DecodeError::Shape("top level is not an object"));
        }
        if !raw
            .get("metadata")
            .is_some_and(serde_json::Value::is_object)
        {
            return Err(DecodeError::Shape("metadata is missing or not an object"));
        }
        let name = str_at(raw, "/metadata/name")
            .filter(|s| !s.is_empty())
            .ok_or(DecodeError::Shape("metadata.name is missing or empty"))?
            .to_owned();
        let namespace = str_at(raw, "/metadata/namespace")
            .unwrap_or_default()
            .to_owned();
        let resource_version = str_at(raw, "/metadata/resourceVersion")
            .unwrap_or_default()
            .to_owned();
        let text_cell = |p: &str| str_at(raw, p).map(|s| Cell::Text(s.to_owned()));
        let cells = match kind {
            Kind::Pods => vec![
                Some(Cell::Text(name.clone())),
                Some(Cell::Text(namespace.clone())),
                text_cell("/status/phase"),
                text_cell("/spec/nodeName"),
                Some(Cell::Json(raw.clone())),
            ],
            Kind::ConfigMaps => vec![
                Some(Cell::Text(name.clone())),
                Some(Cell::Text(namespace.clone())),
                // Absent data is an empty map, so `data ? 'key'` never NULL-propagates.
                Some(Cell::Json(
                    raw.get("data")
                        .filter(|d| d.is_object())
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                )),
                Some(Cell::Json(raw.clone())),
            ],
        };
        Ok(Self {
            name,
            namespace,
            resource_version,
            cells,
        })
    }

    /// The cell for a named column, if the kind has it.
    pub fn cell(&self, kind: Kind, column: &str) -> Option<&Cell> {
        kind.column_index(column)
            .and_then(|i| self.cells.get(i))
            .and_then(Option::as_ref)
    }
}

// --- write path -------------------------------------------------------------------

/// Column values supplied by SQL for an INSERT, or the full new tuple of an
/// UPDATE (Postgres fills untouched columns from the old row), aligned with
/// [`Kind::columns`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewRow {
    /// One entry per column; `None` = SQL NULL.
    pub cells: Vec<Option<Cell>>,
}

/// Identity of an existing object, taken from its `raw` column as last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `metadata.namespace`.
    pub namespace: String,
    /// `metadata.name`.
    pub name: String,
    /// `metadata.resourceVersion` at read time; the optimistic-concurrency token.
    pub resource_version: String,
}

/// A fully built write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteBody {
    /// Target identity (for UPDATE, `resource_version` is the token to send).
    pub identity: Identity,
    /// Full object JSON to send.
    pub body: serde_json::Value,
}

/// Why SQL-side values could not be turned into a write. Messages are meant
/// to be shown to the SQL user verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The kind is read-only.
    ReadOnly(Kind),
    /// `name` is NULL or missing on INSERT.
    MissingName,
    /// `namespace` is NULL or missing on INSERT.
    MissingNamespace,
    /// A name/namespace is not a valid Kubernetes name.
    InvalidName(&'static str, String),
    /// UPDATE tried to change `name` or `namespace`.
    IdentityChange(&'static str),
    /// A jsonb column that must be an object is not one.
    NotAnObject(&'static str),
    /// `ConfigMap` `data` values must be strings.
    DataValueNotString(String),
    /// The old `raw` value lacks an identity/resourceVersion (never read from the gateway?).
    BadOldRaw(&'static str),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadOnly(k) => write!(f, "{k:?} foreign tables are read-only"),
            Self::MissingName => write!(f, "column \"name\" is required and must not be NULL"),
            Self::MissingNamespace => {
                write!(f, "column \"namespace\" is required and must not be NULL")
            }
            Self::InvalidName(col, v) => write!(
                f,
                "{v:?} is not a valid Kubernetes {col} (lowercase DNS-1123 subdomain)"
            ),
            Self::IdentityChange(col) => write!(
                f,
                "cannot change \"{col}\" with UPDATE; DELETE and INSERT instead"
            ),
            Self::NotAnObject(col) => write!(f, "column \"{col}\" must be a JSON object"),
            Self::DataValueNotString(k) => write!(
                f,
                "data[{k:?}] must be a string (ConfigMap data values are strings)"
            ),
            Self::BadOldRaw(what) => write!(f, "cannot identify the row to write: {what}"),
        }
    }
}

impl std::error::Error for WriteError {}

fn cell_text<'a>(kind: Kind, new: &'a NewRow, column: &str) -> Option<&'a str> {
    match kind
        .column_index(column)
        .and_then(|i| new.cells.get(i))
        .and_then(Option::as_ref)
    {
        Some(Cell::Text(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn cell_json<'a>(kind: Kind, new: &'a NewRow, column: &str) -> Option<&'a serde_json::Value> {
    match kind
        .column_index(column)
        .and_then(|i| new.cells.get(i))
        .and_then(Option::as_ref)
    {
        Some(Cell::Json(v)) => Some(v),
        _ => None,
    }
}

/// Validates a `ConfigMap` `data` value: an object whose values are all strings.
fn check_configmap_data(data: &serde_json::Value) -> Result<(), WriteError> {
    let obj = data.as_object().ok_or(WriteError::NotAnObject("data"))?;
    if let Some((k, _)) = obj.iter().find(|(_, v)| !v.is_string()) {
        return Err(WriteError::DataValueNotString(k.clone()));
    }
    Ok(())
}

/// Extracts the identity of an existing object from its `raw` column.
pub fn identity_from_raw(raw: &serde_json::Value) -> Result<Identity, WriteError> {
    let name = str_at(raw, "/metadata/name")
        .filter(|s| !s.is_empty())
        .ok_or(WriteError::BadOldRaw("metadata.name missing"))?;
    let namespace = str_at(raw, "/metadata/namespace").unwrap_or_default();
    let resource_version = str_at(raw, "/metadata/resourceVersion")
        .filter(|s| !s.is_empty())
        .ok_or(WriteError::BadOldRaw("metadata.resourceVersion missing"))?;
    Ok(Identity {
        namespace: namespace.to_owned(),
        name: name.to_owned(),
        resource_version: resource_version.to_owned(),
    })
}

/// Applies the SQL-visible columns of `new` onto `base` for `kind`, returning
/// the object body. Shared by insert and update.
fn apply_columns(
    kind: Kind,
    base: serde_json::Value,
    new: &NewRow,
) -> Result<serde_json::Value, WriteError> {
    // If SQL supplied `raw`, it is the whole object; typed columns then overlay it.
    let mut body = match cell_json(kind, new, "raw") {
        Some(r) => {
            if !r.is_object() {
                return Err(WriteError::NotAnObject("raw"));
            }
            r.clone()
        }
        None => base,
    };
    if !body.is_object() {
        body = serde_json::json!({});
    }
    match kind {
        Kind::ConfigMaps => {
            if let Some(data) = cell_json(kind, new, "data") {
                check_configmap_data(data)?;
                body["data"] = data.clone();
            } else if body.get("data").is_none() {
                body["data"] = serde_json::json!({});
            } else {
                check_configmap_data(&body["data"])?;
            }
        }
        Kind::Pods => return Err(WriteError::ReadOnly(kind)),
    }
    Ok(body)
}

fn set_identity(body: &mut serde_json::Value, kind: Kind, id: &Identity) {
    let (group, version, k) = kind.gvk();
    let api_version = if group.is_empty() {
        version.to_owned()
    } else {
        format!("{group}/{version}")
    };
    body["apiVersion"] = serde_json::Value::String(api_version);
    body["kind"] = serde_json::Value::String(k.to_owned());
    if !body["metadata"].is_object() {
        body["metadata"] = serde_json::json!({});
    }
    body["metadata"]["name"] = serde_json::Value::String(id.name.clone());
    body["metadata"]["namespace"] = serde_json::Value::String(id.namespace.clone());
    if id.resource_version.is_empty() {
        if let Some(m) = body["metadata"].as_object_mut() {
            m.remove("resourceVersion");
        }
    } else {
        body["metadata"]["resourceVersion"] =
            serde_json::Value::String(id.resource_version.clone());
    }
}

/// Builds the body for `INSERT`. `name` and `namespace` are required; `data`
/// defaults to `{}`; `raw`, if given, is the base object.
pub fn insert_body(kind: Kind, new: &NewRow) -> Result<WriteBody, WriteError> {
    if !kind.writable() {
        return Err(WriteError::ReadOnly(kind));
    }
    let raw_name = str_at(
        cell_json(kind, new, "raw").unwrap_or(&serde_json::Value::Null),
        "/metadata/name",
    );
    let raw_ns = str_at(
        cell_json(kind, new, "raw").unwrap_or(&serde_json::Value::Null),
        "/metadata/namespace",
    );
    let name = cell_text(kind, new, "name")
        .or(raw_name)
        .ok_or(WriteError::MissingName)?
        .to_owned();
    let namespace = cell_text(kind, new, "namespace")
        .or(raw_ns)
        .ok_or(WriteError::MissingNamespace)?
        .to_owned();
    if !crate::quals::is_valid_k8s_name(&name) {
        return Err(WriteError::InvalidName("name", name));
    }
    if !crate::quals::is_valid_k8s_name(&namespace) {
        return Err(WriteError::InvalidName("namespace", namespace));
    }
    let id = Identity {
        namespace,
        name,
        resource_version: String::new(),
    };
    let mut body = apply_columns(kind, serde_json::json!({}), new)?;
    set_identity(&mut body, kind, &id);
    Ok(WriteBody { identity: id, body })
}

/// Builds the body for `UPDATE`: the old object (from the `raw` junk column as
/// last read) with the new tuple's columns applied, carrying the old
/// `resourceVersion` so the gateway can detect concurrent modification.
/// Changing `name` or `namespace` is rejected.
pub fn update_body(
    kind: Kind,
    old_raw: &serde_json::Value,
    new: &NewRow,
) -> Result<WriteBody, WriteError> {
    if !kind.writable() {
        return Err(WriteError::ReadOnly(kind));
    }
    let id = identity_from_raw(old_raw)?;
    if let Some(n) = cell_text(kind, new, "name") {
        if n != id.name {
            return Err(WriteError::IdentityChange("name"));
        }
    }
    if let Some(ns) = cell_text(kind, new, "namespace") {
        if ns != id.namespace {
            return Err(WriteError::IdentityChange("namespace"));
        }
    }
    // A `raw` equal to the old one means "not changed"; only a differing raw replaces the object.
    let new_for_apply = match cell_json(kind, new, "raw") {
        Some(r) if r == old_raw => NewRow {
            cells: new
                .cells
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if Some(i) == kind.column_index("raw") {
                        None
                    } else {
                        c.clone()
                    }
                })
                .collect(),
        },
        _ => new.clone(),
    };
    let mut body = apply_columns(kind, old_raw.clone(), &new_for_apply)?;
    set_identity(&mut body, kind, &id);
    Ok(WriteBody { identity: id, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const POD: &str = r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"web-0","namespace":"shop","resourceVersion":"42"},
        "spec":{"nodeName":"node-a","containers":[]},"status":{"phase":"Running"}}"#;

    fn cm_raw() -> serde_json::Value {
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"app","namespace":"shop","resourceVersion":"9","uid":"u1"},
               "data":{"LOG_LEVEL":"info"}})
    }

    fn new_row(kind: Kind, pairs: &[(&str, Option<Cell>)]) -> NewRow {
        let mut cells = vec![None; kind.columns().len()];
        for (col, cell) in pairs {
            cells[kind.column_index(col).expect("column")] = cell.clone();
        }
        NewRow { cells }
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "mirrors the Option<Cell> slot type for readable fixtures"
    )]
    fn t(s: &str) -> Option<Cell> {
        Some(Cell::Text(s.into()))
    }

    #[test]
    fn parse_and_metadata() {
        assert_eq!(Kind::parse("pods"), Some(Kind::Pods));
        assert_eq!(Kind::parse(" configmaps "), Some(Kind::ConfigMaps));
        assert_eq!(Kind::parse("deployments"), None);
        assert_eq!(Kind::Pods.gvk(), ("", "v1", "Pod"));
        assert!(!Kind::Pods.writable());
        assert!(Kind::ConfigMaps.writable());
        assert_eq!(Kind::ConfigMaps.column_index("data"), Some(2));
        assert_eq!(Kind::ConfigMaps.column_index("phase"), None);
        for k in [Kind::Pods, Kind::ConfigMaps] {
            assert_eq!(
                k.columns().iter().filter(|c| c.name == "raw").count(),
                1,
                "{k:?} must expose raw"
            );
        }
    }

    #[test]
    fn decodes_pod() {
        let r = Kind::Pods
            .decode(POD.as_bytes(), MAX_OBJECT_BYTES)
            .expect("valid");
        assert_eq!(r.name, "web-0");
        assert_eq!(r.namespace, "shop");
        assert_eq!(r.resource_version, "42");
        assert_eq!(
            r.cell(Kind::Pods, "phase"),
            Some(&Cell::Text("Running".into()))
        );
        assert_eq!(
            r.cell(Kind::Pods, "node"),
            Some(&Cell::Text("node-a".into()))
        );
        assert!(matches!(r.cell(Kind::Pods, "raw"), Some(Cell::Json(v)) if v["kind"] == "Pod"));
        assert_eq!(r.cell(Kind::Pods, "data"), None);
        // optional fields absent / wrong type → NULL
        let r = Kind::Pods
            .decode(
                br#"{"metadata":{"name":"p"},"status":{"phase":7}}"#,
                MAX_OBJECT_BYTES,
            )
            .expect("valid");
        assert_eq!(r.cell(Kind::Pods, "phase"), None);
        assert_eq!(r.cell(Kind::Pods, "node"), None);
        assert_eq!(r.namespace, "");
    }

    #[test]
    fn decodes_configmap_with_empty_data_default() {
        let r = Row::from_value(Kind::ConfigMaps, &cm_raw()).expect("valid");
        assert_eq!(
            r.cell(Kind::ConfigMaps, "data"),
            Some(&Cell::Json(json!({"LOG_LEVEL":"info"})))
        );
        let r = Kind::ConfigMaps
            .decode(
                br#"{"metadata":{"name":"x","namespace":"n"}}"#,
                MAX_OBJECT_BYTES,
            )
            .expect("valid");
        assert_eq!(
            r.cell(Kind::ConfigMaps, "data"),
            Some(&Cell::Json(json!({})))
        );
        let r = Kind::ConfigMaps
            .decode(
                br#"{"metadata":{"name":"x"},"data":"notobj"}"#,
                MAX_OBJECT_BYTES,
            )
            .expect("valid");
        assert_eq!(
            r.cell(Kind::ConfigMaps, "data"),
            Some(&Cell::Json(json!({})))
        );
    }

    #[test]
    fn decode_rejects_bad_input_without_echoing_it() {
        assert_eq!(
            Kind::Pods.decode(POD.as_bytes(), 10),
            Err(DecodeError::TooLarge {
                bytes: POD.len(),
                max: 10
            })
        );
        assert!(matches!(
            Kind::Pods.decode(b"{not json", MAX_OBJECT_BYTES),
            Err(DecodeError::Json(_))
        ));
        assert_eq!(
            Kind::Pods.decode(b"[]", MAX_OBJECT_BYTES),
            Err(DecodeError::Shape("top level is not an object"))
        );
        assert_eq!(
            Kind::Pods.decode(b"{}", MAX_OBJECT_BYTES),
            Err(DecodeError::Shape("metadata is missing or not an object"))
        );
        assert_eq!(
            Kind::Pods.decode(br#"{"metadata":{"name":""}}"#, MAX_OBJECT_BYTES),
            Err(DecodeError::Shape("metadata.name is missing or empty"))
        );
        let msg = Kind::Pods
            .decode(b"{\"secret\":\"hunter2\"", MAX_OBJECT_BYTES)
            .expect_err("bad")
            .to_string();
        assert!(!msg.contains("hunter2"), "{msg}");
    }

    #[test]
    fn insert_builds_full_object() {
        let new = new_row(
            Kind::ConfigMaps,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", Some(Cell::Json(json!({"A":"1"})))),
            ],
        );
        let w = insert_body(Kind::ConfigMaps, &new).expect("valid");
        assert_eq!(
            w.identity,
            Identity {
                namespace: "shop".into(),
                name: "app".into(),
                resource_version: String::new()
            }
        );
        assert_eq!(
            w.body,
            json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"app","namespace":"shop"},"data":{"A":"1"}})
        );
    }

    #[test]
    fn insert_defaults_and_raw_base() {
        let new = new_row(
            Kind::ConfigMaps,
            &[("name", t("app")), ("namespace", t("shop"))],
        );
        assert_eq!(
            insert_body(Kind::ConfigMaps, &new).expect("valid").body["data"],
            json!({})
        );
        // raw supplies identity and extra fields; typed data overlays it; rv is stripped.
        let raw = json!({"metadata":{"name":"app","namespace":"shop","labels":{"x":"y"},"resourceVersion":"5"},"data":{"OLD":"1"}});
        let new = new_row(
            Kind::ConfigMaps,
            &[
                ("raw", Some(Cell::Json(raw))),
                ("data", Some(Cell::Json(json!({"NEW":"2"})))),
            ],
        );
        let w = insert_body(Kind::ConfigMaps, &new).expect("valid");
        assert_eq!(w.identity.name, "app");
        assert_eq!(w.body["metadata"]["labels"], json!({"x":"y"}));
        assert_eq!(w.body["data"], json!({"NEW":"2"}));
        assert!(w.body["metadata"].get("resourceVersion").is_none());
    }

    #[test]
    fn insert_validation_errors() {
        let e = |pairs: &[(&str, Option<Cell>)]| {
            insert_body(Kind::ConfigMaps, &new_row(Kind::ConfigMaps, pairs)).expect_err("err")
        };
        assert_eq!(e(&[("namespace", t("shop"))]), WriteError::MissingName);
        assert_eq!(e(&[("name", t("app"))]), WriteError::MissingNamespace);
        assert_eq!(
            e(&[("name", t("Bad Name")), ("namespace", t("shop"))]),
            WriteError::InvalidName("name", "Bad Name".into())
        );
        assert_eq!(
            e(&[("name", t("app")), ("namespace", t("-x"))]),
            WriteError::InvalidName("namespace", "-x".into())
        );
        assert_eq!(
            e(&[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", Some(Cell::Json(json!([1]))))
            ]),
            WriteError::NotAnObject("data")
        );
        assert_eq!(
            e(&[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", Some(Cell::Json(json!({"k": 1}))))
            ]),
            WriteError::DataValueNotString("k".into())
        );
        assert_eq!(
            e(&[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("raw", Some(Cell::Json(json!("str"))))
            ]),
            WriteError::NotAnObject("raw")
        );
        let pods = new_row(Kind::Pods, &[("name", t("p")), ("namespace", t("n"))]);
        assert_eq!(
            insert_body(Kind::Pods, &pods),
            Err(WriteError::ReadOnly(Kind::Pods))
        );
    }

    #[test]
    fn update_applies_data_and_keeps_identity_and_rv() {
        // Postgres hands the full new tuple: unchanged columns carry old values.
        let new = new_row(
            Kind::ConfigMaps,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", Some(Cell::Json(json!({"LOG_LEVEL":"debug"})))),
                ("raw", Some(Cell::Json(cm_raw()))),
            ],
        );
        let w = update_body(Kind::ConfigMaps, &cm_raw(), &new).expect("valid");
        assert_eq!(
            w.identity,
            Identity {
                namespace: "shop".into(),
                name: "app".into(),
                resource_version: "9".into()
            }
        );
        assert_eq!(w.body["data"], json!({"LOG_LEVEL":"debug"}));
        assert_eq!(
            w.body["metadata"]["uid"], "u1",
            "untouched metadata preserved"
        );
        assert_eq!(w.body["metadata"]["resourceVersion"], "9");
    }

    #[test]
    fn update_with_changed_raw_replaces_object_but_pins_identity() {
        let mut raw2 = cm_raw();
        raw2["metadata"]["labels"] = json!({"tier":"web"});
        raw2["metadata"]["name"] = json!("evil"); // must be overridden
        raw2["metadata"]["resourceVersion"] = json!("999"); // must be overridden by the read rv
        let new = new_row(
            Kind::ConfigMaps,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", Some(Cell::Json(json!({"LOG_LEVEL":"info"})))),
                ("raw", Some(Cell::Json(raw2))),
            ],
        );
        let w = update_body(Kind::ConfigMaps, &cm_raw(), &new).expect("valid");
        assert_eq!(w.body["metadata"]["labels"], json!({"tier":"web"}));
        assert_eq!(w.body["metadata"]["name"], "app");
        assert_eq!(w.body["metadata"]["resourceVersion"], "9");
        assert_eq!(w.body["kind"], "ConfigMap");
    }

    #[test]
    fn update_rejects_identity_change_and_bad_old_raw() {
        let new = new_row(
            Kind::ConfigMaps,
            &[("name", t("renamed")), ("namespace", t("shop"))],
        );
        assert_eq!(
            update_body(Kind::ConfigMaps, &cm_raw(), &new),
            Err(WriteError::IdentityChange("name"))
        );
        let new = new_row(
            Kind::ConfigMaps,
            &[("name", t("app")), ("namespace", t("other"))],
        );
        assert_eq!(
            update_body(Kind::ConfigMaps, &cm_raw(), &new),
            Err(WriteError::IdentityChange("namespace"))
        );
        let new = new_row(Kind::ConfigMaps, &[]);
        assert_eq!(
            update_body(Kind::ConfigMaps, &json!({"metadata":{"name":"app"}}), &new),
            Err(WriteError::BadOldRaw("metadata.resourceVersion missing"))
        );
        assert_eq!(
            update_body(Kind::ConfigMaps, &json!({}), &new),
            Err(WriteError::BadOldRaw("metadata.name missing"))
        );
        assert_eq!(
            update_body(Kind::Pods, &cm_raw(), &new),
            Err(WriteError::ReadOnly(Kind::Pods))
        );
    }

    #[test]
    fn identity_from_raw_reads_metadata() {
        let id = identity_from_raw(&cm_raw()).expect("valid");
        assert_eq!(
            id,
            Identity {
                namespace: "shop".into(),
                name: "app".into(),
                resource_version: "9".into()
            }
        );
    }
}
