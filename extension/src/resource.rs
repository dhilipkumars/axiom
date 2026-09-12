//! Runtime resource identity: which Kubernetes kind a foreign table maps to.
//!
//! Phases 1-3 had a closed `Kind` enum of two variants, which is why no CRD
//! could be served. Phase 4 replaces it with a value carrying the resolved
//! `(group, version, kind, plural)` and the kind's scope, filled in from table
//! options that `IMPORT FOREIGN SCHEMA` generates.
//!
//! The type stays `Copy` and plain-data, with inline fixed-capacity strings
//! rather than `String`s, for two reasons: it is stored verbatim in a
//! shared-memory subscription slot, which must be valid when zeroed and free of
//! pointers into process memory; and it is passed by value through the FDW
//! callbacks the way the enum was, so generalising the identity did not have to
//! become a lifetime refactor of every call site.

use std::fmt;

/// Maximum length of an API group, matching the DNS-1123 subdomain limit the
/// API server enforces on `CustomResourceDefinition` group names.
pub const GROUP_MAX: usize = 253;
/// Maximum length of a version, kind or plural name. Kubernetes keeps these
/// well inside a DNS-1035 label; 63 is that bound.
pub const NAME_MAX: usize = 63;

/// A fixed-capacity inline ASCII string.
///
/// Valid when zeroed (the empty string), `Copy`, and free of heap pointers, so
/// a struct containing one can be memcpy'd into shared memory. Content is
/// bounded at construction and never truncated: a value that does not fit is
/// rejected, because a truncated group or kind would silently address a
/// different resource.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct InlineStr<const N: usize> {
    len: u8,
    buf: [u8; N],
}

impl<const N: usize> Default for InlineStr<N> {
    fn default() -> Self {
        Self {
            len: 0,
            buf: [0; N],
        }
    }
}

/// Why a string could not be stored inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineError {
    /// Longer than the field's capacity.
    TooLong { what: &'static str, max: usize },
    /// Contains a byte outside the permitted set.
    BadChar { what: &'static str },
}

impl fmt::Display for InlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { what, max } => {
                write!(f, "{what} is longer than the {max} character limit")
            }
            Self::BadChar { what } => write!(
                f,
                "{what} contains a character that is not allowed in a Kubernetes name"
            ),
        }
    }
}

impl std::error::Error for InlineError {}

impl<const N: usize> InlineStr<N> {
    /// Stores `s`, rejecting anything longer than the capacity.
    ///
    /// `what` names the field for the error message. Callers that need
    /// character validation use [`InlineStr::parse_name`] instead.
    pub fn new(what: &'static str, s: &str) -> Result<Self, InlineError> {
        if s.len() > N {
            return Err(InlineError::TooLong { what, max: N });
        }
        let mut buf = [0u8; N];
        buf[..s.len()].copy_from_slice(s.as_bytes());
        // The length fits in a u8 because N is never above 255; the assertion
        // is a compile-time-ish guard on future callers choosing a bigger N.
        debug_assert!(u8::try_from(N).is_ok(), "InlineStr capacity must fit in u8");
        Ok(Self {
            len: u8::try_from(s.len()).map_err(|_| InlineError::TooLong { what, max: N })?,
            buf,
        })
    }

    /// Stores `s` after checking it holds only characters Kubernetes allows in
    /// a group, version, kind or resource name: ASCII alphanumerics, `-`, `_`
    /// and `.`.
    ///
    /// This is the boundary that keeps a table option from reaching the gateway
    /// as a path segment or selector fragment (docs/RULES.md §3). The gateway
    /// validates independently; neither side relies on the other.
    pub fn parse_name(what: &'static str, s: &str) -> Result<Self, InlineError> {
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Err(InlineError::BadChar { what });
        }
        Self::new(what, s)
    }

    /// The stored string.
    pub fn as_str(&self) -> &str {
        // SAFETY-adjacent: the buffer only ever receives bytes copied from a
        // `&str`, so the prefix is valid UTF-8. `from_utf8` is used rather than
        // the unchecked form so a corrupted shared-memory read degrades to the
        // empty string instead of undefined behaviour.
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }

    /// Whether the string is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> fmt::Debug for InlineStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> fmt::Display for InlineStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Kubernetes kind a foreign table maps to, fully resolved.
