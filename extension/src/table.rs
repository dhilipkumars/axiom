//! A foreign table's resolved shape, and the decode/write paths driven by it.
//!
//! Phases 1-3 kept one static column list per hardcoded kind here. Phase 4
//! resolves the columns of the *declared* table instead: whatever a
//! `CREATE FOREIGN TABLE` names, matched against the projection rule in
//! [`crate::schema`]. That is what makes an arbitrary CRD servable without a
//! Rust change per kind.
//!
//! Object JSON is treated as untrusted throughout: size-bounded and
//! shape-checked before anything is trusted (docs/RULES.md §3).

use std::fmt;

use crate::resource::Resource;
use crate::schema::{self, Projection, SqlType};

/// Upper bound on one object's JSON. Matches the gRPC default max message size
/// so nothing larger can arrive anyway; enforced here so the FDW never trusts
/// the transport for this, and so a hostile object cannot exhaust the
/// shared-memory segment.
pub const MAX_OBJECT_BYTES: usize = 4 * 1024 * 1024;

/// One column of a declared foreign table, resolved against its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedColumn {
    /// Column name as declared in the DDL.
    pub name: String,
    /// SQL type the column must be declared with.
    pub sql_type: SqlType,
    /// How the value is read out of an object.
    pub projection: Projection,
    /// Whether SQL may write it.
    pub writable: bool,
}

/// The resolved shape of one foreign table.
///
/// Built once per scan or modify from the relation's tuple descriptor, then
/// used to decode every row, so the per-row path does no name matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSchema {
    /// The kind this table maps to.
    pub resource: Resource,
    /// Whether INSERT/UPDATE/DELETE are offered.
    pub writable: bool,
    /// Columns in attribute order. `None` marks a dropped column, whose
    /// attribute number still occupies a slot in the tuple descriptor.
    pub columns: Vec<Option<ResolvedColumn>>,
}

impl TableSchema {
    /// Resolves declared column names against a kind.
    ///
    /// `declared` is one entry per attribute in order, `None` for dropped
    /// columns. Every name resolves; a name matching no promoted column becomes
    /// a top-level lookup that reads NULL when the kind has no such field.
    pub fn resolve(resource: Resource, writable: bool, declared: &[Option<&str>]) -> Self {
        let columns = declared
            .iter()
            .map(|d| {
                d.map(|name| {
                    let c = schema::column(&resource, name);
                    ResolvedColumn {
                        name: name.to_owned(),
                        sql_type: c.sql_type,
                        projection: c.projection,
                        writable: c.writable,
                    }
                })
            })
            .collect();
        Self {
            resource,
            writable,
            columns,
        }
    }

    /// The resolved column with this name, if the table declares it.
    pub fn column(&self, name: &str) -> Option<&ResolvedColumn> {
        self.columns.iter().flatten().find(|c| c.name == name)
    }

    /// Attribute index of a named column, if declared.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.as_ref().is_some_and(|c| c.name == name))
    }

    /// Decodes one object into a row aligned with [`TableSchema::columns`].
    pub fn decode(&self, bytes: &[u8], max_bytes: usize) -> Result<Row, DecodeError> {
        if bytes.len() > max_bytes {
            return Err(DecodeError::TooLarge {
                bytes: bytes.len(),
                max: max_bytes,
            });
        }
        let raw: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| DecodeError::Json(e.to_string()))?;
        self.row_from_value(&raw)
    }

    /// Builds a row from an already-parsed object.
    pub fn row_from_value(&self, raw: &serde_json::Value) -> Result<Row, DecodeError> {
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
        let cells = self
            .columns
            .iter()
            .map(|c| c.as_ref().and_then(|c| project(raw, c)))
            .collect();
        Ok(Row {
            name,
            namespace,
            resource_version,
            cells,
        })
    }
}

/// Reads one column's value out of an object. `None` is SQL NULL.
fn project(raw: &serde_json::Value, column: &ResolvedColumn) -> Option<Cell> {
    match column.projection {
        Projection::Text(pointer) => str_at(raw, pointer).map(|s| Cell::Text(s.to_owned())),
        Projection::Scalar(pointer) => raw.pointer(pointer).and_then(|v| match v {
            serde_json::Value::String(s) => Some(Cell::Text(s.clone())),
            serde_json::Value::Number(n) => Some(Cell::Text(n.to_string())),
            serde_json::Value::Bool(b) => Some(Cell::Text(b.to_string())),
            // Objects, arrays and null have no faithful text rendering here.
            _ => None,
        }),
        Projection::Json {
            pointer,
            empty_object,
        } => {
            let v = raw.pointer(pointer).filter(|v| !v.is_null());
            match (v, empty_object) {
                (Some(v), _) => Some(Cell::Json(v.clone())),
                (None, true) => Some(Cell::Json(serde_json::json!({}))),
                (None, false) => None,
            }
        }
        Projection::Raw => Some(Cell::Json(raw.clone())),
        Projection::TopLevel => schema::top_level_field(raw, &column.name)
            .filter(|v| !v.is_null())
            .map(|v| Cell::Json(v.clone())),
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

/// One decoded object, values aligned with [`TableSchema::columns`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// `metadata.name`.
    pub name: String,
    /// `metadata.namespace` (empty for cluster-scoped kinds).
    pub namespace: String,
    /// `metadata.resourceVersion`, needed for optimistic-concurrency writes.
    pub resource_version: String,
    /// Column values in attribute order.
    pub cells: Vec<Option<Cell>>,
}

impl Row {
    /// The cell for a named column, if the table declares it.
    pub fn cell<'a>(&'a self, schema: &TableSchema, column: &str) -> Option<&'a Cell> {
        schema
            .index_of(column)
            .and_then(|i| self.cells.get(i))
            .and_then(Option::as_ref)
    }
}

/// Why an object could not be decoded. Never echoes the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Payload exceeds the configured bound.
    TooLarge {
        /// Actual size.
        bytes: usize,
        /// Permitted size.
        max: usize,
    },
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

// --- write path -------------------------------------------------------------------

/// One SQL-side column value for a write. Distinguishes a column the foreign
/// table does not declare from an explicit SQL NULL: `SET data = NULL` must
/// mean "clear", and `SET name = NULL` must be an error, not "keep the old".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NewCell {
    /// The foreign table has no such column.
    #[default]
    Undeclared,
    /// Declared, and SQL supplied NULL.
    Null,
    /// Declared, with a value.
    Value(Cell),
}

/// Column values supplied by SQL for an INSERT, or the full new tuple of an
/// UPDATE (Postgres fills untouched columns from the old row), aligned with
/// [`TableSchema::columns`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewRow {
    /// One entry per attribute of the table.
    pub cells: Vec<NewCell>,
}

