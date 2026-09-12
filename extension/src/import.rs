//! `IMPORT FOREIGN SCHEMA`: turning a gateway's discovered kinds into
//! `CREATE FOREIGN TABLE` statements.
//!
//! The DDL generated here embeds names that originate in the cluster. In a
//! multi-tenant cluster a CRD's group, kind and field names are influenced by
//! whoever may create a `CustomResourceDefinition`, and they reach this code
//! across the gRPC boundary, so they are untrusted input twice over
//! (docs/RULES.md §3). Nothing is interpolated without being both validated
//! against a strict character rule and quoted per Postgres identifier syntax;
//! a name that fails validation drops its column, or its whole table, rather
//! than being sanitised into something that might collide with another.
//!
//! This module is pure: it takes already-fetched schemas and returns strings.
//! The FDW callback that calls the gateway and hands the result to Postgres
//! lives in `fdw.rs`.

use std::fmt::Write as _;

use crate::cache::CacheMode;

/// A column as the gateway described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportColumn {
    /// SQL column name.
    pub name: String,
    /// SQL type name, already narrowed to `text` or `jsonb`.
    pub sql_type: &'static str,
}

/// One kind as the gateway described it, reduced to what DDL generation needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportKind {
    /// API group; empty for the core group.
    pub group: String,
    /// API version.
    pub version: String,
    /// Kind.
    pub kind: String,
    /// Plural resource name; also the generated table name.
    pub plural: String,
    /// Whether objects of this kind live in a namespace.
    pub namespaced: bool,
    /// Whether the API server advertises create/update/delete.
    pub writable: bool,
    /// Whether the API server advertises watch.
    pub watchable: bool,
    /// Columns to declare.
    pub columns: Vec<ImportColumn>,
}

/// Why a kind could not be turned into DDL. Every variant names the kind so the
/// `WARNING` the caller emits is actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The table name is not a usable SQL identifier.
    BadTableName(String),
    /// Group, version or kind is not a valid Kubernetes name.
    BadIdentity(&'static str, String),
    /// After dropping unusable columns, nothing was left to select.
    NoColumns,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadTableName(n) => {
                write!(f, "resource name {n:?} is not a usable SQL identifier")
            }
            Self::BadIdentity(what, v) => {
                write!(f, "{what} {v:?} is not a valid Kubernetes name")
            }
            Self::NoColumns => write!(f, "no usable columns"),
        }
    }
}

/// Options accepted by `IMPORT FOREIGN SCHEMA ... OPTIONS (...)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOptions {
    /// `cache_mode` applied to every generated table.
    pub cache_mode: CacheMode,
    /// `prefix` prepended to every generated table name, for importing two
    /// clusters into one schema without a collision.
    pub prefix: String,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            cache_mode: CacheMode::OnDemand,
            prefix: String::new(),
        }
    }
}

/// Maximum length of a Postgres identifier (NAMEDATALEN - 1). A longer name is
/// truncated by the server, which could silently collide two tables, so a name
/// that does not fit is refused instead.
const MAX_IDENT: usize = 63;

