//! Pure parsing/validation of `CREATE SERVER` / `CREATE FOREIGN TABLE` options.
//!
//! The FDW validator and scan callbacks hand raw `(key, value)` pairs in here;
//! nothing in this module touches Postgres.

use std::fmt;
use std::time::Duration;

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

/// Kinds the extension can expose as foreign tables. Phase 1: Pods only.
/// TODO(phase4): replace with schema-discovered kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// core/v1 Pod.
    Pods,
}

impl Resource {
    /// Parses the `resource` table option.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "pods" => Some(Self::Pods),
            _ => None,
        }
    }
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
    /// Which kind this table maps to (`resource`).
    pub resource: Resource,
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
    /// `resource` names a kind we do not serve.
    Resource(String),
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
                "option \"resource\" {v:?} is not supported; valid values: pods"
            ),
        }
    }
}

impl std::error::Error for OptionsError {}

const SERVER_OPTIONS: &[&str] = &["endpoint", "ca_cert", "rpc_timeout_secs"];
const TABLE_OPTIONS: &[&str] = &["resource"];
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

fn allowed(catalog: Catalog) -> String {
    let names: &[&str] = match catalog {
        Catalog::Server => SERVER_OPTIONS,
        Catalog::Table => TABLE_OPTIONS,
        Catalog::Wrapper | Catalog::UserMapping => &[],
    };
    if names.is_empty() {
        "no options are accepted".to_owned()
    } else {
        format!("valid options are {}", names.join(", "))
    }
}

/// Checks that every option name is allowed for `catalog` and none repeats.
fn check_names(catalog: Catalog, opts: &[(String, String)]) -> Result<(), OptionsError> {
    let names: &[&str] = match catalog {
        Catalog::Server => SERVER_OPTIONS,
        Catalog::Table => TABLE_OPTIONS,
        Catalog::Wrapper | Catalog::UserMapping => &[],
    };
    for (i, (k, _)) in opts.iter().enumerate() {
        if !names.contains(&k.as_str()) {
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

impl TableOptions {
    /// Parses foreign-table options. `resource` is required.
    pub fn parse(opts: &[(String, String)]) -> Result<Self, OptionsError> {
        check_names(Catalog::Table, opts)?;
        let raw = get(opts, "resource").ok_or(OptionsError::Missing("resource"))?;
        let resource =
            Resource::parse(raw).ok_or_else(|| OptionsError::Resource(raw.to_owned()))?;
        Ok(Self { resource })
    }
}

/// Validates an option list for `catalog` the way `CREATE ...` DDL needs:
/// names must be known and unique, and values must parse. Wrapper and user
/// mapping accept no options in Phase 1 (user mappings arrive in Phase 6).
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
                resource: Resource::Pods
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
}