impl NewRow {
    /// A row with every column undeclared.
    pub fn undeclared(schema: &TableSchema) -> Self {
        Self {
            cells: vec![NewCell::Undeclared; schema.columns.len()],
        }
    }
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
    /// The table is read-only.
    ReadOnly(String),
    /// `name` is NULL or missing on INSERT.
    MissingName,
    /// `namespace` is NULL or missing on INSERT.
    MissingNamespace,
    /// A column that cannot be NULL was set to NULL.
    NullNotAllowed(String),
    /// A name/namespace is not a valid Kubernetes name.
    InvalidName(&'static str, String),
    /// UPDATE tried to change `name` or `namespace`.
    IdentityChange(&'static str),
    /// A jsonb column that must be an object is not one.
    NotAnObject(String),
    /// `ConfigMap` `data` values must be strings.
    DataValueNotString(String),
    /// SQL tried to set a column the API server manages.
    NotWritable(String),
    /// The old `raw` value lacks an identity/resourceVersion.
    BadOldRaw(&'static str),
    /// `raw` names a different apiVersion/kind from the table's: (found, want).
    WrongKind(String, String),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadOnly(r) => write!(f, "foreign tables on {r} are read-only"),
            Self::MissingName => write!(f, "column \"name\" is required and must not be NULL"),
            Self::MissingNamespace => {
                write!(f, "column \"namespace\" is required and must not be NULL")
            }
            Self::NullNotAllowed(col) => write!(f, "column \"{col}\" cannot be set to NULL"),
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
            Self::NotWritable(col) => write!(
                f,
                "column \"{col}\" is managed by the Kubernetes API server and cannot be written"
            ),
            Self::BadOldRaw(what) => write!(f, "cannot identify the row to write: {what}"),
            Self::WrongKind(found, want) => {
                write!(f, "raw describes {found}, but this table holds {want}")
            }
        }
    }
}

impl std::error::Error for WriteError {}

fn cell<'a>(schema: &TableSchema, new: &'a NewRow, column: &str) -> &'a NewCell {
    static UNDECLARED: NewCell = NewCell::Undeclared;
    schema
        .index_of(column)
        .and_then(|i| new.cells.get(i))
        .unwrap_or(&UNDECLARED)
}