/// Whether a string is safe to use as a generated SQL identifier.
///
/// Deliberately stricter than Postgres itself allows: lowercase ASCII letters,
/// digits and underscores, not starting with a digit, within the identifier
/// length limit. Everything this module generates comes from the gateway's own
/// normalization, which produces exactly this shape, so a name failing here
/// means the value did not come from where it should have.
pub fn is_safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_IDENT
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Whether a string is usable as a Kubernetes group/version/kind in a table
/// option. Mirrors the validation `Resource` applies when the generated DDL is
/// later parsed, so IMPORT never emits DDL that its own validator rejects.
fn is_safe_k8s_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Quotes an identifier for use in generated DDL.
///
/// Always quotes rather than only when necessary, and doubles any embedded
/// quote. Callers validate with [`is_safe_ident`] first; this is the second
/// layer, so a validation gap cannot become an injection.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Quotes a string literal for use in generated DDL, doubling embedded
/// single quotes.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Generates the `CREATE FOREIGN TABLE` statement for one kind.
///
/// Returns the statement, plus the names of any columns that were dropped
/// because they were not usable identifiers, so the caller can warn about them.
/// Every dropped column remains reachable through `raw`.
///
/// # Errors
///
/// Returns [`SkipReason`] when the kind cannot be represented at all: an
/// unusable table name, an identity that would not survive the FDW validator,
/// or nothing left to select after dropping columns.
pub fn create_table_sql(
    kind: &ImportKind,
    server: &str,
    local_schema: &str,
    opts: &ImportOptions,
) -> Result<(String, Vec<String>), SkipReason> {
    let table = format!("{}{}", opts.prefix, kind.plural);
    if !is_safe_ident(&table) {
        return Err(SkipReason::BadTableName(table));
    }
    for (what, v) in [
        ("version", &kind.version),
        ("kind", &kind.kind),
        ("resource", &kind.plural),
    ] {
        if !is_safe_k8s_name(v) {
            return Err(SkipReason::BadIdentity(what, v.clone()));
        }
    }
    if !kind.group.is_empty() && !is_safe_k8s_name(&kind.group) {
        return Err(SkipReason::BadIdentity("group", kind.group.clone()));
    }

    let mut dropped = Vec::new();
    let mut cols = Vec::new();
    for c in &kind.columns {
        if !is_safe_ident(&c.name) {
            dropped.push(c.name.clone());
            continue;
        }
        if cols.iter().any(|(n, _): &(String, &str)| *n == c.name) {
            // The gateway drops colliding columns already; a duplicate here
            // would be a second definition of the same attribute, which
            // Postgres rejects with a far less clear message.
            dropped.push(c.name.clone());
            continue;
        }
        cols.push((c.name.clone(), c.sql_type));
    }
    if cols.is_empty() {
        return Err(SkipReason::NoColumns);
    }

    let mut sql = String::with_capacity(256 + cols.len() * 32);
    write!(
        sql,
        "CREATE FOREIGN TABLE {}.{} (",
        quote_ident(local_schema),
        quote_ident(&table)
    )
    .expect("writing to a String cannot fail");
    for (i, (name, ty)) in cols.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        write!(sql, "{} {ty}", quote_ident(name)).expect("writing to a String cannot fail");
    }
    write!(sql, ") SERVER {} OPTIONS (", quote_ident(server))
        .expect("writing to a String cannot fail");

    let opt = |sql: &mut String, first: &mut bool, k: &str, v: &str| {
        if !*first {
            sql.push_str(", ");
        }
        *first = false;
        write!(sql, "{k} {}", quote_literal(v)).expect("writing to a String cannot fail");
    };
    let mut first = true;
    opt(&mut sql, &mut first, "resource", &kind.plural);
    if !kind.group.is_empty() {
        opt(&mut sql, &mut first, "group", &kind.group);
    }
    opt(&mut sql, &mut first, "version", &kind.version);
    opt(&mut sql, &mut first, "kind", &kind.kind);
    if !kind.namespaced {
        opt(&mut sql, &mut first, "namespaced", "false");
    }
    if !kind.writable {
        opt(&mut sql, &mut first, "writable", "false");
    }
    // A watch is only offered where the API server actually supports one;
    // asking for `cache_mode 'watch'` on an unwatchable kind would leave a
    // subscription stuck rather than failing honestly.
    if opts.cache_mode == CacheMode::Watch && kind.watchable {
        opt(&mut sql, &mut first, "cache_mode", "watch");
    }
    sql.push(')');
    Ok((sql, dropped))
}

