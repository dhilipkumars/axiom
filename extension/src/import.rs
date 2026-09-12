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

/// Normalizes an API group into an identifier fragment, for disambiguating a
/// table name. `events.k8s.io` becomes `events_k8s_io`; the core group, whose
/// real name is the empty string, becomes `core`.
fn group_suffix(group: &str) -> String {
    if group.is_empty() {
        return "core".to_owned();
    }
    group
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// A short, stable hex digest of `group`, for the last-resort disambiguation
/// step. FNV-1a: tiny, deterministic across runs and machines, and not used for
/// anything security-sensitive -- only to separate two API groups whose names
/// normalize to the same identifier fragment.
fn group_digest(group: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in group.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:06x}", hash & 0x00ff_ffff)
}

/// Assigns a table name to every kind being imported, in the order given.
///
/// A plural is only unique within an API group, so importing a whole cluster
/// into one schema collides: `events` exists in both the core group and
/// `events.k8s.io`, and under one schema per cluster both want the same table.
/// Left alone, the second `CREATE` fails and takes the entire import with it.
///
/// A kind whose plural is unique across the set keeps the bare name. When two
/// or more share a plural, *none* of them gets it and each is suffixed with its
/// group: `events_core` and `events_events_k8s_io`. Handing the bare name to
/// one of them would make `events` mean whichever the rule happened to favour,
/// which is exactly the ambiguity worth avoiding; the same reasoning drops both
/// sides of a colliding column rather than picking a winner.
///
/// Returns one name per input kind, positionally. A name that cannot be made
/// into a safe identifier is returned empty, and the caller skips that kind.
pub fn assign_table_names(kinds: &[ImportKind], prefix: &str) -> Vec<String> {
    // Three tiers of increasing specificity. A kind uses the least specific one
    // that nothing else is also using: the bare plural, the plural suffixed
    // with its API group, or that plus a digest of the group. The digest tier
    // exists because the suffix alone cannot always separate two groups --
    // `a-b.io` and `a.b.io` both normalize to `a_b_io` -- and because a kind
    // genuinely named `events_core` collides with the generated name for core
    // events.
    let tiers = |k: &ImportKind| {
        let suffixed = format!("{}_{}", k.plural, group_suffix(&k.group));
        [
            format!("{prefix}{}", k.plural),
            format!("{prefix}{suffixed}"),
            format!("{prefix}{suffixed}_{}", group_digest(&k.group)),
        ]
    };
    let all: Vec<[String; 3]> = kinds.iter().map(tiers).collect();

    // Start everyone at the least specific tier, then promote *every* member of
    // any colliding set until nothing collides. Promoting the whole set rather
    // than the later claimant is what makes the result independent of the order
    // discovery returned kinds in, so re-importing the same cluster produces
    // the same table names. It also keeps the rule consistent with colliding
    // columns: an ambiguous name goes to nobody rather than to whoever asked
    // first.
    let mut tier = vec![0usize; kinds.len()];
    loop {
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (i, t) in tier.iter().enumerate() {
            *counts.entry(all[i][*t].as_str()).or_default() += 1;
        }
        let mut promoted = false;
        for i in 0..kinds.len() {
            let name = all[i][tier[i]].as_str();
            // An unusable name is promoted for the same reason a colliding one
            // is: a more specific tier may well be usable.
            let bad = !is_safe_ident(name) || counts.get(name).copied().unwrap_or(0) > 1;
            if bad && tier[i] + 1 < all[i].len() {
                tier[i] += 1;
                promoted = true;
            }
        }
        if !promoted {
            break;
        }
    }

    // Anything still unusable or duplicated has exhausted its tiers: a hostile
    // plural, or a name too long at every tier. The caller warns and skips it.
    let mut taken: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(kinds.len());
    for i in 0..kinds.len() {
        let name = all[i][tier[i]].as_str();
        if !is_safe_ident(name) || !taken.insert(name) {
            out.push(String::new());
            continue;
        }
        out.push(name.to_owned());
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
        // Naming is decided by assign_table_names, which may prefix or
        // disambiguate; the `resource` option must always stay the plural the
        // gateway will be asked for.
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
    fn unique_plurals_keep_their_bare_name() {
        let kinds = vec![
            kind_named("", "pods"),
            kind_named("apps", "deployments"),
            kind_named("example.com", "widgets"),
        ];
        assert_eq!(
            assign_table_names(&kinds, ""),
            vec!["pods", "deployments", "widgets"]
        );
    }

    #[test]
    fn a_colliding_plural_disambiguates_both_sides_by_group() {
        // `events` really does exist in both the core group and events.k8s.io.
        let kinds = vec![
            kind_named("", "events"),
            kind_named("events.k8s.io", "events"),
            kind_named("", "pods"),
        ];
        let got = assign_table_names(&kinds, "");
        assert_eq!(got, vec!["events_core", "events_events_k8s_io", "pods"]);
        assert!(
            !got.contains(&"events".to_owned()),
            "neither side may keep the bare name: `events` would silently mean \
             whichever the rule favoured"
        );
    }

    #[test]
    fn three_way_collisions_all_disambiguate() {
        let kinds = vec![
            kind_named("", "things"),
            kind_named("a.io", "things"),
            kind_named("b.io", "things"),
        ];
        assert_eq!(
            assign_table_names(&kinds, ""),
            vec!["things_core", "things_a_io", "things_b_io"]
        );
    }

    #[test]
    fn the_prefix_applies_after_disambiguation() {
        let kinds = vec![
            kind_named("", "events"),
            kind_named("events.k8s.io", "events"),
        ];
        assert_eq!(
            assign_table_names(&kinds, "c1_"),
            vec!["c1_events_core", "c1_events_events_k8s_io"]
        );
    }

    #[test]
    fn a_name_that_cannot_be_an_identifier_is_skipped_not_mangled() {
        let kinds = vec![
            kind_named("", &"a".repeat(MAX_IDENT + 1)),
            kind_named("", "pods"),
        ];
        let got = assign_table_names(&kinds, "");
        assert_eq!(got[0], "", "an over-long name yields no table");
        assert_eq!(got[1], "pods", "and does not disturb the others");
    }

    #[test]
    fn a_disambiguated_name_colliding_with_a_real_kind_still_gets_one() {
        // A cluster with both a colliding `events` pair and a kind genuinely
        // called `events_core`. Every one of the three is permitted, so every
        // one must get a table: dropping any would contradict the point of
        // importing a whole cluster.
        let kinds = vec![
            kind_named("", "events"),
            kind_named("events.k8s.io", "events"),
            kind_named("other.io", "events_core"),
        ];
        let got = assign_table_names(&kinds, "");
        assert_eq!(
            got.iter().filter(|n| n.is_empty()).count(),
            0,
            "every permitted kind must get a table: {got:?}"
        );
        let unique: std::collections::HashSet<&String> = got.iter().collect();
        assert_eq!(unique.len(), 3, "names must be distinct: {got:?}");
        assert_eq!(got[1], "events_events_k8s_io");
        // Core events and the real `events_core` both wanted `events_core`, so
        // neither keeps it -- the same rule as any other ambiguous name.
        assert!(got[0].starts_with("events_core"), "{got:?}");
        assert!(got[2].starts_with("events_core"), "{got:?}");
    }

    #[test]
    fn groups_that_normalize_alike_are_separated_by_a_digest() {
        // `a-b.io` and `a.b.io` are different API groups that both normalize to
        // `a_b_io`, so the group suffix alone cannot tell them apart.
        let kinds = vec![
            kind_named("a-b.io", "things"),
            kind_named("a.b.io", "things"),
        ];
        let got = assign_table_names(&kinds, "");
        assert_eq!(got.len(), 2);
        assert!(
            got.iter().all(|n| !n.is_empty()),
            "neither may be dropped: {got:?}"
        );
        assert_ne!(got[0], got[1], "the two must get distinct names: {got:?}");
        assert!(got[0].starts_with("things_a_b_io"), "{got:?}");
        assert!(got[1].starts_with("things_a_b_io"), "{got:?}");
    }

    #[test]
    fn assigned_names_are_stable_regardless_of_order() {
        // The digest is of the group itself, not of position, so re-importing
        // the same cluster yields the same table names whatever order
        // discovery happened to return.
        let a = kind_named("a-b.io", "things");
        let b = kind_named("a.b.io", "things");
        let forward = assign_table_names(&[a.clone(), b.clone()], "");
        let reverse = assign_table_names(&[b, a], "");
        assert_eq!(forward[0], reverse[1]);
        assert_eq!(forward[1], reverse[0]);
    }

    #[test]
    fn every_permitted_kind_receives_a_unique_name() {
        let kinds = vec![
            kind_named("", "events"),
            kind_named("events.k8s.io", "events"),
            kind_named("a-b.io", "things"),
            kind_named("a.b.io", "things"),
            kind_named("other.io", "events_core"),
            kind_named("", "pods"),
        ];
        let got = assign_table_names(&kinds, "");
        assert_eq!(got.iter().filter(|n| n.is_empty()).count(), 0, "{got:?}");
        let unique: std::collections::HashSet<&String> = got.iter().collect();
        assert_eq!(unique.len(), got.len(), "names are not unique: {got:?}");
    }

    #[test]
    fn group_digest_is_stable_and_distinguishes_similar_groups() {
        assert_eq!(group_digest("a-b.io"), group_digest("a-b.io"));
        assert_ne!(group_digest("a-b.io"), group_digest("a.b.io"));
        assert_eq!(group_digest("example.com").len(), 6);
        assert!(group_digest("example.com")
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn group_suffix_normalizes_to_an_identifier_fragment() {
        assert_eq!(group_suffix(""), "core");
        assert_eq!(group_suffix("events.k8s.io"), "events_k8s_io");
        assert_eq!(group_suffix("example.com"), "example_com");
        assert_eq!(group_suffix("Mixed.Case"), "mixed_case");
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
