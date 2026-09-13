//! Pure parsing/validation of `CREATE SERVER` / `CREATE FOREIGN TABLE` options.
//!
//! The FDW validator and scan callbacks hand raw `(key, value)` pairs in here;
//! nothing in this module touches Postgres.

use std::fmt;
use std::time::Duration;

use crate::cache::CacheMode;
use crate::resource::{self, InlineError, Resource};
use crate::transport::{Target, TargetError};

/// Which catalog an option list belongs to (mirrors the validator's `catalog`
/// argument, mapped from OIDs by the glue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalog {
    /// `CREATE FOREIGN DATA WRAPPER ... OPTIONS`.
    Wrapper,
    /// `CREATE SERVER ... OPTIONS`.
    Server,
    /// `CREATE FOREIGN TABLE ... OPTIONS`.
    Table,
    /// `CREATE USER MAPPING ... OPTIONS`.
    UserMapping,
}

/// Validated `CREATE SERVER` options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOptions {
    /// Gateway address and TLS settings (`endpoint`, `ca_cert`).
    pub target: Target,
    /// Per-RPC deadline (`rpc_timeout_secs`, default 30).
    pub rpc_timeout: Duration,
}

/// Validated `CREATE FOREIGN TABLE` options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableOptions {
    /// The fully resolved kind this table maps to.
    pub resource: Resource,
    /// `cache_mode`: serve scans from the watch cache or always via RPC.
    pub cache_mode: CacheMode,
    /// Whether INSERT/UPDATE/DELETE are offered on this table.
    pub writable: bool,
}

/// Why an option list was rejected. Every variant names the offending option
/// so the SQL error is actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionsError {
    /// Option not recognised for this catalog.
    Unknown { catalog: Catalog, name: String },
    /// Same option given twice.
    Duplicate(String),
    /// Required option missing.
    Missing(&'static str),
    /// `endpoint` / `ca_cert` failed [`Target::parse`].
    Endpoint(TargetError),
    /// `rpc_timeout_secs` not a positive integer.
    Timeout(String),
    /// `resource` is not a built-in and no explicit identity was given.
    Resource(String),
    /// `cache_mode` is not `on_demand` or `watch`.
    CacheMode(String),
    /// A group/version/kind/resource value is malformed or over-long.
    Identity(InlineError),
    /// A boolean option is not `true` or `false`.
    Bool(&'static str, String),
    /// `writable 'true'` was given for a kind the extension keeps read-only.
    ForcedWritable(String),
}

impl fmt::Display for OptionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { catalog, name } => write!(
                f,
                "invalid option {name:?} for {catalog:?}: {}",
                allowed(*catalog)
            ),
            Self::Duplicate(n) => write!(f, "option {n:?} given more than once"),
            Self::Missing(n) => write!(f, "required option {n:?} is missing"),
            Self::Endpoint(e) => write!(f, "option \"endpoint\": {e}"),
            Self::Timeout(v) => write!(
                f,
                "option \"rpc_timeout_secs\" must be a positive integer (got {v:?})"
            ),
            Self::Resource(v) => write!(
                f,
                "option \"resource\" {v:?} needs options \"version\" and \"kind\" \
                 (and \"group\" for a non-core API group) to identify it; \
                 only {} are known without them. IMPORT FOREIGN SCHEMA writes these for you",
                resource::builtin_names().join(", ")
            ),
            Self::CacheMode(v) => write!(
                f,
                "option \"cache_mode\" {v:?} is not supported; valid values: on_demand, watch"
            ),
            Self::Identity(e) => write!(f, "{e}"),
            Self::Bool(name, v) => {
                write!(f, "option {name:?} must be true or false (got {v:?})")
            }
            Self::ForcedWritable(r) => write!(
                f,
                "option \"writable\" cannot be true for {r}: the extension keeps this kind \
                 read-only because SQL UPDATE has no sane meaning for it"
            ),
        }
    }
}

impl std::error::Error for OptionsError {}

/// One accepted option: what the validator checks names against, and what
/// `docs/generated/fdw-options.md` is generated from.
///
/// The two uses are deliberately the same table. A reference page maintained
/// alongside the allowlist drifts the first time an option is added or
/// removed and nobody notices; one generated from the allowlist the validator
/// itself consults cannot. `make docs-generate` reads the literals below and
/// CI fails on an uncommitted diff (docs/PLAN.md Phase 6 Part 2), so the
/// enforcement is mechanical rather than a review convention.
///
/// Keep the fields as plain literals. The generator parses this source rather
/// than linking the crate, because the crate is a pgrx `cdylib` that cannot be
/// built or run outside a Postgres build, and it fails loudly rather than
/// emitting a partial page if the shape here stops matching.
pub struct OptionDoc {
    /// Option name as written in `OPTIONS (...)`.
    pub name: &'static str,
    /// Whether the statement is rejected when it is absent.
    pub required: bool,
    /// Value used when it is absent, or `None` when there is no default.
    pub default: Option<&'static str>,
    /// One line, in the voice of the error the validator would raise.
    pub summary: &'static str,
}