/// Resolves the `remote_schema` of an `IMPORT FOREIGN SCHEMA` to an API group
/// filter.
///
/// `k8s` means every kind the gateway serves. `core` and `v1` both mean the
/// core API group, whose real name is the empty string and so cannot be typed
/// as a schema name. Anything else is taken as the API group verbatim.
pub fn group_filter(remote_schema: &str) -> Option<String> {
    match remote_schema {
        "k8s" => None,
        "core" | "v1" => Some(String::new()),
        other => Some(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widget() -> ImportKind {
        ImportKind {
            group: "example.com".into(),
            version: "v1".into(),
            kind: "Widget".into(),
            plural: "widgets".into(),
            namespaced: true,
            writable: true,
            watchable: true,
            columns: vec![
                ImportColumn {
                    name: "name".into(),
                    sql_type: "text",
                },
                ImportColumn {
                    name: "namespace".into(),
                    sql_type: "text",
                },
                ImportColumn {
                    name: "spec".into(),
                    sql_type: "jsonb",
                },
                ImportColumn {
                    name: "raw".into(),
                    sql_type: "jsonb",
                },
            ],
        }
    }

    #[test]
    fn generates_ddl_a_table_definition_round_trips_from() {
        let (sql, dropped) =
            create_table_sql(&widget(), "prod", "k8s", &ImportOptions::default()).expect("valid");
        assert!(dropped.is_empty());
        assert_eq!(
            sql,
            "CREATE FOREIGN TABLE \"k8s\".\"widgets\" \
             (\"name\" text, \"namespace\" text, \"spec\" jsonb, \"raw\" jsonb) \
             SERVER \"prod\" OPTIONS (resource 'widgets', group 'example.com', \
             version 'v1', kind 'Widget')"
        );
    }

    #[test]
    fn core_group_kinds_omit_the_group_option() {
        let mut pod = widget();
        pod.group = String::new();
        pod.kind = "Pod".into();
        pod.plural = "pods".into();
        let (sql, _) =
            create_table_sql(&pod, "prod", "k8s", &ImportOptions::default()).expect("valid");
        assert!(!sql.contains("group"), "{sql}");
        assert!(sql.contains("resource 'pods'"), "{sql}");
    }

    #[test]
    fn cluster_scoped_and_read_only_kinds_carry_their_options() {
        let mut k = widget();
        k.namespaced = false;
        k.writable = false;
        let (sql, _) =
            create_table_sql(&k, "prod", "k8s", &ImportOptions::default()).expect("valid");
        assert!(sql.contains("namespaced 'false'"), "{sql}");
        assert!(sql.contains("writable 'false'"), "{sql}");
    }

    #[test]
    fn watch_cache_mode_is_only_emitted_for_watchable_kinds() {
        let opts = ImportOptions {
            cache_mode: CacheMode::Watch,
            prefix: String::new(),
        };
        let (sql, _) = create_table_sql(&widget(), "prod", "k8s", &opts).expect("valid");
        assert!(sql.contains("cache_mode 'watch'"), "{sql}");

        let mut unwatchable = widget();
        unwatchable.watchable = false;
        let (sql, _) = create_table_sql(&unwatchable, "prod", "k8s", &opts).expect("valid");
        assert!(
            !sql.contains("cache_mode"),
            "a kind the API server will not watch must not be given a watch table: {sql}"
        );
    }

    #[test]
    fn prefix_option_renames_the_table_only() {
        let opts = ImportOptions {
            cache_mode: CacheMode::OnDemand,
            prefix: "prod_".into(),
        };
        let (sql, _) = create_table_sql(&widget(), "prod", "k8s", &opts).expect("valid");
        assert!(sql.contains("\"prod_widgets\""), "{sql}");
        assert!(
            sql.contains("resource 'widgets'"),
            "the prefix must not change the resource option: {sql}"
        );
    }

    #[test]
    fn hostile_names_cannot_escape_the_generated_ddl() {
        // A CRD name is attacker-influenceable in a multi-tenant cluster.
        let mut k = widget();
        k.plural = "widgets\"; DROP TABLE users; --".into();
        assert!(
            matches!(
                create_table_sql(&k, "prod", "k8s", &ImportOptions::default()),
                Err(SkipReason::BadTableName(_))
            ),
            "a table name outside the safe character set must be refused, not quoted and hoped for"
        );

        let mut k = widget();
        k.group = "example.com'); DROP TABLE users; --".into();
        assert!(matches!(
            create_table_sql(&k, "prod", "k8s", &ImportOptions::default()),
            Err(SkipReason::BadIdentity("group", _))
        ));

        let mut k = widget();
        k.version = "v1/../../secrets".into();
        assert!(matches!(
            create_table_sql(&k, "prod", "k8s", &ImportOptions::default()),
            Err(SkipReason::BadIdentity("version", _))
        ));
    }

    #[test]
    fn an_unusable_column_is_dropped_and_reported_not_renamed() {
        let mut k = widget();
        k.columns.insert(
            2,
            ImportColumn {
                name: "Bad Name\"".into(),
                sql_type: "jsonb",
            },
        );
        let (sql, dropped) =
            create_table_sql(&k, "prod", "k8s", &ImportOptions::default()).expect("valid");
        assert_eq!(dropped, vec!["Bad Name\"".to_owned()]);
        assert!(!sql.contains("Bad Name"), "{sql}");
        assert!(
            sql.contains("\"raw\" jsonb"),
            "raw must survive: it is where a dropped column is still reachable"
        );
    }

    #[test]
    fn a_duplicate_column_is_dropped_rather_than_defined_twice() {
        let mut k = widget();
        k.columns.push(ImportColumn {
            name: "spec".into(),
            sql_type: "jsonb",
        });
        let (sql, dropped) =
            create_table_sql(&k, "prod", "k8s", &ImportOptions::default()).expect("valid");
        assert_eq!(dropped, vec!["spec".to_owned()]);
        assert_eq!(sql.matches("\"spec\"").count(), 1, "{sql}");
    }

    #[test]
    fn a_kind_with_no_usable_columns_is_skipped() {
        let mut k = widget();
        k.columns = vec![ImportColumn {
            name: "Bad".into(),
            sql_type: "jsonb",
        }];
        assert_eq!(
            create_table_sql(&k, "prod", "k8s", &ImportOptions::default()),
            Err(SkipReason::NoColumns)
        );
    }

    #[test]
    fn quoting_doubles_embedded_delimiters() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_literal("a'b"), "'a''b'");
        // The server name comes from the DDL the user typed, so it is quoted
        // rather than validated.
        let (sql, _) = create_table_sql(
            &widget(),
            "weird\"server",
            "sch\"ema",
            &ImportOptions::default(),
        )
        .expect("valid");
        assert!(sql.contains("\"weird\"\"server\""), "{sql}");
        assert!(sql.contains("\"sch\"\"ema\""), "{sql}");
    }

    #[test]
    fn safe_ident_rules() {
        for ok in ["name", "resource_version", "x1", "_leading"] {
            assert!(is_safe_ident(ok), "{ok} should be safe");
        }
        for bad in ["", "Name", "1st", "with-dash", "with space", "a\"b"] {
            assert!(!is_safe_ident(bad), "{bad} should be rejected");
        }
        assert!(is_safe_ident(&"a".repeat(MAX_IDENT)));
        assert!(
            !is_safe_ident(&"a".repeat(MAX_IDENT + 1)),
            "an over-long name would be truncated by the server into a possible collision"
        );
    }

    #[test]
    fn remote_schema_maps_to_an_api_group() {
        assert_eq!(group_filter("k8s"), None, "k8s means every served kind");
        assert_eq!(group_filter("core"), Some(String::new()));
        assert_eq!(
            group_filter("v1"),
            Some(String::new()),
            "the core group's real name is empty and cannot be typed as a schema"
        );
        assert_eq!(group_filter("example.com"), Some("example.com".to_owned()));
    }
}