///
/// Equality and hashing cover every field, so two tables naming the same kind
/// at different versions are distinct cache subscriptions rather than one
/// subscription serving both.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Resource {
    /// API group; empty for the core group.
    pub group: InlineStr<GROUP_MAX>,
    /// API version, e.g. `v1`.
    pub version: InlineStr<NAME_MAX>,
    /// Kind, e.g. `Pod`. This is what the gateway resolves against.
    pub kind: InlineStr<NAME_MAX>,
    /// Plural resource name, e.g. `pods`. Used for messages and as the
    /// `resource` table option.
    pub plural: InlineStr<NAME_MAX>,
    /// False for cluster-scoped kinds, whose objects have no namespace.
    pub namespaced: bool,
}

impl fmt::Debug for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for Resource {
    /// Renders the kubectl spelling: `plural` for the core group, otherwise
    /// `plural.group`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.group.is_empty() {
            write!(f, "{}", self.plural)
        } else {
            write!(f, "{}.{}", self.plural, self.group)
        }
    }
}

impl Resource {
    /// Builds a resource from already-validated parts.
    pub fn new(
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
        namespaced: bool,
    ) -> Result<Self, InlineError> {
        Ok(Self {
            group: InlineStr::parse_name("option \"group\"", group)?,
            version: InlineStr::parse_name("option \"version\"", version)?,
            kind: InlineStr::parse_name("option \"kind\"", kind)?,
            plural: InlineStr::parse_name("option \"resource\"", plural)?,
            namespaced,
        })
    }

    /// `apiVersion` as Kubernetes spells it: `version` for the core group,
    /// otherwise `group/version`. Used when building write bodies.
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.to_string()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }
}

/// A built-in kind the extension knows without discovery, so the Phase 1-3
/// spellings (`OPTIONS (resource 'pods')`) keep working with no `group`,
/// `version` or `kind` option and no round-trip to the gateway.
struct Builtin {
    plural: &'static str,
    group: &'static str,
    version: &'static str,
    kind: &'static str,
    namespaced: bool,
    /// Whether SQL writes are offered. Pods are creatable and deletable
    /// through the API, but a Pod spec is largely immutable and "UPDATE a pod"
    /// has no sane SQL semantics, so the extension declines to expose it.
    writable: bool,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        plural: "pods",
        group: "",
        version: "v1",
        kind: "Pod",
        namespaced: true,
        writable: false,
    },
    Builtin {
        plural: "configmaps",
        group: "",
        version: "v1",
        kind: "ConfigMap",
        namespaced: true,
        writable: true,
    },
];

/// Resolves a bare `resource` option against the built-in table.
///
/// Returns `None` for anything not built in, which the caller then requires
/// explicit `group`/`version`/`kind` options for. This is what keeps a scan off
/// the discovery path: an unrecognised plural is a DDL error, never an RPC.
pub fn builtin(plural: &str) -> Option<(Resource, bool)> {
    let b = BUILTINS.iter().find(|b| b.plural == plural)?;
    let r = Resource::new(b.group, b.version, b.kind, b.plural, b.namespaced).ok()?;
    Some((r, b.writable))
}

/// Whether a kind is one the extension refuses to write to regardless of what
/// the API server supports. Keyed on identity, not on the `resource` option, so
/// a table that spells Pods out with explicit group/version/kind options is
/// still read-only.
pub fn builtin_read_only(r: &Resource) -> bool {
    BUILTINS
        .iter()
        .any(|b| !b.writable && b.group == r.group.as_str() && b.kind == r.kind.as_str())
}

