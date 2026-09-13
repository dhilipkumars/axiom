//! Pure qual-pushdown logic: turns the equality quals the planner hands us
//! into the gateway `List` filters, deciding what is pushed down and what can
//! never match. Extraction from Postgres nodes lives in `fdw.rs`.

/// One `column = 'literal'` restriction on a text column, as extracted from
/// the plan. Anything else (other operators, non-constant RHS, other columns)
/// is not represented here and stays a local qual evaluated by Postgres.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Qual {
    /// Foreign-table column name.
    pub column: String,
    /// Literal compared against.
    pub value: String,
}

/// Filters for a scan, pushed to the gateway as `ListRequest` fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// `namespace = X`, if present.
    pub namespace: Option<String>,
    /// `name = Y`, if present.
    pub name: Option<String>,
    /// `true` when the quals can never match any object (e.g. two different
    /// literals for the same column, or a literal that is not a valid
    /// Kubernetes name). The scan returns zero rows without an RPC.
    pub impossible: bool,
}

/// Maximum length of a Kubernetes namespace or name (DNS-1123 subdomain).
const MAX_NAME_LEN: usize = 253;

/// Whether `s` could be a Kubernetes object name/namespace (DNS-1123
/// subdomain). Anything else cannot match, so it is not worth an RPC and,
/// more importantly, must not be turned into a gateway `INVALID_ARGUMENT`
/// error: `WHERE name = 'Foo'` is a legal query that matches nothing.
pub fn is_valid_k8s_name(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_NAME_LEN {
        return false;
    }
    s.split('.').all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

impl Filter {
    /// Derives the pushed-down filter from equality quals. Columns other than
    /// `namespace`/`name` are ignored here (Postgres still evaluates them).
    pub fn from_quals(quals: &[Qual]) -> Self {
        let mut f = Self::default();
        for q in quals {
            let slot = match q.column.as_str() {
                "namespace" => &mut f.namespace,
                "name" => &mut f.name,
                _ => continue,
            };
            if !is_valid_k8s_name(&q.value) {
                f.impossible = true;
            }
            match slot {
                Some(existing) if *existing != q.value => f.impossible = true,
                Some(_) => {}
                None => *slot = Some(q.value.clone()),
            }
        }
        f
    }

    /// Planner row estimate for this filter.
    ///
    /// Coarse on purpose, and deliberately not sized from the live cache. That
    /// was once noted here as future work and is the wrong thing to build:
    ///
    /// - it would read shared memory under a lock inside `GetForeignRelSize`,
    ///   on every plan of every query, to refine a number the planner only
    ///   needs to an order of magnitude;
    /// - a cold, empty or resyncing cache reports zero, and a zero estimate
    ///   pushes the planner into nested loops that are catastrophic when the
    ///   real count is large. The constants below are wrong by a constant
    ///   factor; zero is wrong by an unbounded one;
    /// - the cached count is what the *gateway* sees, which from Phase 7 on is
    ///   not the calling identity's permitted slice (docs/AUTH.md).
    ///
    /// A better estimate has to come from a real statistics source, per kind
    /// and per caller. Until one exists, these bounds encode the only thing
    /// actually known: a name match is one row, a namespace narrows a lot, and
    /// an unfiltered scan is the whole collection.
    pub fn estimated_rows(&self) -> f64 {
        match (
            self.impossible,
            self.name.is_some(),
            self.namespace.is_some(),
        ) {
            (true, _, _) => 0.0,
            (false, true, _) => 1.0,
            (false, false, true) => 100.0,
            (false, false, false) => 1000.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(c: &str, v: &str) -> Qual {
        Qual {
            column: c.into(),
            value: v.into(),
        }
    }

    #[test]
    fn empty_quals_means_list_everything() {
        let f = Filter::from_quals(&[]);
        assert_eq!(f, Filter::default());
        assert!(!f.impossible);
        assert!((f.estimated_rows() - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn namespace_and_name_are_pushed_down() {
        let f = Filter::from_quals(&[q("namespace", "payments"), q("name", "api-0")]);
        assert_eq!(f.namespace.as_deref(), Some("payments"));
        assert_eq!(f.name.as_deref(), Some("api-0"));
        assert!(!f.impossible);
        assert!((f.estimated_rows() - 1.0).abs() < f64::EPSILON);
        assert!(
            (Filter::from_quals(&[q("namespace", "x")]).estimated_rows() - 100.0).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn other_columns_are_left_to_postgres() {
        let f = Filter::from_quals(&[q("phase", "Running"), q("node", "n1")]);
        assert_eq!(f, Filter::default());
    }

    #[test]
    fn contradictions_are_impossible() {
        let f = Filter::from_quals(&[q("name", "a"), q("name", "b")]);
        assert!(f.impossible);
        assert!(f.estimated_rows().abs() < f64::EPSILON);
        let f = Filter::from_quals(&[q("name", "a"), q("name", "a")]);
        assert!(!f.impossible);
        assert_eq!(f.name.as_deref(), Some("a"));
    }

    #[test]
    fn invalid_names_are_impossible_not_errors() {
        for bad in [
            "",
            "Foo",
            "a_b",
            "-a",
            "a-",
            "a..b",
            "a/b",
            "a b",
            &"x".repeat(254),
        ] {
            assert!(Filter::from_quals(&[q("name", bad)]).impossible, "{bad:?}");
        }
        for good in ["a", "api-0", "kube-system", "a.b-c.d", &"x".repeat(253)] {
            assert!(
                !Filter::from_quals(&[q("namespace", good)]).impossible,
                "{good:?}"
            );
        }
    }
}
