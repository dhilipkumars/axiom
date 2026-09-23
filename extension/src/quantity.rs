//! Kubernetes quantity strings as Postgres `numeric`.
//!
//! Kubernetes reports every measured value as a string with a unit suffix --
//! `49903n` of CPU, `14488Ki` of memory, `100m` of a core. Postgres cannot
//! compare or sum those, so without a conversion `WHERE cpu > '1'` is a string
//! comparison that silently answers wrongly, and `sum()` is unavailable.
//!
//! Conversion is exact rather than floating point: `100m` becomes `0.1`, not
//! `0.1000000000000000055`. The parser works in integer digits and a decimal
//! exponent, and renders a decimal string that `numeric` parses exactly.
//!
//! There is deliberately one function rather than a CPU one and a memory one.
//! A quantity carries no unit of measure -- only a scale -- so the same parse
//! serves both; what the number means is decided by the field it came from.
//! `axiom_quantity(usage->>'cpu')` is cores, `axiom_quantity(usage->>'memory')`
//! is bytes, because that is what Kubernetes puts in those fields.

use pgrx::prelude::*;
use std::str::FromStr;

/// Decimal (SI) suffixes, as a power of ten.
fn decimal_exponent(suffix: &str) -> Option<i32> {
    Some(match suffix {
        "n" => -9,
        "u" => -6,
        "m" => -3,
        "" => 0,
        "k" => 3,
        "M" => 6,
        "G" => 9,
        "T" => 12,
        "P" => 15,
        "E" => 18,
        _ => return None,
    })
}

/// Binary suffixes, as a power of 1024.
fn binary_power(suffix: &str) -> Option<u32> {
    Some(match suffix {
        "Ki" => 1,
        "Mi" => 2,
        "Gi" => 3,
        "Ti" => 4,
        "Pi" => 5,
        "Ei" => 6,
        _ => return None,
    })
}

/// Splits `123.45Ki` into its mantissa digits (`12345`), the decimal exponent
/// implied by the fraction (`-2`), and the suffix (`Ki`). Returns `None` for
/// anything that is not a quantity, including the empty string.
fn split(q: &str) -> Option<(i128, i32, &str, bool)> {
    let q = q.trim();
    if q.is_empty() {
        return None;
    }
    let (negative, rest) = match q.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, q.strip_prefix('+').unwrap_or(q)),
    };
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let (number, suffix) = rest.split_at(end);
    if number.is_empty() {
        return None;
    }
    let (int_part, frac_part) = match number.split_once('.') {
        Some((i, f)) => {
            if f.contains('.') {
                return None;
            }
            (i, f)
        }
        None => (number, ""),
    };
    let digits: i128 = format!("{int_part}{frac_part}").parse().ok()?;
    let exponent = -i32::try_from(frac_part.len()).ok()?;
    Some((digits, exponent, suffix, negative))
}

/// Renders `digits * 10^exponent` as a decimal string `numeric` parses exactly.
fn render(digits: i128, exponent: i32, negative: bool) -> String {
    let sign = if negative && digits != 0 { "-" } else { "" };
    if exponent >= 0 {
        let zeros = "0".repeat(usize::try_from(exponent).unwrap_or(0));
        return format!("{sign}{digits}{zeros}");
    }
    let shift = usize::try_from(-exponent).unwrap_or(0);
    let text = digits.to_string();
    if text.len() > shift {
        let (whole, frac) = text.split_at(text.len() - shift);
        format!("{sign}{whole}.{frac}")
    } else {
        let pad = "0".repeat(shift - text.len());
        format!("{sign}0.{pad}{text}")
    }
}

/// Parses a Kubernetes quantity into an exact decimal string.
///
/// Returns `None` when the input is not a quantity -- an unknown suffix, an
/// empty string, or a value too large for the intermediate representation --
/// so a malformed field yields SQL `NULL` rather than an error that would take
/// out a query over a whole cluster for one bad row.
pub fn parse(q: &str) -> Option<String> {
    let (digits, frac_exponent, suffix, negative) = split(q)?;
    if let Some(power) = binary_power(suffix) {
        let factor = 1024_i128.checked_pow(power)?;
        let scaled = digits.checked_mul(factor)?;
        return Some(render(scaled, frac_exponent, negative));
    }
    // A bare exponent form, `1e3`, is legal in the Kubernetes grammar.
    if let Some(exp) = suffix.strip_prefix(['e', 'E']) {
        let exp: i32 = exp.parse().ok()?;
        return Some(render(digits, frac_exponent.checked_add(exp)?, negative));
    }
    let exponent = decimal_exponent(suffix)?;
    Some(render(
        digits,
        frac_exponent.checked_add(exponent)?,
        negative,
    ))
}

/// Converts a Kubernetes quantity string to `numeric`.
///
/// `axiom_quantity('100m')` is `0.1`, `axiom_quantity('128Mi')` is `134217728`.
/// Returns `NULL` for `NULL` input and for anything that is not a quantity.
#[pg_extern(immutable, parallel_safe)]
fn axiom_quantity(quantity: Option<&str>) -> Option<AnyNumeric> {
    let text = parse(quantity?)?;
    AnyNumeric::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn p(q: &str) -> Option<String> {
        parse(q)
    }

    #[test]
    fn cpu_suffixes_are_exact_not_floating_point() {
        assert_eq!(p("100m").as_deref(), Some("0.100"));
        assert_eq!(p("1").as_deref(), Some("1"));
        assert_eq!(p("49903n").as_deref(), Some("0.000049903"));
        assert_eq!(p("1500u").as_deref(), Some("0.001500"));
    }

    #[test]
    fn binary_suffixes_are_powers_of_1024_not_1000() {
        assert_eq!(p("1Ki").as_deref(), Some("1024"));
        assert_eq!(p("14488Ki").as_deref(), Some("14835712"));
        assert_eq!(p("128Mi").as_deref(), Some("134217728"));
        assert_eq!(p("1Gi").as_deref(), Some("1073741824"));
    }

    #[test]
    fn decimal_suffixes_are_powers_of_1000() {
        assert_eq!(p("1k").as_deref(), Some("1000"));
        assert_eq!(p("1M").as_deref(), Some("1000000"));
        // The distinction that matters: M is not Mi.
        assert_ne!(p("1M"), p("1Mi"));
    }

    #[test]
    fn fractions_survive_a_suffix() {
        assert_eq!(p("1.5Gi").as_deref(), Some("1610612736.0"));
        assert_eq!(p("0.5").as_deref(), Some("0.5"));
        assert_eq!(p("2.5m").as_deref(), Some("0.0025"));
    }

    #[test]
    fn exponent_form_is_accepted() {
        assert_eq!(p("1e3").as_deref(), Some("1000"));
        assert_eq!(p("1.5e3").as_deref(), Some("1500"));
        assert_eq!(p("1e-3").as_deref(), Some("0.001"));
    }

    #[test]
    fn zero_and_negatives() {
        assert_eq!(p("0").as_deref(), Some("0"));
        assert_eq!(p("0Ki").as_deref(), Some("0"));
        assert_eq!(p("-1Ki").as_deref(), Some("-1024"));
    }

    #[test]
    fn malformed_input_is_none_rather_than_an_error() {
        // A query across a cluster must not fail because one field is odd.
        assert_eq!(p(""), None);
        assert_eq!(p("   "), None);
        assert_eq!(p("Ki"), None);
        assert_eq!(p("12Xi"), None);
        assert_eq!(p("1.2.3"), None);
        assert_eq!(p("abc"), None);
        assert_eq!(p("12Ki extra"), None);
    }
}