/// The `resource` option values that need no other options, for error messages.
pub fn builtin_names() -> Vec<&'static str> {
    BUILTINS.iter().map(|b| b.plural).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::items_after_statements)]
    fn inline_str_roundtrips_and_bounds() {
        let s = InlineStr::<8>::new("x", "abc").expect("fits");
        assert_eq!(s.as_str(), "abc");
        assert!(!s.is_empty());
        assert!(InlineStr::<8>::new("x", "").expect("empty fits").is_empty());
        assert_eq!(
            InlineStr::<3>::new("x", "abc").expect("exact").as_str(),
            "abc"
        );
        assert!(matches!(
            InlineStr::<3>::new("x", "abcd"),
            Err(InlineError::TooLong { max: 3, .. })
        ));
    }

    #[test]
    fn inline_str_default_is_empty() {
        // The zeroed value must be a valid empty string: shared-memory slots
        // start life as zeroes.
        let s = InlineStr::<16>::default();
        assert!(s.is_empty());
        assert_eq!(s.as_str(), "");
    }

    #[test]
    fn parse_name_rejects_path_and_selector_characters() {
        for bad in ["a/b", "a b", "a,b", "a=b", "a\0b", "../x", "a\"b"] {
            assert!(
                matches!(
                    InlineStr::<64>::parse_name("x", bad),
                    Err(InlineError::BadChar { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
        for ok in [
            "pods",
            "example.com",
            "v1alpha1",
            "Widget",
            "with-dash",
            "with_underscore",
        ] {
            assert!(
                InlineStr::<64>::parse_name("x", ok).is_ok(),
                "{ok:?} should be accepted"
            );
        }
    }

    #[test]
    fn resource_renders_the_kubectl_spelling() {
        let core = Resource::new("", "v1", "Pod", "pods", true).expect("valid");
        assert_eq!(core.to_string(), "pods");
        assert_eq!(core.api_version(), "v1");

        let crd = Resource::new("example.com", "v1", "Widget", "widgets", true).expect("valid");
        assert_eq!(crd.to_string(), "widgets.example.com");
        assert_eq!(crd.api_version(), "example.com/v1");
    }

    #[test]
    fn resources_differing_only_by_version_are_distinct() {
        let a = Resource::new("example.com", "v1", "Widget", "widgets", true).expect("valid");
        let b = Resource::new("example.com", "v2", "Widget", "widgets", true).expect("valid");
        assert_ne!(
            a, b,
            "two versions of a kind must be separate subscriptions"
        );
    }

    #[test]
    fn builtins_resolve_without_discovery() {
        let (pods, writable) = builtin("pods").expect("pods is built in");
        assert_eq!(pods.kind.as_str(), "Pod");
        assert_eq!(pods.plural.as_str(), "pods");
        assert!(pods.namespaced);
        assert!(!writable, "pods stay read-only at the SQL layer");
        assert!(builtin_read_only(&pods));

        let (cm, writable) = builtin("configmaps").expect("configmaps is built in");
        assert_eq!(cm.kind.as_str(), "ConfigMap");
        assert!(writable);
        assert!(!builtin_read_only(&cm));

        assert!(builtin("widgets").is_none(), "a CRD needs explicit options");
    }

    #[test]
    fn read_only_policy_follows_identity_not_spelling() {
        // Spelling Pods out the long way must not become a way to write to them.
        let spelled_out = Resource::new("", "v1", "Pod", "pods", true).expect("valid");
        assert!(builtin_read_only(&spelled_out));
        let lookalike = Resource::new("example.com", "v1", "Pod", "pods", true).expect("valid");
        assert!(
            !builtin_read_only(&lookalike),
            "a CRD named Pod is not the core Pod"
        );
    }

    #[test]
    fn oversized_names_are_rejected_not_truncated() {
        let long = "a".repeat(GROUP_MAX + 1);
        assert!(
            Resource::new(&long, "v1", "Widget", "widgets", true).is_err(),
            "a truncated group would silently address a different resource"
        );
    }
}
