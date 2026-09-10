//! Pure mapping from a Pod's JSON (as returned by the gateway) to the typed
//! columns of a `k8s_pods` foreign table. Treats the JSON as untrusted input:
//! size-bounded and shape-checked before anything is trusted.

use std::fmt;

/// Columns a Pods foreign table may declare. Column matching is by name; the
/// glue also checks the declared SQL type against [`PodColumn::sql_type`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodColumn {
    /// `metadata.name`, text.
    Name,
    /// `metadata.namespace`, text.
    Namespace,
    /// `status.phase`, text, NULL if absent.
    Phase,
    /// `spec.nodeName`, text, NULL if unscheduled.
    Node,
    /// Whole object, jsonb.
    Raw,
}

/// SQL type a column must be declared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    /// `text`
    Text,
    /// `jsonb`
    Jsonb,
}

impl PodColumn {
    /// Maps a foreign-table column name to a Pod column.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "name" => Some(Self::Name),
            "namespace" => Some(Self::Namespace),
            "phase" => Some(Self::Phase),
            "node" => Some(Self::Node),
            "raw" => Some(Self::Raw),
            _ => None,
        }
    }

    /// Required SQL type for the column.
    pub fn sql_type(self) -> SqlType {
        match self {
            Self::Raw => SqlType::Jsonb,
            Self::Name | Self::Namespace | Self::Phase | Self::Node => SqlType::Text,
        }
    }

    /// All supported column names, for error messages.
    pub const NAMES: &'static [&'static str] = &["name", "namespace", "phase", "node", "raw"];
}

/// One decoded Pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodRow {
    /// `metadata.name`.
    pub name: String,
    /// `metadata.namespace` (empty string if somehow absent; Pods are namespaced).
    pub namespace: String,
    /// `status.phase`.
    pub phase: Option<String>,
    /// `spec.nodeName`.
    pub node: Option<String>,
    /// Full object.
    pub raw: serde_json::Value,
}

/// Why an object could not be decoded. Never echoes the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodError {
    /// Payload exceeds the configured bound.
    TooLarge { bytes: usize, max: usize },
    /// Not valid JSON.
    Json(String),
    /// JSON is not an object, or `metadata.name` is not a non-empty string.
    Shape(&'static str),
}

impl fmt::Display for PodError {
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

impl std::error::Error for PodError {}

/// Upper bound on one object's JSON. Matches the gRPC default max message size
/// so nothing larger can arrive anyway; enforced here so the FDW never trusts
/// the transport for this. TODO(phase3): tie to the shared-memory value bound.
pub const MAX_OBJECT_BYTES: usize = 4 * 1024 * 1024;

impl PodRow {
    /// Decodes one Pod from raw JSON bytes, enforcing `max_bytes`.
    pub fn from_json(bytes: &[u8], max_bytes: usize) -> Result<Self, PodError> {
        if bytes.len() > max_bytes {
            return Err(PodError::TooLarge {
                bytes: bytes.len(),
                max: max_bytes,
            });
        }
        let raw: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| PodError::Json(e.to_string()))?;
        let obj = raw
            .as_object()
            .ok_or(PodError::Shape("top level is not an object"))?;
        let metadata = obj
            .get("metadata")
            .and_then(serde_json::Value::as_object)
            .ok_or(PodError::Shape("metadata is missing or not an object"))?;
        let name = metadata
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(PodError::Shape("metadata.name is missing or empty"))?
            .to_owned();
        let namespace = metadata
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let phase = raw
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let node = raw
            .pointer("/spec/nodeName")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok(Self {
            name,
            namespace,
            phase,
            node,
            raw,
        })
    }

    /// The text value of a text column (`None` = SQL NULL). `Raw` is not a
    /// text column; callers use [`PodRow::raw`] for it.
    pub fn text(&self, col: PodColumn) -> Option<&str> {
        match col {
            PodColumn::Name => Some(&self.name),
            PodColumn::Namespace => Some(&self.namespace),
            PodColumn::Phase => self.phase.as_deref(),
            PodColumn::Node => self.node.as_deref(),
            PodColumn::Raw => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POD: &str = r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"web-0","namespace":"shop","resourceVersion":"42"},
        "spec":{"nodeName":"node-a","containers":[]},"status":{"phase":"Running"}}"#;

    #[test]
    fn decodes_all_columns() {
        let r = PodRow::from_json(POD.as_bytes(), MAX_OBJECT_BYTES).expect("valid");
        assert_eq!(r.name, "web-0");
        assert_eq!(r.namespace, "shop");
        assert_eq!(r.text(PodColumn::Phase), Some("Running"));
        assert_eq!(r.text(PodColumn::Node), Some("node-a"));
        assert_eq!(r.text(PodColumn::Name), Some("web-0"));
        assert_eq!(r.text(PodColumn::Raw), None);
        assert_eq!(r.raw["kind"], "Pod");
    }

    #[test]
    fn optional_fields_become_null() {
        let r = PodRow::from_json(
            br#"{"metadata":{"name":"p","namespace":"n"}}"#,
            MAX_OBJECT_BYTES,
        )
        .expect("valid");
        assert_eq!(r.phase, None);
        assert_eq!(r.node, None);
        // Wrong types for optional fields are treated as absent, not errors.
        let r = PodRow::from_json(
            br#"{"metadata":{"name":"p"},"status":{"phase":7},"spec":{"nodeName":null}}"#,
            MAX_OBJECT_BYTES,
        )
        .expect("valid");
        assert_eq!(r.phase, None);
        assert_eq!(r.node, None);
        assert_eq!(r.namespace, "");
    }

    #[test]
    fn rejects_oversized_before_parsing() {
        let err = PodRow::from_json(POD.as_bytes(), 10).expect_err("too large");
        assert_eq!(
            err,
            PodError::TooLarge {
                bytes: POD.len(),
                max: 10
            }
        );
    }

    #[test]
    fn rejects_bad_json_and_shapes() {
        assert!(matches!(
            PodRow::from_json(b"{not json", MAX_OBJECT_BYTES),
            Err(PodError::Json(_))
        ));
        assert_eq!(
            PodRow::from_json(b"[]", MAX_OBJECT_BYTES),
            Err(PodError::Shape("top level is not an object"))
        );
        assert_eq!(
            PodRow::from_json(b"{}", MAX_OBJECT_BYTES),
            Err(PodError::Shape("metadata is missing or not an object"))
        );
        assert_eq!(
            PodRow::from_json(br#"{"metadata":{"name":""}}"#, MAX_OBJECT_BYTES),
            Err(PodError::Shape("metadata.name is missing or empty"))
        );
        assert_eq!(
            PodRow::from_json(br#"{"metadata":{"name":5}}"#, MAX_OBJECT_BYTES),
            Err(PodError::Shape("metadata.name is missing or empty"))
        );
        // Error text must not echo the payload.
        let msg = PodRow::from_json(b"{\"secret\":\"hunter2\"", MAX_OBJECT_BYTES)
            .expect_err("bad")
            .to_string();
        assert!(!msg.contains("hunter2"), "{msg}");
    }

    #[test]
    fn column_names_and_types() {
        for n in PodColumn::NAMES {
            assert!(PodColumn::from_name(n).is_some(), "{n}");
        }
        assert_eq!(PodColumn::from_name("labels"), None);
        assert_eq!(PodColumn::Raw.sql_type(), SqlType::Jsonb);
        assert_eq!(PodColumn::Phase.sql_type(), SqlType::Text);
    }
}
