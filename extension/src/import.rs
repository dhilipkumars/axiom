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
    /// clusters into one schema without a collision. Applied by
    /// [`assign_table_names`], not here.
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

/// `IMPORT FOREIGN SCHEMA ... OPTIONS` — how a whole schema is generated.
///
/// Same table-as-documentation arrangement as the server and foreign-table
/// options in [`crate::options`]: `make docs-generate` reads these literals to
/// build `docs/generated/fdw-options.md`, so an option added or removed here
/// changes the reference page and CI fails on the uncommitted diff.
pub const IMPORT_OPTION_DOCS: &[crate::options::OptionDoc] = &[
    crate::options::OptionDoc {
        name: "cache_mode",
        required: false,
        default: Some("on_demand"),
        summary: "applied to every generated table, and silently downgraded to \
                  on_demand for a kind the API server will not let the gateway watch",
    },
    crate::options::OptionDoc {
        name: "prefix",
        required: false,
        default: Some("\"\" (no prefix)"),
        summary: "prepended to every generated table name, so two clusters can be \
                  imported into one schema without colliding",
    },
];

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

/// Encodes an API group or resource name as an identifier fragment.
///
/// `.` becomes `_` and `-` becomes `__`. Both are legal in a DNS name and
/// neither in an unquoted identifier, and they have to stay distinguishable:
/// mapping both to `_` made `a-b.io` and `a.b.io` the same table. DNS rules
/// make the encoding reversible -- a label cannot begin or end with `-`, so a
/// run of underscores that came from `.` has length one and a run that came
/// from `-` has even length.
///
/// Returns `None` for anything that is not a lowercase DNS name, so a hostile
/// plural never reaches the generated DDL.
fn encode(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            'a'..='z' | '0'..='9' => out.push(c),
            '.' => out.push('_'),
            '-' => out.push_str("__"),
            _ => return None,
        }
    }
    Some(out)
}

/// An 8-hex-digit digest of a kind's own `group/plural`, for a name too long
/// to spell out. FNV-1a: tiny, identical across runs and machines, and not
/// used for anything security-sensitive. Keyed on the kind alone, never on
/// what else is being imported.
fn name_digest(group: &str, plural: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in group
        .bytes()
        .chain(std::iter::once(b'/'))
        .chain(plural.bytes())
    {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash & 0xffff_ffff)
}

/// The table name for one kind: `<group>_<plural>`, with the core group
/// spelled `core` -- `core_pods`, `apps_deployments`,
/// `postgresql_cnpg_io_clusters`.
///
/// The name is a function of the kind's own group and plural and nothing
/// else: not of what else the cluster serves, what RBAC grants, or the order
/// discovery lists kinds in. Deriving it from the set instead is what let
/// installing metrics-server rename `pods` to `pods_core` (#80), and what
/// makes CRDs that share a plural -- `CloudNativePG` and Cluster API both define
/// `clusters` -- rename each other on install.
///
/// A name longer than the identifier limit keeps its plural whole and
/// shortens the group, with a digest of the full `group/plural` between them:
/// `dbforpostg_aeb7bb51_flexibleserveractivedirectoryadministrators`. A plural
/// too long to keep is shortened itself and the digest ends the name. A name
/// that would start with a digit, from a group like `3scale.net`, is prefixed
/// with `_` so it stays usable unquoted.
///
/// Returns `None` when the kind's names are not DNS names or nothing usable
/// fits beside the prefix.
pub fn table_name(kind: &ImportKind, prefix: &str) -> Option<String> {
    let mut group = if kind.group.is_empty() {
        "core".to_owned()
    } else {
        encode(&kind.group)?
    };
    // Only a group can start with a digit; a plural is a DNS-1035 label.
    if group.starts_with(|c: char| c.is_ascii_digit()) {
        group.insert(0, '_');
    }
    let plural = encode(&kind.plural)?;
    let budget = MAX_IDENT.checked_sub(prefix.len())?;

    let mut name = format!("{group}_{plural}");
    if name.len() > budget {
        let tag = name_digest(&kind.group, &kind.plural);
        name = match budget.checked_sub(plural.len() + tag.len() + 2) {
            Some(room) if room >= 1 => {
                let g = group[..room.min(group.len())].trim_end_matches('_');
                format!("{g}_{tag}_{plural}")
            }
            _ => {
                let room = budget.checked_sub(tag.len() + 1).filter(|r| *r >= 1)?;
                let p = plural[..room.min(plural.len())].trim_end_matches('_');
                format!("{p}_{tag}")
            }
        };
    }
    let name = format!("{prefix}{name}");
    is_safe_ident(&name).then_some(name)
}