/// `CREATE SERVER ... OPTIONS` — how to reach a gateway.
pub const SERVER_OPTION_DOCS: &[OptionDoc] = &[
    OptionDoc {
        name: "endpoint",
        required: true,
        default: None,
        summary: "gateway address as an https URL, e.g. https://axiom-gateway:8443; \
                  plaintext is refused because the connection carries cluster data",
    },
    OptionDoc {
        name: "ca_cert",
        required: false,
        default: Some("the host trust store"),
        summary: "path, readable by the Postgres server process, to the PEM CA bundle \
                  that signs the gateway certificate",
    },
    OptionDoc {
        name: "rpc_timeout_secs",
        required: false,
        default: Some("30"),
        summary: "per-RPC deadline in whole seconds, greater than zero; a whole-cluster \
                  IMPORT FOREIGN SCHEMA can need more than the default",
    },
];

/// `CREATE FOREIGN TABLE ... OPTIONS` — which kind a table maps to.
pub const TABLE_OPTION_DOCS: &[OptionDoc] = &[
    OptionDoc {
        name: "resource",
        required: true,
        default: None,
        summary: "plural resource name, e.g. pods; sufficient on its own for the \
                  built-in kinds",
    },
    OptionDoc {
        name: "group",
        required: false,
        default: Some("\"\" (the core API group)"),
        summary: "API group of the kind, e.g. apps or example.com",
    },
    OptionDoc {
        name: "version",
        required: false,
        default: None,
        summary: "API version, e.g. v1; required together with kind for any kind that \
                  is not built in",
    },
    OptionDoc {
        name: "kind",
        required: false,
        default: None,
        summary: "singular CamelCase kind, e.g. Widget; required together with version \
                  for any kind that is not built in",
    },
    OptionDoc {
        name: "namespaced",
        required: false,
        default: Some("true"),
        summary: "whether the kind is namespaced; false makes the namespace column \
                  meaningless and it is omitted from generated DDL",
    },
    OptionDoc {
        name: "writable",
        required: false,
        default: Some("false for built-in read-only kinds, true otherwise"),
        summary: "whether INSERT, UPDATE and DELETE are offered; cannot be turned on \
                  for kinds the extension keeps read-only, such as Pods",
    },
    OptionDoc {
        name: "cache_mode",
        required: false,
        default: Some("on_demand"),
        summary: "on_demand serves every scan by RPC; watch serves scans from the \
                  watch-driven cache and starts a subscription for the kind",
    },
];

const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// The options accepted for `catalog`. Wrapper and user mapping accept none:
/// the wrapper carries no configuration, and a user mapping will only start
/// carrying one in Phase 7 (docs/AUTH.md).
fn option_docs(catalog: Catalog) -> &'static [OptionDoc] {
    match catalog {
        Catalog::Server => SERVER_OPTION_DOCS,
        Catalog::Table => TABLE_OPTION_DOCS,
        Catalog::Wrapper | Catalog::UserMapping => &[],
    }
}

fn allowed(catalog: Catalog) -> String {
    let docs = option_docs(catalog);
    if docs.is_empty() {
        "no options are accepted".to_owned()
    } else {
        let names: Vec<&str> = docs.iter().map(|d| d.name).collect();
        format!("valid options are {}", names.join(", "))
    }
}

/// Checks that every option name is allowed for `catalog` and none repeats.
fn check_names(catalog: Catalog, opts: &[(String, String)]) -> Result<(), OptionsError> {
    let docs = option_docs(catalog);
    for (i, (k, _)) in opts.iter().enumerate() {
        if !docs.iter().any(|d| d.name == k.as_str()) {
            return Err(OptionsError::Unknown {
                catalog,
                name: k.clone(),
            });
        }
        if opts[..i].iter().any(|(k2, _)| k2 == k) {
            return Err(OptionsError::Duplicate(k.clone()));
        }
    }
    Ok(())
}

fn get<'a>(opts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    opts.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

