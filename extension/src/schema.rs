//! The projection rule: how a declared SQL column is read out of a Kubernetes
//! object, and written back into one.
//!
//! This is the extension's half of a two-language contract. The gateway decides
//! *which* columns a kind's foreign table should have (see `Columns` in
//! `gateway/internal/k8s/schema.go`), and this module decides what each of
//! those columns *means*. Keeping the meaning here, keyed on the column name
//! alone, is what lets `IMPORT FOREIGN SCHEMA` generate DDL for an arbitrary
//! CRD without shipping a per-kind mapping table into shared memory, and what
//! makes it impossible for the two sides to disagree about a column's value:
//! the gateway never gets a say in it.
//!
//! Two functions must stay in step with their Go counterparts:
//! [`normalize_field_name`] mirrors `NormalizeFieldName`, and
//! [`promoted_columns`] mirrors the `promoted` table. Both have unit tests on
//! each side asserting the same cases.

use crate::resource::Resource;

/// SQL type a column must be declared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    /// `text`
    Text,
    /// `jsonb`
    Jsonb,
}

impl SqlType {
    /// The SQL type name, for generated DDL and error messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Jsonb => "jsonb",
        }
    }
}

/// Where a column's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    /// A JSON pointer into the object, read as a string. Strict: a value that
    /// is not a JSON string reads as SQL NULL rather than being coerced, so a
    /// field whose type changed upstream is visible as missing instead of
    /// silently reinterpreted.
    Text(&'static str),
    /// A JSON pointer into the object, read as text from any JSON scalar:
    /// string, number or boolean. For fields Kubernetes models as numbers,
    /// such as a Deployment's replica counts, which [`Projection::Text`]
    /// would read as NULL. Objects, arrays and null read as SQL NULL.
    Scalar(&'static str),
    /// A JSON pointer into the object, read as JSON. Absent means SQL NULL
    /// unless `empty_object` is set, in which case an absent value reads as
    /// `{}` so containment and key tests do not NULL-propagate.
    Json {
        /// Pointer to the value.
        pointer: &'static str,
        /// Whether absence reads as `{}` rather than NULL.
        empty_object: bool,
    },
    /// The whole object.
    Raw,
    /// A top-level field of the object, found by matching the column name
    /// against [`normalize_field_name`] of each key. Always JSON.
    TopLevel,
}

/// One column of a foreign table, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    /// SQL column name as declared.
    pub name: &'static str,
    /// Required SQL type.
    pub sql_type: SqlType,
    /// How to read it.
    pub projection: Projection,
    /// Whether SQL may write it. Server-managed metadata is readable but not
    /// settable: accepting `uid` or `resource_version` in an INSERT would look
    /// like it worked while the API server ignored or rejected it.
    pub writable: bool,
}

const fn text(name: &'static str, pointer: &'static str, writable: bool) -> Column {
    Column {
        name,
        sql_type: SqlType::Text,
        projection: Projection::Text(pointer),
        writable,
    }
}

const fn scalar(name: &'static str, pointer: &'static str) -> Column {
    Column {
        name,
        sql_type: SqlType::Text,
        projection: Projection::Scalar(pointer),
        // Server-reported counts. spec.replicas is genuinely settable through
        // Kubernetes, but writing it here would mean reaching into a nested
        // path; scale by assigning the `spec` column instead.
        writable: false,
    }
}

const fn json(name: &'static str, pointer: &'static str, empty_object: bool) -> Column {
    Column {
        name,
        sql_type: SqlType::Jsonb,
        projection: Projection::Json {
            pointer,
            empty_object,
        },
        writable: true,
    }
}

/// Columns every kind has, whatever its schema. Mirrors `metadataColumns` in
/// the gateway, plus `raw`.
const META_COLUMNS: &[Column] = &[
    text("name", "/metadata/name", true),
    text("namespace", "/metadata/namespace", true),
    text("uid", "/metadata/uid", false),
    text("resource_version", "/metadata/resourceVersion", false),
    text("creation_timestamp", "/metadata/creationTimestamp", false),
    json("labels", "/metadata/labels", false),
    json("annotations", "/metadata/annotations", false),
];

const POD_PROMOTED: &[Column] = &[
    text("phase", "/status/phase", false),
    text("node", "/spec/nodeName", false),
];