/// The plural a table name generated by [`table_name`] was made from, used to
/// narrow what `LIMIT TO` asks the gateway for.
///
/// The plural follows the last run of underscores with odd length: a plural
/// contains only the even runs that encode `-`, and the separator before it
/// is a single `_`. Returns `None` for a name without the prefix, or one whose
/// plural was shortened into a digest -- and for an eight-hex-digit plural,
/// which cannot be told apart from one. Callers must then ask for every kind
/// and filter by name locally: a wrong guess here would silently drop a table
/// the user named, where no guess only costs a wider request.
pub fn plural_from_table_name(table: &str, prefix: &str) -> Option<String> {
    let rest = table.strip_prefix(prefix)?;
    let bytes = rest.as_bytes();
    let mut i = bytes.len();
    let mut start = None;
    while i > 0 {
        if bytes[i - 1] == b'_' {
            let end = i;
            while i > 0 && bytes[i - 1] == b'_' {
                i -= 1;
            }
            if (end - i) % 2 == 1 {
                start = Some(end);
                break;
            }
        } else {
            i -= 1;
        }
    }
    let encoded = &rest[start?..];
    let digest_shaped = encoded.len() == 8 && encoded.bytes().all(|b| b.is_ascii_hexdigit());
    if encoded.is_empty() || digest_shaped {
        return None;
    }
    Some(encoded.replace("__", "-"))
}

/// Assigns a table name to every kind being imported, positionally.
///
/// Each name comes from [`table_name`] alone. The only interaction between
/// kinds is a final check that no two ended up with the same name, which the
/// encoding rules out for real API groups; a set that produces a duplicate
/// anyway -- an aggregated API whose group is literally `core`, say -- gets
/// neither name rather than whichever was listed first.
///
/// A kind whose name is unusable or duplicated gets an empty string, and the
/// caller warns and skips it.
pub fn assign_table_names(kinds: &[ImportKind], prefix: &str) -> Vec<String> {
    let names: Vec<Option<String>> = kinds.iter().map(|k| table_name(k, prefix)).collect();
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for n in names.iter().flatten() {
        *counts.entry(n.as_str()).or_default() += 1;
    }
    names
        .iter()
        .map(|n| match n {
            Some(n) if counts.get(n.as_str()) == Some(&1) => n.clone(),
            _ => String::new(),
        })
        .collect()
}