fn cell_text<'a>(schema: &TableSchema, new: &'a NewRow, column: &str) -> Option<&'a str> {
    match cell(schema, new, column) {
        NewCell::Value(Cell::Text(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn is_null(schema: &TableSchema, new: &NewRow, column: &str) -> bool {
    matches!(cell(schema, new, column), NewCell::Null)
}

/// Validates a `ConfigMap` `data` value: an object whose values are all strings.
///
/// Kept as a kind-specific check because the failure is far clearer here than
/// as a 422 from the API server, and because `data` is the column most SQL
/// actually writes. Every other kind relies on the API server's own validation.
fn check_configmap_data(data: &serde_json::Value) -> Result<(), WriteError> {
    let obj = data
        .as_object()
        .ok_or_else(|| WriteError::NotAnObject("data".to_owned()))?;
    if let Some((k, _)) = obj.iter().find(|(_, v)| !v.is_string()) {
        return Err(WriteError::DataValueNotString(k.clone()));
    }
    Ok(())
}

fn is_configmap(r: &Resource) -> bool {
    r.group.is_empty() && r.kind.as_str() == "ConfigMap"
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

fn set_identity(body: &mut serde_json::Value, resource: &Resource, id: &Identity) {
    body["apiVersion"] = serde_json::Value::String(resource.api_version());
    body["kind"] = serde_json::Value::String(resource.kind.to_string());
    if !body["metadata"].is_object() {
        body["metadata"] = serde_json::json!({});
    }
    body["metadata"]["name"] = serde_json::Value::String(id.name.clone());
    if resource.namespaced {
        body["metadata"]["namespace"] = serde_json::Value::String(id.namespace.clone());
    } else if let Some(m) = body["metadata"].as_object_mut() {
        // A cluster-scoped object has no namespace, and the API server refuses
        // one that claims it does -- which a hand-written raw can.
        m.remove("namespace");
    }
    if id.resource_version.is_empty() {
        if let Some(m) = body["metadata"].as_object_mut() {
            m.remove("resourceVersion");
        }
    } else {
        body["metadata"]["resourceVersion"] =
            serde_json::Value::String(id.resource_version.clone());
    }
}

/// Sets or removes the value at a JSON pointer, creating intermediate objects.
///
/// Only the pointers in [`crate::schema`] reach this: `/data` at the top level
/// and `/metadata/labels` and `/metadata/annotations` one level down. A segment
/// that exists but is not an object is replaced, since the column's type is
/// what the SQL user asked the field to be.
fn set_at_pointer(body: &mut serde_json::Value, pointer: &str, value: Option<&serde_json::Value>) {
    let segments: Vec<&str> = pointer.split('/').filter(|s| !s.is_empty()).collect();
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut cursor = body;
    for seg in parents {
        if !cursor[*seg].is_object() {
            cursor[*seg] = serde_json::json!({});
        }
        cursor = &mut cursor[*seg];
    }
    if !cursor.is_object() {
        *cursor = serde_json::json!({});
    }
    match value {
        Some(v) => cursor[*last] = v.clone(),
        None => {
            if let Some(m) = cursor.as_object_mut() {
                m.remove(*last);
            }
        }
    }
}

/// Writes one column's SQL value into the body being built.
fn apply_column(
    body: &mut serde_json::Value,
    resource: &Resource,
    col: &ResolvedColumn,
    value: Option<&serde_json::Value>,
) -> Result<(), WriteError> {
    // `data` on a ConfigMap is validated here so the error names the offending
    // key instead of arriving as an opaque 422.
    if col.name == "data" && is_configmap(resource) {
        if let Some(v) = value {
            check_configmap_data(v)?;
        }
    }
    match col.projection {
        Projection::Json { pointer, .. } => set_at_pointer(body, pointer, value),
        Projection::TopLevel => {
            let key = schema::top_level_key(Some(body), &col.name);
            match value {
                Some(v) => body[key] = v.clone(),
                None => {
                    if let Some(m) = body.as_object_mut() {
                        m.remove(&key);
                    }
                }
            }
        }
        // Text, Scalar and Raw projections are never applied column-wise: name
        // and namespace go through set_identity, raw is the base object, and
        // every other text projection is server-managed.
        Projection::Text(_) | Projection::Scalar(_) | Projection::Raw => {}
    }
    Ok(())
}

/// Rejects an attempt to write a server-managed column.
///
/// On UPDATE Postgres supplies the whole new tuple, so a read-only column
/// arrives carrying the value the scan read. Only a *different* value is a
/// write attempt; an unchanged one is just the tuple being complete.
fn check_read_only(
    schema: &TableSchema,
    new: &NewRow,
    old: Option<&serde_json::Value>,
) -> Result<(), WriteError> {
    for (i, col) in schema.columns.iter().enumerate() {
        let Some(col) = col else { continue };
        if col.writable || col.name == "raw" {
            continue;
        }
        let supplied = match new.cells.get(i) {
            Some(NewCell::Value(c)) => Some(c),
            Some(NewCell::Null | NewCell::Undeclared) | None => None,
        };
        let Some(supplied) = supplied else { continue };
        let old_cell = old.and_then(|o| project(o, col));
        if old_cell.as_ref() != Some(supplied) {
            return Err(WriteError::NotWritable(col.name.clone()));
        }
    }
    Ok(())
}

/// Refuses a `raw` that says it is some other kind of object.
///
/// `set_identity` stamps the table's apiVersion and kind onto whatever it is
/// given, so without this a `Deployment` manifest inserted into a table of
/// `ConfigMap`s would quietly become a `ConfigMap` carrying a `Deployment`'s
/// fields. A `raw` that omits them is fine: the table supplies them.
fn check_raw_kind(raw: &serde_json::Value, resource: &Resource) -> Result<(), WriteError> {
    let want_api = resource.api_version();
    let want_kind = resource.kind.to_string();
    let api = str_at(raw, "/apiVersion");
    let kind = str_at(raw, "/kind");
    if api.is_some_and(|a| a != want_api) || kind.is_some_and(|k| k != want_kind) {
        return Err(WriteError::WrongKind(
            format!(
                "{} {}",
                api.unwrap_or(&want_api),
                kind.unwrap_or(&want_kind)
            ),
            format!("{want_api} {want_kind}"),
        ));
    }
    Ok(())
}

/// Metadata the API server assigns when it creates an object. A `raw` read
/// from another object carries them, and using that as a template is the
/// point of honouring `raw` on INSERT, so they are dropped rather than sent:
/// a create cannot choose them, and some make it fail outright.
const SERVER_ASSIGNED_METADATA: &[&str] = &[
    "uid",
    "resourceVersion",
    "creationTimestamp",
    "generation",
    "managedFields",
    "deletionTimestamp",
    "deletionGracePeriodSeconds",
    "selfLink",
];

/// Builds the body for `INSERT`.
///
/// `name` is required (from the column or from `raw`), `namespace` too for a
/// namespaced kind. `raw`, if given, is the base object, so a whole manifest
/// can be inserted as one `jsonb` value.
///
/// Precedence when both are given: a non-NULL typed column overrides the same
/// field in `raw`, as it does on UPDATE, which is what lets `raw` serve as a
/// template (`INSERT ... (name, raw) VALUES ('copy', (SELECT raw ...))`). A
/// NULL column leaves `raw` alone. Postgres fills every column an INSERT does
/// not mention with NULL, so NULL cannot mean "clear this": treating it that
/// way wiped `raw`'s labels, annotations and data and reported success (#78).
pub fn insert_body(schema: &TableSchema, new: &NewRow) -> Result<WriteBody, WriteError> {
    if !schema.writable {
        return Err(WriteError::ReadOnly(schema.resource.to_string()));
    }
    check_read_only(schema, new, None)?;

    // On INSERT a NULL `raw` is the same as not providing one: there is no
    // existing object it could be clearing.
    let raw = match cell(schema, new, "raw") {
        NewCell::Value(Cell::Json(r)) => Some(r),
        _ => None,
    };
    if let Some(r) = raw {
        if !r.is_object() {
            return Err(WriteError::NotAnObject("raw".to_owned()));
        }
        check_raw_kind(r, &schema.resource)?;
    }
    let raw_name = raw.and_then(|r| str_at(r, "/metadata/name"));
    let raw_ns = raw.and_then(|r| str_at(r, "/metadata/namespace"));
    let name = cell_text(schema, new, "name")
        .or(raw_name)
        .ok_or(WriteError::MissingName)?
        .to_owned();
    let namespace = if schema.resource.namespaced {
        cell_text(schema, new, "namespace")
            .or(raw_ns)
            .ok_or(WriteError::MissingNamespace)?
            .to_owned()
    } else {
        String::new()
    };
    if !crate::quals::is_valid_k8s_name(&name) {
        return Err(WriteError::InvalidName("name", name));
    }
    if schema.resource.namespaced && !crate::quals::is_valid_k8s_name(&namespace) {
        return Err(WriteError::InvalidName("namespace", namespace));
    }

    let mut body = raw.cloned().unwrap_or_else(|| serde_json::json!({}));
    if let Some(m) = body.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        for key in SERVER_ASSIGNED_METADATA {
            m.remove(*key);
        }
    }
    for (i, col) in schema.columns.iter().enumerate() {
        let Some(col) = col else { continue };
        if !col.writable || col.name == "name" || col.name == "namespace" {
            continue;
        }
        // NULL is how Postgres fills a column the INSERT did not mention;
        // there is no existing object for it to clear, so only values apply.
        if let Some(NewCell::Value(Cell::Json(v))) = new.cells.get(i) {
            apply_column(&mut body, &schema.resource, col, Some(v))?;
        }
    }
    // A ConfigMap without `data` gets an empty map so `data ? 'k'` is false
    // rather than NULL on the row that comes back; `data` that arrived through
    // raw gets the check a typed `data` column already had.
    if is_configmap(&schema.resource) {
        match body.get("data") {
            Some(d) => check_configmap_data(d)?,
            None => body["data"] = serde_json::json!({}),
        }
    }

    let id = Identity {
        namespace,
        name,
        resource_version: String::new(),
    };
    set_identity(&mut body, &schema.resource, &id);
    Ok(WriteBody { identity: id, body })
}

/// Builds the body for `UPDATE`: the old object (from the `raw` junk column as
/// last read) with the new tuple's columns applied, carrying the old
/// `resourceVersion` so the gateway can detect concurrent modification.
///
/// Postgres hands the *full* new tuple, so "did SQL change this column" is
/// decided by comparing against the old object: a column equal to what the old
/// object projects is left alone, which is what lets
/// `SET raw = jsonb_set(raw, ...)` change a field inside `raw` without the
/// untouched typed column overwriting it. Explicit NULLs are honoured:
/// `SET spec = NULL` clears, `SET name/namespace/raw = NULL` is an error.
/// Changing `name` or `namespace` is rejected.
pub fn update_body(
    schema: &TableSchema,
    old_raw: &serde_json::Value,
    new: &NewRow,
) -> Result<WriteBody, WriteError> {
    if !schema.writable {
        return Err(WriteError::ReadOnly(schema.resource.to_string()));
    }
    let id = identity_from_raw(old_raw)?;
    for col in ["name", "namespace", "raw"] {
        if is_null(schema, new, col) {
            return Err(WriteError::NullNotAllowed(col.to_owned()));
        }
    }
    if cell_text(schema, new, "name").is_some_and(|n| n != id.name) {
        return Err(WriteError::IdentityChange("name"));
    }
    if cell_text(schema, new, "namespace").is_some_and(|ns| ns != id.namespace) {
        return Err(WriteError::IdentityChange("namespace"));
    }
    check_read_only(schema, new, Some(old_raw))?;

    // A changed `raw` replaces the base wholesale; otherwise the old object is
    // the base and the typed columns are applied over it.
    let mut body = match cell(schema, new, "raw") {
        NewCell::Value(Cell::Json(r)) if r != old_raw => {
            if !r.is_object() {
                return Err(WriteError::NotAnObject("raw".to_owned()));
            }
            check_raw_kind(r, &schema.resource)?;
            r.clone()
        }
        _ => old_raw.clone(),
    };
    if !body.is_object() {
        body = serde_json::json!({});
    }

    for (i, col) in schema.columns.iter().enumerate() {
        let Some(col) = col else { continue };
        if !col.writable || col.name == "name" || col.name == "namespace" {
            continue;
        }
        let old_cell = project(old_raw, col);
        match new.cells.get(i) {
            Some(NewCell::Value(c)) if Some(c) != old_cell.as_ref() => {
                let Cell::Json(v) = c else { continue };
                apply_column(&mut body, &schema.resource, col, Some(v))?;
            }
            Some(NewCell::Null) if old_cell.is_some() => {
                apply_column(&mut body, &schema.resource, col, None)?;
            }
            _ => {}
        }
    }
    // Keep the ConfigMap invariant on a body whose data came through `raw`.
    if is_configmap(&schema.resource) {
        match body.get("data") {
            Some(d) => check_configmap_data(d)?,
            None => body["data"] = serde_json::json!({}),
        }
    }

    set_identity(&mut body, &schema.resource, &id);
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

    /// The Phase 1 Pods table: `(name, namespace, phase, node, raw)`.
    fn pods() -> TableSchema {
        let r = Resource::new("", "v1", "Pod", "pods", true).expect("valid");
        TableSchema::resolve(
            r,
            false,
            &[
                Some("name"),
                Some("namespace"),
                Some("phase"),
                Some("node"),
                Some("raw"),
            ],
        )
    }

    /// The Phase 2 `ConfigMaps` table: `(name, namespace, data, raw)`.
    fn configmaps() -> TableSchema {
        let r = Resource::new("", "v1", "ConfigMap", "configmaps", true).expect("valid");
        TableSchema::resolve(
            r,
            true,
            &[Some("name"), Some("namespace"), Some("data"), Some("raw")],
        )
    }

    /// A CRD table exercising the generic path: promoted metadata, top-level
    /// jsonb columns, and raw.
    fn widgets() -> TableSchema {
        let r = Resource::new("example.com", "v1", "Widget", "widgets", true).expect("valid");
        TableSchema::resolve(
            r,
            true,
            &[
                Some("name"),
                Some("namespace"),
                Some("labels"),
                Some("spec"),
                Some("status"),
                Some("raw"),
            ],
        )
    }

    /// Builds a row where listed columns are declared (`Some` = value, `None` = SQL NULL)
    /// and everything else is undeclared.
    fn new_row(schema: &TableSchema, pairs: &[(&str, Option<Cell>)]) -> NewRow {
        let mut row = NewRow::undeclared(schema);
        for (col, cell) in pairs {
            row.cells[schema.index_of(col).expect("column")] =
                cell.clone().map_or(NewCell::Null, NewCell::Value);
        }
        row
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "mirrors the Option<Cell> fixture shape (None = SQL NULL)"
    )]
    fn j(v: serde_json::Value) -> Option<Cell> {
        Some(Cell::Json(v))
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "mirrors the Option<Cell> slot type for readable fixtures"
    )]
    fn t(s: &str) -> Option<Cell> {
        Some(Cell::Text(s.into()))
    }

    // --- resolution -------------------------------------------------------------

    #[test]
    fn resolve_maps_declared_columns_and_keeps_attribute_order() {
        let s = pods();
        assert_eq!(s.columns.len(), 5);
        assert_eq!(s.index_of("phase"), Some(2));
        assert_eq!(s.index_of("raw"), Some(4));
        assert_eq!(s.index_of("nonexistent"), None);
        assert_eq!(s.column("phase").expect("declared").sql_type, SqlType::Text);
        assert_eq!(s.column("raw").expect("declared").sql_type, SqlType::Jsonb);
    }

    #[test]
    fn resolve_keeps_a_slot_for_dropped_columns() {
        let r = Resource::new("", "v1", "Pod", "pods", true).expect("valid");
        let s = TableSchema::resolve(r, false, &[Some("name"), None, Some("raw")]);
        assert_eq!(
            s.columns.len(),
            3,
            "a dropped column still occupies an attnum"
        );
        assert!(s.columns[1].is_none());
        assert_eq!(s.index_of("raw"), Some(2));
    }

    #[test]
    fn resolve_accepts_any_column_name_for_a_crd() {
        // No discovery happens at DDL time, so an unknown name is a top-level
        // lookup rather than an error; it reads NULL if the kind lacks it.
        let s = widgets();
        assert_eq!(
            s.column("spec").expect("declared").projection,
            Projection::TopLevel
        );
    }

    // --- decode -----------------------------------------------------------------

    #[test]
    fn decodes_pod() {
        let s = pods();
        let r = s.decode(POD.as_bytes(), MAX_OBJECT_BYTES).expect("valid");
        assert_eq!(r.name, "web-0");
        assert_eq!(r.namespace, "shop");
        assert_eq!(r.resource_version, "42");
        assert_eq!(r.cell(&s, "phase"), Some(&Cell::Text("Running".into())));
        assert_eq!(r.cell(&s, "node"), Some(&Cell::Text("node-a".into())));
        assert!(matches!(r.cell(&s, "raw"), Some(Cell::Json(v)) if v["kind"] == "Pod"));
        // Optional fields absent or of the wrong type read as NULL.
        let r = s
            .decode(
                br#"{"metadata":{"name":"p"},"status":{"phase":7}}"#,
                MAX_OBJECT_BYTES,
            )
            .expect("valid");
        assert_eq!(r.cell(&s, "phase"), None);
        assert_eq!(r.cell(&s, "node"), None);
        assert_eq!(r.namespace, "");
    }

    #[test]
    fn decodes_configmap_with_empty_data_default() {
        let s = configmaps();
        let r = s.row_from_value(&cm_raw()).expect("valid");
        assert_eq!(
            r.cell(&s, "data"),
            Some(&Cell::Json(json!({"LOG_LEVEL":"info"})))
        );
        let r = s
            .decode(
                br#"{"metadata":{"name":"x","namespace":"n"}}"#,
                MAX_OBJECT_BYTES,
            )
            .expect("valid");
        assert_eq!(
            r.cell(&s, "data"),
            Some(&Cell::Json(json!({}))),
            "absent data reads as {{}} so `data ? 'k'` is false, not NULL"
        );
    }

    #[test]
    fn decodes_deployment_replica_counts_from_json_numbers() {
        let r = Resource::new("apps", "v1", "Deployment", "deployments", true).expect("valid");
        let s = TableSchema::resolve(
            r,
            true,
            &[
                Some("name"),
                Some("replicas"),
                Some("ready_replicas"),
                Some("available_replicas"),
                Some("updated_replicas"),
            ],
        );
        let obj = json!({
            "metadata": {"name": "api", "namespace": "shop"},
            "spec": {"replicas": 3},
            "status": {"readyReplicas": 2, "availableReplicas": 2}
        });
        let row = s.row_from_value(&obj).expect("valid");
        assert_eq!(row.cell(&s, "replicas"), Some(&Cell::Text("3".into())));
        assert_eq!(
            row.cell(&s, "ready_replicas"),
            Some(&Cell::Text("2".into()))
        );
        assert_eq!(
            row.cell(&s, "available_replicas"),
            Some(&Cell::Text("2".into()))
        );
        // An absent count is NULL, never 0: "not reported" and "zero replicas"
        // are different facts.
        assert_eq!(row.cell(&s, "updated_replicas"), None);
    }

    #[test]
    fn scalar_projection_renders_only_json_scalars() {
        let r = Resource::new("apps", "v1", "Deployment", "deployments", true).expect("valid");
        let s = TableSchema::resolve(r, true, &[Some("name"), Some("replicas")]);
        for (value, want) in [
            (json!(0), Some(Cell::Text("0".into()))),
            (json!("3"), Some(Cell::Text("3".into()))),
            (json!(true), Some(Cell::Text("true".into()))),
            (json!(null), None),
            (json!({"a": 1}), None),
            (json!([1]), None),
        ] {
            let obj = json!({"metadata": {"name": "d"}, "spec": {"replicas": value}});
            let row = s.row_from_value(&obj).expect("valid");
            assert_eq!(row.cell(&s, "replicas"), want.as_ref(), "value {value}");
        }
    }

    #[test]
    fn decodes_a_crd_through_the_generic_projection() {
        let s = widgets();
        let obj = json!({
            "apiVersion": "example.com/v1",
            "kind": "Widget",
            "metadata": {"name": "w1", "namespace": "shop", "resourceVersion": "7",
                         "labels": {"tier": "web"}},
            "spec": {"size": 3},
            "status": {"ready": true}
        });
        let r = s.row_from_value(&obj).expect("valid");
        assert_eq!(r.name, "w1");
        assert_eq!(r.resource_version, "7");
        assert_eq!(r.cell(&s, "spec"), Some(&Cell::Json(json!({"size":3}))));
        assert_eq!(
            r.cell(&s, "status"),
            Some(&Cell::Json(json!({"ready":true})))
        );
        assert_eq!(
            r.cell(&s, "labels"),
            Some(&Cell::Json(json!({"tier":"web"})))
        );
    }

    #[test]
    fn a_column_the_kind_does_not_have_reads_null() {
        let r = Resource::new("example.com", "v1", "Widget", "widgets", true).expect("valid");
        let s = TableSchema::resolve(r, true, &[Some("name"), Some("nonesuch")]);
        let obj = json!({"metadata":{"name":"w1"},"spec":{}});
        let row = s.row_from_value(&obj).expect("valid");
        assert_eq!(row.cell(&s, "nonesuch"), None);
    }

    #[test]
    fn camel_case_top_level_fields_match_their_snake_case_column() {
        let r = Resource::new("", "v1", "Secret", "secrets", true).expect("valid");
        let s = TableSchema::resolve(r, true, &[Some("name"), Some("string_data")]);
        let obj = json!({"metadata":{"name":"s"},"stringData":{"k":"v"}});
        let row = s.row_from_value(&obj).expect("valid");
        assert_eq!(
            row.cell(&s, "string_data"),
            Some(&Cell::Json(json!({"k":"v"})))
        );
    }

    #[test]
    fn decode_rejects_bad_input_without_echoing_it() {
        let s = pods();
        assert_eq!(
            s.decode(POD.as_bytes(), 10),
            Err(DecodeError::TooLarge {
                bytes: POD.len(),
                max: 10
            })
        );
        assert!(matches!(
            s.decode(b"{not json", MAX_OBJECT_BYTES),
            Err(DecodeError::Json(_))
        ));
        assert_eq!(
            s.decode(b"[]", MAX_OBJECT_BYTES),
            Err(DecodeError::Shape("top level is not an object"))
        );
        assert_eq!(
            s.decode(b"{}", MAX_OBJECT_BYTES),
            Err(DecodeError::Shape("metadata is missing or not an object"))
        );
        assert_eq!(
            s.decode(br#"{"metadata":{"name":""}}"#, MAX_OBJECT_BYTES),
            Err(DecodeError::Shape("metadata.name is missing or empty"))
        );
        let msg = s
            .decode(b"{\"secret\":\"hunter2\"", MAX_OBJECT_BYTES)
            .expect_err("bad")
            .to_string();
        assert!(!msg.contains("hunter2"), "{msg}");
    }

    // --- insert -----------------------------------------------------------------

    #[test]
    fn insert_builds_full_object() {
        let s = configmaps();
        let new = new_row(
            &s,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", j(json!({"A":"1"}))),
            ],
        );
        let w = insert_body(&s, &new).expect("valid");
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
        let s = configmaps();
        let new = new_row(&s, &[("name", t("app")), ("namespace", t("shop"))]);
        assert_eq!(
            insert_body(&s, &new).expect("valid").body["data"],
            json!({})
        );
        // raw supplies identity and extra fields; typed data overlays it; rv is stripped.
        let raw = json!({"metadata":{"name":"app","namespace":"shop","labels":{"x":"y"},"resourceVersion":"5"},"data":{"OLD":"1"}});
        let new = new_row(&s, &[("raw", j(raw)), ("data", j(json!({"NEW":"2"})))]);
        let w = insert_body(&s, &new).expect("valid");
        assert_eq!(w.identity.name, "app");
        assert_eq!(w.body["metadata"]["labels"], json!({"x":"y"}));
        assert_eq!(w.body["data"], json!({"NEW":"2"}));
        assert!(w.body["metadata"].get("resourceVersion").is_none());
    }

    #[test]
    fn insert_builds_a_crd_object_from_top_level_columns() {
        let s = widgets();
        let new = new_row(
            &s,
            &[
                ("name", t("w1")),
                ("namespace", t("shop")),
                ("labels", j(json!({"tier":"web"}))),
                ("spec", j(json!({"size":3}))),
            ],
        );
        let w = insert_body(&s, &new).expect("valid");
        assert_eq!(
            w.body,
            json!({
                "apiVersion": "example.com/v1",
                "kind": "Widget",
                "metadata": {"name":"w1","namespace":"shop","labels":{"tier":"web"}},
                "spec": {"size":3}
            })
        );
    }

    #[test]
    fn insert_into_a_cluster_scoped_kind_needs_no_namespace() {
        let r = Resource::new(
            "example.com",
            "v1",
            "ClusterWidget",
            "clusterwidgets",
            false,
        )
        .expect("valid");
        let s = TableSchema::resolve(r, true, &[Some("name"), Some("spec"), Some("raw")]);
        let new = new_row(&s, &[("name", t("cw")), ("spec", j(json!({"a":1})))]);
        let w = insert_body(&s, &new).expect("valid");
        assert_eq!(w.identity.namespace, "");
        assert!(
            w.body["metadata"].get("namespace").is_none(),
            "a cluster-scoped object must not carry a namespace"
        );
    }

    #[test]
    fn a_cluster_scoped_raw_that_claims_a_namespace_loses_it() {
        let r = Resource::new(
            "example.com",
            "v1",
            "ClusterWidget",
            "clusterwidgets",
            false,
        )
        .expect("valid");
        let s = TableSchema::resolve(r, true, &[Some("name"), Some("spec"), Some("raw")]);
        let raw = json!({"metadata":{"name":"cw","namespace":"default"},"spec":{"a":1}});
        let w = insert_body(&s, &pg_row(&s, &[("raw", j(raw))])).expect("valid");
        assert!(
            w.body["metadata"].get("namespace").is_none(),
            "the API server refuses a cluster-scoped object that names a namespace"
        );
    }

    #[test]
    fn insert_validation_errors() {
        let s = configmaps();
        let e =
            |pairs: &[(&str, Option<Cell>)]| insert_body(&s, &new_row(&s, pairs)).expect_err("err");
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
                ("data", j(json!([1])))
            ]),
            WriteError::NotAnObject("data".into())
        );
        assert_eq!(
            e(&[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", j(json!({"k": 1})))
            ]),
            WriteError::DataValueNotString("k".into())
        );
        assert_eq!(
            e(&[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("raw", j(json!("str")))
            ]),
            WriteError::NotAnObject("raw".into())
        );
    }

    #[test]
    fn insert_into_a_read_only_table_is_refused() {
        let s = pods();
        let new = new_row(&s, &[("name", t("p")), ("namespace", t("n"))]);
        assert_eq!(
            insert_body(&s, &new),
            Err(WriteError::ReadOnly("pods".into()))
        );
    }

    #[test]
    fn insert_rejects_server_managed_columns() {
        let r = Resource::new("example.com", "v1", "Widget", "widgets", true).expect("valid");
        let s = TableSchema::resolve(
            r,
            true,
            &[
                Some("name"),
                Some("namespace"),
                Some("uid"),
                Some("resource_version"),
            ],
        );
        let new = new_row(
            &s,
            &[
                ("name", t("w1")),
                ("namespace", t("shop")),
                ("uid", t("forged-uid")),
            ],
        );
        assert_eq!(
            insert_body(&s, &new),
            Err(WriteError::NotWritable("uid".into())),
            "accepting uid would look like it worked while the API server ignored it"
        );
    }

    /// Builds a row the way Postgres hands one to an INSERT: every column of
    /// the table is present, and each one the statement did not mention is
    /// NULL. `new_row` leaves them undeclared instead, which is why its tests
    /// never saw the NULLs that wiped `raw` (#78).
    fn pg_row(schema: &TableSchema, pairs: &[(&str, Option<Cell>)]) -> NewRow {
        let mut row = NewRow::undeclared(schema);
        for (i, col) in schema.columns.iter().enumerate() {
            if col.is_some() {
                row.cells[i] = NewCell::Null;
            }
        }
        for (col, cell) in pairs {
            row.cells[schema.index_of(col).expect("column")] =
                cell.clone().map_or(NewCell::Null, NewCell::Value);
        }
        row
    }

    #[test]
    fn insert_of_raw_alone_keeps_everything_in_it() {
        // #78, as Postgres delivers it: `INSERT ... (namespace, name, raw)`
        // leaves data, labels and annotations NULL.
        let s = configmaps();
        let raw = json!({"apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"rawtest","namespace":"default",
                        "labels":{"from":"raw"},"annotations":{"note":"raw"}},
            "data":{"k":"v"}});
        let new = pg_row(
            &s,
            &[
                ("namespace", t("default")),
                ("name", t("rawtest")),
                ("raw", j(raw)),
            ],
        );
        let w = insert_body(&s, &new).expect("valid");
        assert_eq!(w.body["data"], json!({"k":"v"}), "data was discarded");
        assert_eq!(w.body["metadata"]["labels"], json!({"from":"raw"}));
        assert_eq!(w.body["metadata"]["annotations"], json!({"note":"raw"}));
    }

    #[test]
    fn insert_takes_identity_from_raw_when_the_columns_are_null() {
        let s = configmaps();
        let raw = json!({"metadata":{"name":"fromraw","namespace":"shop"},"data":{"k":"v"}});
        let w = insert_body(&s, &pg_row(&s, &[("raw", j(raw))])).expect("valid");
        assert_eq!(w.identity.name, "fromraw");
        assert_eq!(w.identity.namespace, "shop");
        assert_eq!(w.body["data"], json!({"k":"v"}));
    }

    #[test]
    fn a_typed_column_overrides_the_same_field_in_raw() {
        // raw as a template: the documented precedence, as on UPDATE.
        let s = configmaps();
        let template = json!({"metadata":{"name":"template","namespace":"shop",
                                          "labels":{"tier":"web"}},"data":{"OLD":"1"}});
        let new = pg_row(
            &s,
            &[
                ("name", t("copy")),
                ("raw", j(template)),
                ("data", j(json!({"NEW":"2"}))),
            ],
        );
        let w = insert_body(&s, &new).expect("valid");
        assert_eq!(w.identity.name, "copy", "the name column wins over raw");
        assert_eq!(
            w.body["data"],
            json!({"NEW":"2"}),
            "the data column wins over raw"
        );
        assert_eq!(
            w.body["metadata"]["labels"],
            json!({"tier":"web"}),
            "a field no column set comes from raw"
        );
    }

    #[test]
    fn raw_read_from_another_object_works_as_a_template() {
        // What `SELECT raw FROM ...` returns: server-assigned metadata that a
        // create cannot choose. It is dropped; what the user owns is kept.
        let s = configmaps();
        let read = json!({"apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"orig","namespace":"shop","uid":"u1","resourceVersion":"9",
                        "creationTimestamp":"2026-01-01T00:00:00Z","generation":3,
                        "managedFields":[{"manager":"kubectl"}],"labels":{"tier":"web"}},
            "data":{"k":"v"}});
        let new = pg_row(&s, &[("name", t("copy")), ("raw", j(read))]);
        let w = insert_body(&s, &new).expect("valid");
        let meta = w.body["metadata"].as_object().expect("metadata");
        for key in SERVER_ASSIGNED_METADATA {
            assert!(!meta.contains_key(*key), "{key} was sent on a create");
        }
        assert_eq!(meta["labels"], json!({"tier":"web"}));
        assert_eq!(meta["name"], "copy");
        assert_eq!(w.body["data"], json!({"k":"v"}));
    }

    #[test]
    fn raw_of_another_kind_is_refused_not_relabelled() {
        let s = configmaps();
        for raw in [
            json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"d","namespace":"shop"}}),
            json!({"kind":"Secret","metadata":{"name":"d","namespace":"shop"}}),
            json!({"apiVersion":"v2","metadata":{"name":"d","namespace":"shop"}}),
        ] {
            let got = insert_body(&s, &pg_row(&s, &[("raw", j(raw.clone()))]));
            assert!(
                matches!(got, Err(WriteError::WrongKind(..))),
                "{raw}: got {got:?}"
            );
        }
        // Omitting apiVersion and kind is fine: the table supplies them.
        let raw = json!({"metadata":{"name":"d","namespace":"shop"}});
        let w = insert_body(&s, &pg_row(&s, &[("raw", j(raw))])).expect("valid");
        assert_eq!(w.body["kind"], "ConfigMap");
        assert_eq!(
            WriteError::WrongKind("apps/v1 Deployment".into(), "v1 ConfigMap".into()).to_string(),
            "raw describes apps/v1 Deployment, but this table holds v1 ConfigMap"
        );
    }

    #[test]
    fn configmap_data_from_raw_is_checked_like_the_column() {
        let s = configmaps();
        let raw = json!({"metadata":{"name":"d","namespace":"shop"},"data":{"n":1}});
        assert!(matches!(
            insert_body(&s, &pg_row(&s, &[("raw", j(raw))])),
            Err(WriteError::DataValueNotString(_))
        ));
    }

    #[test]
    fn update_refuses_raw_replaced_with_another_kind() {
        let s = configmaps();
        let mut other = cm_raw();
        other["kind"] = json!("Secret");
        let new = new_row(
            &s,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("raw", j(other)),
            ],
        );
        assert!(matches!(
            update_body(&s, &cm_raw(), &new),
            Err(WriteError::WrongKind(..))
        ));
    }

    #[test]
    fn insert_null_columns() {
        let s = configmaps();
        // NULL data → empty map; NULL raw → as if omitted.
        let new = new_row(
            &s,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", None),
                ("raw", None),
            ],
        );
        let w = insert_body(&s, &new).expect("valid");
        assert_eq!(w.body["data"], json!({}));
        // NULL name is still a missing name.
        let new = new_row(&s, &[("name", None), ("namespace", t("shop"))]);
        assert_eq!(insert_body(&s, &new), Err(WriteError::MissingName));
    }

    // --- update -----------------------------------------------------------------

    #[test]
    fn update_applies_data_and_keeps_identity_and_rv() {
        let s = configmaps();
        // Postgres hands the full new tuple: unchanged columns carry old values.
        let new = new_row(
            &s,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", j(json!({"LOG_LEVEL":"debug"}))),
                ("raw", j(cm_raw())),
            ],
        );
        let w = update_body(&s, &cm_raw(), &new).expect("valid");
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
        let s = configmaps();
        let mut raw2 = cm_raw();
        raw2["metadata"]["labels"] = json!({"tier":"web"});
        raw2["metadata"]["name"] = json!("evil"); // must be overridden
        raw2["metadata"]["resourceVersion"] = json!("999"); // must be overridden by the read rv
        let new = new_row(
            &s,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", j(json!({"LOG_LEVEL":"info"}))),
                ("raw", j(raw2)),
            ],
        );
        let w = update_body(&s, &cm_raw(), &new).expect("valid");
        assert_eq!(w.body["metadata"]["labels"], json!({"tier":"web"}));
        assert_eq!(w.body["metadata"]["name"], "app");
        assert_eq!(w.body["metadata"]["resourceVersion"], "9");
        assert_eq!(w.body["kind"], "ConfigMap");
    }

    #[test]
    fn update_rejects_identity_change_and_bad_old_raw() {
        let s = configmaps();
        let new = new_row(&s, &[("name", t("renamed")), ("namespace", t("shop"))]);
        assert_eq!(
            update_body(&s, &cm_raw(), &new),
            Err(WriteError::IdentityChange("name"))
        );
        let new = new_row(&s, &[("name", t("app")), ("namespace", t("other"))]);
        assert_eq!(
            update_body(&s, &cm_raw(), &new),
            Err(WriteError::IdentityChange("namespace"))
        );
        let new = new_row(&s, &[]);
        assert_eq!(
            update_body(&s, &json!({"metadata":{"name":"app"}}), &new),
            Err(WriteError::BadOldRaw("metadata.resourceVersion missing"))
        );
        assert_eq!(
            update_body(&s, &json!({}), &new),
            Err(WriteError::BadOldRaw("metadata.name missing"))
        );
    }

    #[test]
    fn update_of_a_read_only_table_is_refused() {
        let s = pods();
        let new = new_row(&s, &[]);
        assert_eq!(
            update_body(&s, &cm_raw(), &new),
            Err(WriteError::ReadOnly("pods".into()))
        );
    }

    #[test]
    fn update_via_raw_jsonb_set_changes_data_when_typed_data_untouched() {
        // SET raw = jsonb_set(raw, '{data,VIA_RAW}', '"1"'): the new tuple's `data`
        // still holds the OLD value; it must not overwrite the raw change.
        let s = configmaps();
        let mut raw2 = cm_raw();
        raw2["data"]["VIA_RAW"] = json!("1");
        let new = new_row(
            &s,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", j(json!({"LOG_LEVEL":"info"}))),
                ("raw", j(raw2)),
            ],
        );
        let w = update_body(&s, &cm_raw(), &new).expect("valid");
        assert_eq!(w.body["data"], json!({"LOG_LEVEL":"info","VIA_RAW":"1"}));
    }

    #[test]
    fn update_with_both_raw_and_data_changed_applies_data_last() {
        let s = configmaps();
        let mut raw2 = cm_raw();
        raw2["metadata"]["labels"] = json!({"a":"b"});
        raw2["data"] = json!({"FROM_RAW":"x"});
        let new = new_row(
            &s,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", j(json!({"FROM_DATA":"y"}))),
                ("raw", j(raw2)),
            ],
        );
        let w = update_body(&s, &cm_raw(), &new).expect("valid");
        assert_eq!(w.body["metadata"]["labels"], json!({"a":"b"}));
        assert_eq!(w.body["data"], json!({"FROM_DATA":"y"}));
    }

    #[test]
    fn update_set_data_null_clears_and_identity_null_is_an_error() {
        let s = configmaps();
        let new = new_row(
            &s,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", None),
                ("raw", j(cm_raw())),
            ],
        );
        let w = update_body(&s, &cm_raw(), &new).expect("valid");
        assert_eq!(w.body["data"], json!({}));
        for (col, pairs) in [
            ("name", vec![("name", None), ("namespace", t("shop"))]),
            ("namespace", vec![("name", t("app")), ("namespace", None)]),
            (
                "raw",
                vec![("name", t("app")), ("namespace", t("shop")), ("raw", None)],
            ),
        ] {
            assert_eq!(
                update_body(&s, &cm_raw(), &new_row(&s, &pairs)),
                Err(WriteError::NullNotAllowed(col.to_owned()))
            );
        }
    }

    #[test]
    fn update_with_nothing_changed_is_a_faithful_put_of_the_old_object() {
        let s = configmaps();
        let new = new_row(
            &s,
            &[
                ("name", t("app")),
                ("namespace", t("shop")),
                ("data", j(json!({"LOG_LEVEL":"info"}))),
                ("raw", j(cm_raw())),
            ],
        );
        let w = update_body(&s, &cm_raw(), &new).expect("valid");
        assert_eq!(w.body, cm_raw());
    }

    #[test]
    fn update_of_a_crd_changes_only_the_touched_top_level_field() {
        let s = widgets();
        let old = json!({
            "apiVersion": "example.com/v1", "kind": "Widget",
            "metadata": {"name":"w1","namespace":"shop","resourceVersion":"7","labels":{"tier":"web"}},
            "spec": {"size":3}, "status": {"ready":true}
        });
        let new = new_row(
            &s,
            &[
                ("name", t("w1")),
                ("namespace", t("shop")),
                ("labels", j(json!({"tier":"web"}))),
                ("spec", j(json!({"size":5}))),
                ("status", j(json!({"ready":true}))),
                ("raw", j(old.clone())),
            ],
        );
        let w = update_body(&s, &old, &new).expect("valid");
        assert_eq!(w.body["spec"], json!({"size":5}));
        assert_eq!(
            w.body["status"],
            json!({"ready":true}),
            "untouched status preserved"
        );
        assert_eq!(w.body["metadata"]["resourceVersion"], "7");
    }

    #[test]
    fn update_rejects_a_forged_server_managed_column() {
        let r = Resource::new("example.com", "v1", "Widget", "widgets", true).expect("valid");
        let s = TableSchema::resolve(
            r,
            true,
            &[Some("name"), Some("namespace"), Some("uid"), Some("raw")],
        );
        let old =
            json!({"metadata":{"name":"w1","namespace":"shop","resourceVersion":"7","uid":"real"}});
        // Carrying the value the scan read is just a complete tuple, not a write.
        let unchanged = new_row(
            &s,
            &[
                ("name", t("w1")),
                ("namespace", t("shop")),
                ("uid", t("real")),
                ("raw", j(old.clone())),
            ],
        );
        assert!(update_body(&s, &old, &unchanged).is_ok());
        // Changing it is.
        let forged = new_row(
            &s,
            &[
                ("name", t("w1")),
                ("namespace", t("shop")),
                ("uid", t("forged")),
                ("raw", j(old.clone())),
            ],
        );
        assert_eq!(
            update_body(&s, &old, &forged),
            Err(WriteError::NotWritable("uid".into()))
        );
    }

    #[test]
    fn update_writes_back_to_the_camel_case_field_it_read() {
        let r = Resource::new("", "v1", "Secret", "secrets", true).expect("valid");
        let s = TableSchema::resolve(
            r,
            true,
            &[
                Some("name"),
                Some("namespace"),
                Some("string_data"),
                Some("raw"),
            ],
        );
        let old = json!({"metadata":{"name":"s","namespace":"n","resourceVersion":"3"},
                         "stringData":{"k":"old"}});
        let new = new_row(
            &s,
            &[
                ("name", t("s")),
                ("namespace", t("n")),
                ("string_data", j(json!({"k":"new"}))),
                ("raw", j(old.clone())),
            ],
        );
        let w = update_body(&s, &old, &new).expect("valid");
        assert_eq!(w.body["stringData"], json!({"k":"new"}));
        assert!(
            w.body.get("string_data").is_none(),
            "must not create a snake_case twin of the field it read"
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

    #[test]
    fn undeclared_columns_are_distinct_from_null() {
        let s = configmaps();
        let row = NewRow::undeclared(&s);
        assert!(row.cells.iter().all(|c| *c == NewCell::Undeclared));
        assert!(!is_null(&s, &row, "data"));
        let row = new_row(&s, &[("data", None)]);
        assert!(is_null(&s, &row, "data"));
        assert_eq!(*cell(&s, &row, "nonexistent"), NewCell::Undeclared);
    }
}