/// Deployments carry the numbers people actually filter on inside `spec` and
/// `status`, deep enough that the generic top-level rule cannot reach them.
/// They are text columns holding a rendered number, so `replicas::int` works
/// and an absent field is NULL rather than zero.
const DEPLOYMENT_PROMOTED: &[Column] = &[
    scalar("replicas", "/spec/replicas"),
    scalar("ready_replicas", "/status/readyReplicas"),
    scalar("available_replicas", "/status/availableReplicas"),
    scalar("updated_replicas", "/status/updatedReplicas"),
];

const CONFIGMAP_PROMOTED: &[Column] = &[
    // Absent data reads as `{}` so `data ? 'key'` is false rather than NULL.
    json("data", "/data", true),
];

/// Hand-mapped columns for built-in kinds whose useful fields sit deeper than
/// the generic top-level rule reaches (docs/DESIGN.md §5.4).
///
/// Mirrors the gateway's `promoted` table. A column the gateway emits but this
/// function does not know reads as NULL rather than as a wrong value, which is
/// why the two lists are asserted equal by tests on both sides rather than by a
/// shared artifact.
pub fn promoted_columns(r: &Resource) -> &'static [Column] {
    match (r.group.as_str(), r.kind.as_str()) {
        ("", "Pod") => POD_PROMOTED,
        ("", "ConfigMap") => CONFIGMAP_PROMOTED,
        ("apps", "Deployment") => DEPLOYMENT_PROMOTED,
        _ => &[],
    }
}

/// Maps a Kubernetes field name to the SQL column name representing it.
///
/// Mirrors `NormalizeFieldName` in `gateway/internal/k8s/schema.go`; see that
/// function's comment for the contract. `camelCase` becomes `snake_case`, anything
/// outside `[a-z0-9_]` becomes an underscore, runs collapse, and a leading
/// digit is prefixed so the result always starts a valid identifier.
pub fn normalize_field_name(field: &str) -> String {
    let chars: Vec<char> = field.chars().collect();
    let mut out = String::with_capacity(field.len() + 4);
    let push_sep = |out: &mut String| {
        if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    };
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                let prev = chars[i - 1];
                let end_of_acronym = prev.is_ascii_uppercase()
                    && chars.get(i + 1).is_some_and(char::is_ascii_lowercase);
                if prev.is_ascii_lowercase() || prev.is_ascii_digit() || end_of_acronym {
                    push_sep(&mut out);
                }
            }
            out.push(c.to_ascii_lowercase());
        } else if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else {
            push_sep(&mut out);
        }
    }
    let trimmed = out.trim_end_matches('_');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        return format!("_{trimmed}");
    }
    trimmed.to_owned()
}

/// Resolves a declared column name against a kind.
///
/// Returns the column's required SQL type and how to read it. Every name
/// resolves: one that matches no promoted column falls through to
/// [`Projection::TopLevel`], which reads the object's top-level field of that
/// name and is SQL NULL when the kind has no such field. That fallthrough is
/// what lets a hand-written `CREATE FOREIGN TABLE` name any CRD field without
/// the extension having discovered the kind first.
pub fn column(r: &Resource, name: &str) -> Column {
    if name == "raw" {
        return Column {
            name: "raw",
            sql_type: SqlType::Jsonb,
            projection: Projection::Raw,
            // `raw` is the UPDATE/DELETE identity carrier, not a settable
            // column: writes derive the body from it rather than storing it.
            writable: false,
        };
    }
    // A promoted column wins over the metadata default only where the two do
    // not collide; the gateway drops colliding top-level fields for the same
    // reason, so the two sides agree on which name refers to what.
    if let Some(c) = promoted_columns(r).iter().find(|c| c.name == name) {
        return *c;
    }
    if let Some(c) = META_COLUMNS.iter().find(|c| c.name == name) {
        // A cluster-scoped kind has no namespace; the column reads NULL rather
        // than being rejected, so one piece of DDL can target both scopes.
        return *c;
    }
    Column {
        // The name is borrowed from the caller's tuple descriptor, which
        // outlives the scan; callers that need an owned name keep their own.
        name: "",
        sql_type: SqlType::Jsonb,
        projection: Projection::TopLevel,
        writable: true,
    }
}