impl ServerOptions {
    /// Parses server options. `endpoint` is required and must be `https`.
    pub fn parse(opts: &[(String, String)]) -> Result<Self, OptionsError> {
        check_names(Catalog::Server, opts)?;
        if get(opts, "endpoint").is_none() {
            return Err(OptionsError::Missing("endpoint"));
        }
        let target = Target::parse(get(opts, "endpoint"), get(opts, "ca_cert"))
            .map_err(OptionsError::Endpoint)?;
        let rpc_timeout = match get(opts, "rpc_timeout_secs") {
            None => DEFAULT_RPC_TIMEOUT,
            Some(v) => match v.trim().parse::<u64>() {
                Ok(n) if n > 0 => Duration::from_secs(n),
                _ => return Err(OptionsError::Timeout(v.to_owned())),
            },
        };
        Ok(Self {
            target,
            rpc_timeout,
        })
    }
}

/// Parses a boolean option value.
fn parse_bool(name: &'static str, v: &str) -> Result<bool, OptionsError> {
    match v.trim() {
        "true" | "t" | "yes" | "on" | "1" => Ok(true),
        "false" | "f" | "no" | "off" | "0" => Ok(false),
        other => Err(OptionsError::Bool(name, other.to_owned())),
    }
}

impl TableOptions {
    /// Parses foreign-table options.
    ///
    /// `resource` is always required. For the built-in kinds it is sufficient
    /// on its own, which keeps every Phase 1-3 table definition working
    /// unchanged. Any other kind must also carry `version` and `kind` (and
    /// `group` unless it is in the core API group), which is what
    /// `IMPORT FOREIGN SCHEMA` generates.
    ///
    /// Resolution is entirely local: no option is looked up against the
    /// gateway, so defining a table never depends on the cluster being
    /// reachable and a scan never pays for discovery.
    pub fn parse(opts: &[(String, String)]) -> Result<Self, OptionsError> {
        check_names(Catalog::Table, opts)?;
        let plural = get(opts, "resource")
            .ok_or(OptionsError::Missing("resource"))?
            .trim();

        let explicit = get(opts, "kind").is_some() || get(opts, "version").is_some();
        let (resource, default_writable) = if explicit {
            let kind = get(opts, "kind")
                .ok_or(OptionsError::Missing("kind"))?
                .trim();
            let version = get(opts, "version")
                .ok_or(OptionsError::Missing("version"))?
                .trim();
            let group = get(opts, "group").unwrap_or("").trim();
            let namespaced = match get(opts, "namespaced") {
                None => true,
                Some(v) => parse_bool("namespaced", v)?,
            };
            let r = Resource::new(group, version, kind, plural, namespaced)
                .map_err(OptionsError::Identity)?;
            // A kind spelled out in full still obeys the built-in read-only
            // policy, so writing to Pods cannot be unlocked by verbose DDL.
            (r, !resource::builtin_read_only(&r))
        } else {
            resource::builtin(plural).ok_or_else(|| OptionsError::Resource(plural.to_owned()))?
        };

        let writable = match get(opts, "writable") {
            None => default_writable,
            Some(v) => {
                let want = parse_bool("writable", v)?;
                if want && resource::builtin_read_only(&resource) {
                    return Err(OptionsError::ForcedWritable(resource.to_string()));
                }
                want
            }
        };

        let cache_mode = match get(opts, "cache_mode") {
            None => CacheMode::OnDemand,
            Some(v) => CacheMode::parse(v).ok_or_else(|| OptionsError::CacheMode(v.to_owned()))?,
        };
        Ok(Self {
            resource,
            cache_mode,
            writable,
        })
    }
}

/// Validates an option list for `catalog` the way `CREATE ...` DDL needs:
/// names must be known and unique, and values must parse. Wrapper and user
/// mapping accept no options in Phase 1 (user mappings arrive in Phase 7).
pub fn validate(catalog: Catalog, opts: &[(String, String)]) -> Result<(), OptionsError> {
    match catalog {
        Catalog::Server => ServerOptions::parse(opts).map(|_| ()),
        Catalog::Table => TableOptions::parse(opts).map(|_| ()),
        Catalog::Wrapper | Catalog::UserMapping => check_names(catalog, opts),
    }
}