/// One message per `LIMIT TO` / `EXCEPT` entry that names no generated table.
///
/// Postgres matches those lists against table names and says nothing about an
/// entry that matches none. So `LIMIT TO (pods)` -- the spelling from before
/// tables were named for their group -- would import nothing, and the mistake
/// would surface later as "relation does not exist". An entry that is the
/// plural of an offered kind is answered with the table names it probably
/// meant.
pub fn unmatched_table_list(
    clause: &str,
    requested: &[String],
    offered: &[ImportKind],
    names: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    for want in requested {
        if names.contains(want) {
            continue;
        }
        let meant: Vec<&str> = offered
            .iter()
            .zip(names)
            .filter(|(k, n)| &k.plural == want && !n.is_empty())
            .map(|(_, n)| n.as_str())
            .collect();
        out.push(if meant.is_empty() {
            format!("{clause} names {want:?}, which is not a table this server offers")
        } else {
            format!(
                "{clause} names tables, not resources: no table is called {want:?}; \
                 did you mean {}?",
                meant.join(" or ")
            )
        });
    }
    out
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
    table: &str,
    server: &str,
    local_schema: &str,
    opts: &ImportOptions,
) -> Result<(String, Vec<String>), SkipReason> {
    if !is_safe_ident(table) {
        return Err(SkipReason::BadTableName(table.to_owned()));
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
        quote_ident(table)
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
/// filter, or `None` for "every kind this gateway serves".
///
/// Kubernetes has no schemas, so the remote-schema slot names an API group.
/// Two spellings mean "everything": the literal `k8s`, and the server's own
/// name. The latter exists because the model is one schema per cluster, which
/// makes `IMPORT FOREIGN SCHEMA prod FROM SERVER prod INTO prod` the natural
/// thing to type -- and without this it would silently filter for an API group
/// called `prod` and import nothing.
///
/// `core` and `v1` both mean the core API group, whose real name is the empty
/// string and so cannot be typed as a schema name. Anything else is taken as an
/// API group verbatim.
pub fn group_filter(remote_schema: &str, server_name: &str) -> Option<String> {
    if remote_schema == "k8s" || remote_schema == server_name {
        return None;
    }
    match remote_schema {
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
        let (sql, dropped) = create_table_sql(
            &widget(),
            "widgets",
            "prod",
            "k8s",
            &ImportOptions::default(),
        )
        .expect("valid");
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
        let (sql, _) = create_table_sql(&pod, "widgets", "prod", "k8s", &ImportOptions::default())
            .expect("valid");
        assert!(!sql.contains("group"), "{sql}");
        assert!(sql.contains("resource 'pods'"), "{sql}");
    }

    #[test]
    fn cluster_scoped_and_read_only_kinds_carry_their_options() {
        let mut k = widget();
        k.namespaced = false;
        k.writable = false;
        let (sql, _) = create_table_sql(&k, "widgets", "prod", "k8s", &ImportOptions::default())
            .expect("valid");
        assert!(sql.contains("namespaced 'false'"), "{sql}");
        assert!(sql.contains("writable 'false'"), "{sql}");
    }

    #[test]
    fn watch_cache_mode_is_only_emitted_for_watchable_kinds() {
        let opts = ImportOptions {
            cache_mode: CacheMode::Watch,
            prefix: String::new(),
        };
        let (sql, _) = create_table_sql(&widget(), "widgets", "prod", "k8s", &opts).expect("valid");
        assert!(sql.contains("cache_mode 'watch'"), "{sql}");

        let mut unwatchable = widget();
        unwatchable.watchable = false;
        let (sql, _) =
            create_table_sql(&unwatchable, "widgets", "prod", "k8s", &opts).expect("valid");
        assert!(
            !sql.contains("cache_mode"),
            "a kind the API server will not watch must not be given a watch table: {sql}"
        );
    }

    #[test]
    fn the_table_name_is_independent_of_the_resource_option() {
        // Naming is decided by assign_table_names, which qualifies and may
        // prefix or shorten; the `resource` option must always stay the plural
        // the gateway will be asked for.
        let (sql, _) = create_table_sql(
            &widget(),
            "prod_widgets_example_com",
            "prod",
            "k8s",
            &ImportOptions::default(),
        )
        .expect("valid");
        assert!(sql.contains("\"prod_widgets_example_com\""), "{sql}");
        assert!(
            sql.contains("resource 'widgets'"),
            "a renamed table must not change the resource option: {sql}"
        );
    }

    #[test]
    fn hostile_names_cannot_escape_the_generated_ddl() {
        // A CRD name is attacker-influenceable in a multi-tenant cluster.
        assert!(
            matches!(
                create_table_sql(
                    &widget(),
                    "widgets\"; DROP TABLE users; --",
                    "prod",
                    "k8s",
                    &ImportOptions::default()
                ),
                Err(SkipReason::BadTableName(_))
            ),
            "a table name outside the safe character set must be refused, not quoted and hoped for"
        );
        // And the naming pass refuses to produce such a name in the first place.
        let hostile = kind_named("", "widgets\"; DROP TABLE users; --");
        assert_eq!(
            assign_table_names(&[hostile], ""),
            vec![String::new()],
            "a hostile plural must yield no table name at all"
        );

        let mut k = widget();
        k.group = "example.com'); DROP TABLE users; --".into();
        assert!(matches!(
            create_table_sql(&k, "widgets", "prod", "k8s", &ImportOptions::default()),
            Err(SkipReason::BadIdentity("group", _))
        ));

        let mut k = widget();
        k.version = "v1/../../secrets".into();
        assert!(matches!(
            create_table_sql(&k, "widgets", "prod", "k8s", &ImportOptions::default()),
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
            create_table_sql(&k, "widgets", "prod", "k8s", &ImportOptions::default())
                .expect("valid");
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
            create_table_sql(&k, "widgets", "prod", "k8s", &ImportOptions::default())
                .expect("valid");
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
            create_table_sql(&k, "widgets", "prod", "k8s", &ImportOptions::default()),
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
            "widgets",
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

    /// Builds a kind with a given group and plural, for naming tests.
    fn kind_named(group: &str, plural: &str) -> ImportKind {
        let mut k = widget();
        k.group = group.into();
        k.plural = plural.into();
        k
    }

    #[test]
    fn every_name_is_qualified_by_its_group() {
        let kinds = vec![
            kind_named("", "pods"),
            kind_named("apps", "deployments"),
            kind_named("postgresql.cnpg.io", "clusters"),
            kind_named("metrics.k8s.io", "pods"),
        ];
        assert_eq!(
            assign_table_names(&kinds, ""),
            vec![
                "core_pods",
                "apps_deployments",
                "postgresql_cnpg_io_clusters",
                "metrics_k8s_io_pods"
            ]
        );
    }

    #[test]
    fn a_name_does_not_depend_on_what_else_is_imported() {
        // #80: installing metrics-server renamed `pods` to `pods_core`. A kind's
        // name must be the same alone, beside a same-plural kind, and in any
        // order.
        let core = kind_named("", "pods");
        let metrics = kind_named("metrics.k8s.io", "pods");
        let alone = assign_table_names(std::slice::from_ref(&core), "");
        let beside = assign_table_names(&[core.clone(), metrics.clone()], "");
        let reversed = assign_table_names(&[metrics, core], "");
        assert_eq!(alone[0], "core_pods");
        assert_eq!(beside[0], "core_pods");
        assert_eq!(reversed[1], "core_pods");

        // And the revocation case from the cluster gate: core events keep their
        // name whether or not events.k8s.io is imported beside them.
        let events = kind_named("", "events");
        let newer = kind_named("events.k8s.io", "events");
        assert_eq!(
            assign_table_names(std::slice::from_ref(&events), "")[0],
            assign_table_names(&[events, newer], "")[0]
        );
    }

    #[test]
    fn dots_and_dashes_encode_differently() {
        let kinds = vec![
            kind_named("a-b.io", "things"),
            kind_named("a.b.io", "things"),
            kind_named("x.io", "foo-bar"),
        ];
        assert_eq!(
            assign_table_names(&kinds, ""),
            vec!["a__b_io_things", "a_b_io_things", "x_io_foo__bar"]
        );
    }

    /// Every lowercase DNS subdomain over a small alphabet, up to a length: the
    /// cases that break an encoding are short, so exhaustive beats random.
    fn dns_names(max_len: usize) -> Vec<String> {
        let alphabet = ['a', '1', '-', '.'];
        let mut out = Vec::new();
        let mut frontier = vec![String::new()];
        for _ in 0..max_len {
            let mut next = Vec::new();
            for p in &frontier {
                for c in alphabet {
                    next.push(format!("{p}{c}"));
                }
            }
            out.extend(next.iter().cloned());
            frontier = next;
        }
        out.retain(|s| {
            s.split('.')
                .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'))
        });
        out
    }

    #[test]
    fn distinct_kinds_never_share_a_name() {
        // Exhaustive over short groups and plurals, including `--` runs and
        // groups that start with a digit. Plurals are DNS-1035 labels: no
        // dots, starting with a letter.
        let groups: Vec<String> = std::iter::once(String::new()).chain(dns_names(5)).collect();
        let plurals: Vec<String> = dns_names(4)
            .into_iter()
            .filter(|p| !p.contains('.') && p.starts_with('a'))
            .collect();
        let mut seen: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        for g in &groups {
            for p in &plurals {
                let name = table_name(&kind_named(g, p), "").expect("short names always fit");
                if let Some(prev) = seen.insert(name.clone(), (g.clone(), p.clone())) {
                    panic!("{name} is both {prev:?} and ({g:?}, {p:?})");
                }
            }
        }
        assert!(
            seen.len() > 5_000,
            "the search space is too small to mean anything"
        );
    }

    #[test]
    fn an_over_long_name_keeps_its_plural_and_gains_a_digest() {
        let k = kind_named(
            "dbforpostgresql.azure.m.upbound.io",
            "flexibleserveractivedirectoryadministrators",
        );
        assert_eq!(
            table_name(&k, "").as_deref(),
            Some("dbforpostg_aeb7bb51_flexibleserveractivedirectoryadministrators")
        );
        // Names that fit are left alone, however long.
        let fits = kind_named(
            "dbforpostgresql.azure.upbound.io",
            "flexibleserverconfigurations",
        );
        assert_eq!(
            table_name(&fits, "").as_deref(),
            Some("dbforpostgresql_azure_upbound_io_flexibleserverconfigurations")
        );
    }

    #[test]
    fn the_prefix_counts_against_the_identifier_limit() {
        let k = kind_named(
            "dbforpostgresql.azure.upbound.io",
            "flexibleserverconfigurations",
        );
        let name = table_name(&k, "prod_").expect("fits once shortened");
        assert!(name.len() <= MAX_IDENT, "{name} is {} bytes", name.len());
        assert!(name.starts_with("prod_"), "{name}");
        assert!(name.ends_with("_flexibleserverconfigurations"), "{name}");
    }

    #[test]
    fn a_plural_too_long_to_keep_is_shortened_into_a_digest() {
        let plural = format!("a{}", "b".repeat(61));
        let name = table_name(&kind_named("example.com", &plural), "").expect("fits");
        assert!(name.len() <= MAX_IDENT, "{name}");
        assert_eq!(
            plural_from_table_name(&name, ""),
            None,
            "a shortened plural cannot be decoded, and must not be guessed"
        );
    }

    #[test]
    fn shortening_through_an_encoded_dash_stays_distinct() {
        // Truncation can land inside `__`; the digest still separates groups
        // that differ only past the cut.
        let long = "flexibleserveractivedirectoryadministratorsx";
        let a = table_name(&kind_named("a-bbbbbbbbbbbbbbbb.io", long), "").expect("fits");
        let b = table_name(&kind_named("a-bbbbbbbbbbbbbbbc.io", long), "").expect("fits");
        assert_ne!(a, b);
        assert!(a.len() <= MAX_IDENT && b.len() <= MAX_IDENT);
    }

    #[test]
    fn a_group_starting_with_a_digit_stays_usable_unquoted() {
        assert_eq!(
            table_name(&kind_named("3scale.net", "apis"), "").as_deref(),
            Some("_3scale_net_apis")
        );
    }

    #[test]
    fn a_duplicate_name_goes_to_neither_kind() {
        // Only reachable through a pathological group; the rule must still not
        // depend on order.
        let a = kind_named("", "pods");
        let b = kind_named("core", "pods");
        assert_eq!(
            assign_table_names(&[a.clone(), b.clone()], ""),
            vec!["", ""]
        );
        assert_eq!(assign_table_names(&[b, a], ""), vec!["", ""]);
    }

    #[test]
    fn the_prefix_is_prepended() {
        let kinds = vec![
            kind_named("", "events"),
            kind_named("events.k8s.io", "events"),
        ];
        assert_eq!(
            assign_table_names(&kinds, "c1_"),
            vec!["c1_core_events", "c1_events_k8s_io_events"]
        );
    }

    #[test]
    fn the_plural_is_recovered_from_a_table_name() {
        for (group, plural) in [
            ("", "pods"),
            ("apps", "deployments"),
            ("events.k8s.io", "events"),
            ("a-b.io", "foo-bar"),
            ("x.io", "a--b"),
            ("3scale.net", "apis"),
            (
                "dbforpostgresql.azure.m.upbound.io",
                "flexibleserveractivedirectoryadministrators",
            ),
        ] {
            for prefix in ["", "c1_"] {
                let name = table_name(&kind_named(group, plural), prefix).expect("valid");
                assert_eq!(
                    plural_from_table_name(&name, prefix).as_deref(),
                    Some(plural),
                    "{name}"
                );
            }
        }
        assert_eq!(
            plural_from_table_name("pods", ""),
            None,
            "an old-style name has no group"
        );
        assert_eq!(
            plural_from_table_name("core_pods", "c1_"),
            None,
            "wrong prefix"
        );
    }

    #[test]
    fn every_decodable_name_decodes_to_its_own_plural() {
        let groups: Vec<String> = std::iter::once(String::new()).chain(dns_names(4)).collect();
        let plurals: Vec<String> = dns_names(4)
            .into_iter()
            .filter(|p| !p.contains('.') && p.starts_with('a'))
            .collect();
        for g in &groups {
            for p in &plurals {
                let name = table_name(&kind_named(g, p), "").expect("fits");
                assert_eq!(
                    plural_from_table_name(&name, "").as_deref(),
                    Some(p.as_str()),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn a_table_list_naming_a_plural_suggests_the_table() {
        let offered = vec![kind_named("", "pods"), kind_named("metrics.k8s.io", "pods")];
        let names = assign_table_names(&offered, "");
        let requested = vec!["pods".to_owned(), "core_pods".to_owned(), "nope".to_owned()];
        assert_eq!(
            unmatched_table_list("LIMIT TO", &requested, &offered, &names),
            vec![
                "LIMIT TO names tables, not resources: no table is called \"pods\"; \
                 did you mean core_pods or metrics_k8s_io_pods?"
                    .to_owned(),
                "LIMIT TO names \"nope\", which is not a table this server offers".to_owned(),
            ]
        );
    }

    #[test]
    fn remote_schema_maps_to_an_api_group() {
        assert_eq!(
            group_filter("k8s", "prod"),
            None,
            "k8s means every served kind"
        );
        assert_eq!(group_filter("core", "prod"), Some(String::new()));
        assert_eq!(
            group_filter("v1", "prod"),
            Some(String::new()),
            "the core group's real name is empty and cannot be typed as a schema"
        );
        assert_eq!(
            group_filter("example.com", "prod"),
            Some("example.com".to_owned())
        );
    }

    #[test]
    fn the_servers_own_name_means_every_served_kind() {
        // One schema per cluster makes `IMPORT FOREIGN SCHEMA prod FROM SERVER
        // prod INTO prod` the natural spelling. Without the synonym it would
        // filter for an API group called "prod" and import nothing at all.
        assert_eq!(group_filter("prod", "prod"), None);
        assert_eq!(group_filter("axiom_e2e", "axiom_e2e"), None);
        // A different server's name is still just a group name.
        assert_eq!(group_filter("prod", "staging"), Some("prod".to_owned()));
        // And a real group that happens to share the server's name resolves to
        // everything, which is the safe direction: a narrower-than-expected
        // import would look like the cluster is empty.
        assert_eq!(group_filter("example.com", "example.com"), None);
    }
}