/// Finds the top-level field of `object` whose normalized name is `column`.
///
/// Returns `None` when no field matches, and also when two do: an ambiguous
/// match is NULL rather than an arbitrary pick. The gateway declines to emit a
/// column for an ambiguous pair for the same reason, so this only arises for
/// hand-written DDL.
pub fn top_level_field<'a>(
    object: &'a serde_json::Value,
    column: &str,
) -> Option<&'a serde_json::Value> {
    let map = object.as_object()?;
    let mut found = None;
    for (k, v) in map {
        if normalize_field_name(k) != column {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(v);
    }
    found
}

/// The top-level field name a writable [`Projection::TopLevel`] column writes
/// to, given the base object it is being applied to.
///
/// Prefers an existing field that normalizes to the column name, so an UPDATE
/// of a camelCase field writes back to that exact key instead of creating a
/// second `snake_case` one. Falls back to the column name itself when the base
/// object has no such field, which is the INSERT case.
pub fn top_level_key(base: Option<&serde_json::Value>, column: &str) -> String {
    if let Some(obj) = base.and_then(serde_json::Value::as_object) {
        let mut matches = obj.keys().filter(|k| normalize_field_name(k) == column);
        if let Some(k) = matches.next() {
            if matches.next().is_none() {
                return k.clone();
            }
        }
    }
    column.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pods() -> Resource {
        Resource::new("", "v1", "Pod", "pods", true).expect("valid")
    }
    fn widgets() -> Resource {
        Resource::new("example.com", "v1", "Widget", "widgets", true).expect("valid")
    }
    fn deployments() -> Resource {
        Resource::new("apps", "v1", "Deployment", "deployments", true).expect("valid")
    }

    #[test]
    fn normalize_matches_the_gateway_cases() {
        // These are the same cases as TestNormalizeFieldName in
        // gateway/internal/k8s/schema_test.go. If one side changes, both
        // test suites must change together or a column reads silently NULL.
        for (input, want) in [
            ("spec", "spec"),
            ("status", "status"),
            ("data", "data"),
            ("stringData", "string_data"),
            ("roleRef", "role_ref"),
            ("imagePullSecrets", "image_pull_secrets"),
            ("AllCaps", "all_caps"),
            ("with-dash", "with_dash"),
            ("with.dot", "with_dot"),
            ("with space", "with_space"),
            ("already_snake", "already_snake"),
            ("x509", "x509"),
            ("3rdParty", "_3rd_party"),
            ("APIVersion", "api_version"),
            ("", ""),
        ] {
            assert_eq!(normalize_field_name(input), want, "input {input:?}");
        }
    }

    #[test]
    fn normalize_never_starts_with_a_digit() {
        for input in ["9lives", "0", "-x", ".y", " z"] {
            let got = normalize_field_name(input);
            assert!(!got.is_empty(), "{input:?} normalized to empty");
            assert!(
                !got.starts_with(|c: char| c.is_ascii_digit()),
                "{input:?} -> {got:?} starts with a digit"
            );
        }
    }

    #[test]
    fn promoted_columns_match_the_gateway_table() {
        // The gateway's `promoted` map lists exactly phase and node for Pods.
        let names: Vec<&str> = promoted_columns(&pods()).iter().map(|c| c.name).collect();
        assert_eq!(names, ["phase", "node"]);
        // ConfigMap `data` is promoted here but derived from the top-level rule
        // on the gateway side; both produce a jsonb column called `data`.
        let names: Vec<&str> =
            promoted_columns(&Resource::new("", "v1", "ConfigMap", "configmaps", true).unwrap())
                .iter()
                .map(|c| c.name)
                .collect();
        assert_eq!(names, ["data"]);
        assert!(promoted_columns(&widgets()).is_empty());
        // Mirrors the apps/v1 Deployment entry in the gateway's `promoted` map.
        let names: Vec<&str> = promoted_columns(&deployments())
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(
            names,
            [
                "replicas",
                "ready_replicas",
                "available_replicas",
                "updated_replicas"
            ]
        );
    }

    #[test]
    fn deployment_replica_columns_are_scalars_not_strict_text() {
        // Kubernetes models replica counts as JSON numbers, which
        // Projection::Text reads as NULL. Getting this wrong gives a column
        // that is silently always empty.
        for c in promoted_columns(&deployments()) {
            assert_eq!(c.sql_type, SqlType::Text, "{} should be text", c.name);
            assert!(
                matches!(c.projection, Projection::Scalar(_)),
                "{} must use a Scalar projection to survive a numeric value",
                c.name
            );
            assert!(!c.writable, "{} is server-reported", c.name);
        }
    }

    #[test]
    fn promoted_columns_are_keyed_on_group_as_well_as_kind() {
        // A Deployment in some other group is not the apps/v1 Deployment.
        let impostor =
            Resource::new("example.com", "v1", "Deployment", "deployments", true).expect("valid");
        assert!(promoted_columns(&impostor).is_empty());
    }

    #[test]
    fn promoted_columns_are_keyed_on_identity_not_kind_name() {
        let impostor = Resource::new("example.com", "v1", "Pod", "pods", true).expect("valid");
        assert!(
            promoted_columns(&impostor).is_empty(),
            "a CRD that calls itself Pod must not inherit the core Pod mapping"
        );
    }

    #[test]
    fn metadata_columns_resolve_for_every_kind() {
        for r in [pods(), widgets()] {
            assert_eq!(
                column(&r, "name").projection,
                Projection::Text("/metadata/name")
            );
            assert_eq!(column(&r, "namespace").sql_type, SqlType::Text);
            assert_eq!(column(&r, "labels").sql_type, SqlType::Jsonb);
            assert_eq!(column(&r, "raw").projection, Projection::Raw);
        }
    }

    #[test]
    fn server_managed_columns_are_read_only() {
        let r = widgets();
        for name in ["uid", "resource_version", "creation_timestamp", "raw"] {
            assert!(
                !column(&r, name).writable,
                "{name} is server-managed and must not be settable from SQL"
            );
        }
        for name in ["name", "namespace", "labels", "spec"] {
            assert!(column(&r, name).writable, "{name} should be settable");
        }
    }

    #[test]
    fn pod_promoted_columns_win_over_the_top_level_fallthrough() {
        let c = column(&pods(), "phase");
        assert_eq!(c.sql_type, SqlType::Text);
        assert_eq!(c.projection, Projection::Text("/status/phase"));
        assert_eq!(
            column(&pods(), "node").projection,
            Projection::Text("/spec/nodeName")
        );
        // The same names on a CRD are ordinary top-level lookups.
        assert_eq!(column(&widgets(), "phase").projection, Projection::TopLevel);
    }

    #[test]
    fn unknown_columns_fall_through_to_a_top_level_lookup() {
        let c = column(&widgets(), "spec");
        assert_eq!(c.projection, Projection::TopLevel);
        assert_eq!(c.sql_type, SqlType::Jsonb);
        assert_eq!(
            column(&widgets(), "anything_at_all").projection,
            Projection::TopLevel
        );
    }

    #[test]
    fn top_level_field_matches_through_normalization() {
        let obj = serde_json::json!({
            "apiVersion": "v1",
            "spec": {"replicas": 3},
            "stringData": {"k": "v"},
        });
        assert_eq!(
            top_level_field(&obj, "spec"),
            Some(&serde_json::json!({"replicas": 3}))
        );
        assert_eq!(
            top_level_field(&obj, "string_data"),
            Some(&serde_json::json!({"k": "v"}))
        );
        assert_eq!(top_level_field(&obj, "missing"), None);
    }

    #[test]
    fn an_ambiguous_top_level_match_is_null_not_a_guess() {
        let obj = serde_json::json!({"myField": 1, "my_field": 2});
        assert_eq!(
            top_level_field(&obj, "my_field"),
            None,
            "two fields normalizing to one column must read NULL, not an arbitrary one"
        );
    }

    #[test]
    fn top_level_key_writes_back_to_the_existing_spelling() {
        let base = serde_json::json!({"stringData": {"k": "v"}});
        assert_eq!(
            top_level_key(Some(&base), "string_data"),
            "stringData",
            "an UPDATE must write back to the field it read, not create a snake_case twin"
        );
        // No base object (INSERT), or no such field: use the column name.
        assert_eq!(top_level_key(None, "spec"), "spec");
        assert_eq!(top_level_key(Some(&base), "spec"), "spec");
        // Ambiguous: fall back rather than pick.
        let ambiguous = serde_json::json!({"myField": 1, "my_field": 2});
        assert_eq!(top_level_key(Some(&ambiguous), "my_field"), "my_field");
    }

    #[test]
    fn sql_type_names_are_the_ddl_spellings() {
        assert_eq!(SqlType::Text.name(), "text");
        assert_eq!(SqlType::Jsonb.name(), "jsonb");
    }
}