/// Splits a `key=value` option string as delivered to an FDW validator.
/// Returns `None` if there is no `=`.
pub fn split_option(s: &str) -> Option<(String, String)> {
    let (k, v) = s.split_once('=')?;
    Some((k.to_owned(), v.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn server_options_happy_and_defaults() {
        let s = ServerOptions::parse(&o(&[
            ("endpoint", "https://gw:8443"),
            ("ca_cert", "/c.pem"),
        ]))
        .expect("valid");
        assert_eq!(s.target.endpoint, "https://gw:8443");
        assert_eq!(s.target.ca_cert_path.as_deref(), Some("/c.pem"));
        assert_eq!(s.rpc_timeout, DEFAULT_RPC_TIMEOUT);
        let s = ServerOptions::parse(&o(&[("endpoint", "https://gw"), ("rpc_timeout_secs", "7")]))
            .expect("valid");
        assert_eq!(s.rpc_timeout, Duration::from_secs(7));
    }

    #[test]
    fn server_options_errors() {
        assert_eq!(
            ServerOptions::parse(&o(&[])),
            Err(OptionsError::Missing("endpoint"))
        );
        assert_eq!(
            ServerOptions::parse(&o(&[("endpoint", "http://gw")])),
            Err(OptionsError::Endpoint(TargetError::Plaintext(
                "http".into()
            )))
        );
        assert_eq!(
            ServerOptions::parse(&o(&[("endpoint", "https://gw"), ("bogus", "1")])),
            Err(OptionsError::Unknown {
                catalog: Catalog::Server,
                name: "bogus".into()
            })
        );
        assert_eq!(
            ServerOptions::parse(&o(&[
                ("endpoint", "https://gw"),
                ("endpoint", "https://gw2")
            ])),
            Err(OptionsError::Duplicate("endpoint".into()))
        );
        for bad in ["0", "-1", "abc", ""] {
            assert_eq!(
                ServerOptions::parse(&o(&[("endpoint", "https://gw"), ("rpc_timeout_secs", bad)])),
                Err(OptionsError::Timeout(bad.into())),
                "{bad}"
            );
        }
    }

    #[test]
    fn table_options() {
        assert_eq!(
            TableOptions::parse(&o(&[("resource", "pods")])),
            Ok(TableOptions {
                resource: resource::builtin("pods").expect("built in").0,
                writable: false,
                cache_mode: CacheMode::OnDemand
            })
        );
        assert_eq!(
            TableOptions::parse(&o(&[])),
            Err(OptionsError::Missing("resource"))
        );
        assert_eq!(
            TableOptions::parse(&o(&[("resource", "deployments")])),
            Err(OptionsError::Resource("deployments".into()))
        );
        assert_eq!(
            TableOptions::parse(&o(&[("resource", "pods"), ("schema", "x")])),
            Err(OptionsError::Unknown {
                catalog: Catalog::Table,
                name: "schema".into()
            })
        );
    }

    #[test]
    fn table_cache_mode() {
        let t = TableOptions::parse(&o(&[("resource", "pods"), ("cache_mode", "watch")]))
            .expect("valid");
        assert_eq!(t.cache_mode, CacheMode::Watch);
        assert_eq!(
            TableOptions::parse(&o(&[("resource", "pods"), ("cache_mode", "live")])),
            Err(OptionsError::CacheMode("live".into()))
        );
        assert!(OptionsError::CacheMode("x".into())
            .to_string()
            .contains("on_demand, watch"));
    }

    #[test]
    fn validate_per_catalog() {
        assert_eq!(validate(Catalog::Wrapper, &o(&[])), Ok(()));
        assert_eq!(
            validate(Catalog::Wrapper, &o(&[("x", "1")])),
            Err(OptionsError::Unknown {
                catalog: Catalog::Wrapper,
                name: "x".into()
            })
        );
        assert_eq!(
            validate(Catalog::UserMapping, &o(&[("password", "x")])),
            Err(OptionsError::Unknown {
                catalog: Catalog::UserMapping,
                name: "password".into()
            })
        );
        assert_eq!(
            validate(Catalog::Server, &o(&[("endpoint", "https://gw")])),
            Ok(())
        );
        assert_eq!(
            validate(Catalog::Table, &o(&[("resource", "pods")])),
            Ok(())
        );
        assert!(validate(Catalog::Server, &o(&[])).is_err());
    }

    #[test]
    fn split_option_parses_key_value() {
        assert_eq!(
            split_option("endpoint=https://gw:1"),
            Some(("endpoint".into(), "https://gw:1".into()))
        );
        assert_eq!(split_option("a=b=c"), Some(("a".into(), "b=c".into())));
        assert_eq!(split_option("novalue"), None);
    }

    #[test]
    fn messages_are_actionable() {
        let m = OptionsError::Unknown {
            catalog: Catalog::Server,
            name: "foo".into(),
        }
        .to_string();
        assert!(m.contains("endpoint") && m.contains("foo"), "{m}");
        let m = OptionsError::Unknown {
            catalog: Catalog::UserMapping,
            name: "p".into(),
        }
        .to_string();
        assert!(m.contains("no options are accepted"), "{m}");
    }

    // --- the descriptor table is load-bearing, not prose ------------------
    //
    // `name` is consumed by check_names, so an option that is not listed is
    // rejected and one that is listed is accepted. `required` and `default`
    // are not consumed by anything -- the parsers hard-code both -- so without
    // these tests the generated reference could claim a default the parser
    // does not apply and `make docs-check` would still pass. That would make
    // the cannot-drift guarantee false exactly where a reader most relies on
    // it. These assert the descriptors against the parsers' real behaviour.

    /// Every option the parser rejects as missing must be marked required, and
    /// every option marked required must actually be rejected when absent.
    #[test]
    fn required_flags_match_the_parsers() {
        // Omitting each option in turn from an otherwise complete list.
        let full_server = [
            ("endpoint", "https://gw:8443"),
            ("ca_cert", "/ca.crt"),
            ("rpc_timeout_secs", "5"),
        ];
        for doc in SERVER_OPTION_DOCS {
            let without: Vec<(String, String)> = full_server
                .iter()
                .filter(|(k, _)| *k != doc.name)
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            let rejected = matches!(
                ServerOptions::parse(&without),
                Err(OptionsError::Missing(_))
            );
            assert_eq!(
                rejected, doc.required,
                "SERVER option {:?}: descriptor says required={}, parser says {}",
                doc.name, doc.required, rejected
            );
        }

        // `resource` alone is enough for a built-in kind, which is what makes
        // every other table option optional.
        let full_table = [("resource", "pods")];
        for doc in TABLE_OPTION_DOCS {
            let without: Vec<(String, String)> = full_table
                .iter()
                .filter(|(k, _)| *k != doc.name)
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            let rejected = matches!(TableOptions::parse(&without), Err(OptionsError::Missing(_)));
            assert_eq!(
                rejected, doc.required,
                "TABLE option {:?}: descriptor says required={}, parser says {}",
                doc.name, doc.required, rejected
            );
        }
    }

    /// The documented defaults must be the ones the parsers actually apply.
    ///
    /// Spelled out per option rather than derived, because a default is a
    /// value and the descriptor holds a human-readable rendering of it. The
    /// point is that changing `DEFAULT_RPC_TIMEOUT` without touching the table
    /// fails here, which is what makes the reference page trustworthy.
    #[test]
    fn documented_defaults_match_the_parsers() {
        let doc_default = |docs: &'static [OptionDoc], name: &str| -> Option<&'static str> {
            docs.iter().find(|d| d.name == name).and_then(|d| d.default)
        };

        let srv = ServerOptions::parse(&o(&[("endpoint", "https://gw:8443")])).unwrap();
        assert_eq!(srv.rpc_timeout, DEFAULT_RPC_TIMEOUT);
        assert_eq!(
            doc_default(SERVER_OPTION_DOCS, "rpc_timeout_secs"),
            Some(DEFAULT_RPC_TIMEOUT.as_secs().to_string().as_str()),
            "documented rpc_timeout_secs default does not match DEFAULT_RPC_TIMEOUT"
        );

        let tbl = TableOptions::parse(&o(&[("resource", "configmaps")])).unwrap();
        assert_eq!(tbl.cache_mode, CacheMode::default());
        assert_eq!(
            doc_default(TABLE_OPTION_DOCS, "cache_mode"),
            Some("on_demand"),
            "documented cache_mode default does not match CacheMode::default()"
        );
        assert!(
            CacheMode::parse("on_demand") == Some(CacheMode::default()),
            "the documented cache_mode default is not a value the parser accepts"
        );
        assert!(
            tbl.resource.namespaced,
            "documented `namespaced` default of true does not match the parser"
        );
    }

    /// Every documented name is accepted, and nothing else is.
    #[test]
    fn documented_names_are_exactly_the_accepted_names() {
        for (catalog, docs) in [
            (Catalog::Server, SERVER_OPTION_DOCS),
            (Catalog::Table, TABLE_OPTION_DOCS),
        ] {
            for doc in docs {
                assert!(
                    check_names(catalog, &o(&[(doc.name, "x")])).is_ok(),
                    "{catalog:?} option {:?} is documented but not accepted",
                    doc.name
                );
            }
            assert!(
                matches!(
                    check_names(catalog, &o(&[("definitely_not_an_option", "x")])),
                    Err(OptionsError::Unknown { .. })
                ),
                "{catalog:?} accepted an undocumented option"
            );
        }
    }
}
