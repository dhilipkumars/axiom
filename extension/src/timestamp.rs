//! Kubernetes timestamps as Postgres `timestamptz` values, both ways.
//!
//! Kubernetes writes timestamps as RFC 3339 strings: `metav1.Time` to the
//! second (`2024-05-01T10:00:00Z`), `metav1.MicroTime` to the microsecond, and
//! CRD `date-time` fields with whatever offset the writer chose. Postgres
//! stores a `timestamptz` as microseconds since 2000-01-01 00:00:00 UTC, which
//! is what [`parse`] returns and [`format`] takes, so the datum needs no
//! further conversion and no Postgres call -- which keeps this pure and tested
//! without a server.
//!
//! Parsing is strict. A string that is not RFC 3339 is `None`, and the column
//! reads as NULL, rather than being guessed at the way Postgres's own input
//! function would accept `yesterday` or `May 1`.

/// Seconds from the Unix epoch to the Postgres epoch, 2000-01-01 UTC.
const POSTGRES_EPOCH_UNIX_SECS: i64 = 946_684_800;
const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Parses an RFC 3339 timestamp to microseconds since the Postgres epoch.
///
/// Digits past the sixth of a fraction are truncated, since `timestamptz`
/// holds microseconds. A leap second (`:60`) is rejected, as Go's
/// `time.Parse` rejects it, so nothing Kubernetes wrote can read differently
/// here from what it meant.
#[must_use]
pub fn parse(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let year = digits(b, 0, 4)?;
    let month = digits(b, 5, 2)?;
    let day = digits(b, 8, 2)?;
    let hour = digits(b, 11, 2)?;
    let minute = digits(b, 14, 2)?;
    let second = digits(b, 17, 2)?;
    if b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut i = 19;
    let mut micros = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let frac = &b[start..i];
        if frac.is_empty() {
            return None;
        }
        for pos in 0..6 {
            micros = micros * 10 + frac.get(pos).map_or(0, |d| i64::from(d - b'0'));
        }
    }

    let offset_secs = match b.get(i..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let oh = digits(&[*h1, *h2], 0, 2)?;
            let om = digits(&[*m1, *m2], 0, 2)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let secs = oh * 3600 + om * 60;
            if *sign == b'-' {
                -secs
            } else {
                secs
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let unix_secs = days * SECS_PER_DAY + hour * 3600 + minute * 60 + second - offset_secs;
    Some((unix_secs - POSTGRES_EPOCH_UNIX_SECS) * MICROS_PER_SEC + micros)
}

/// Formats microseconds since the Postgres epoch as RFC 3339 in UTC, always
/// with six fractional digits.
///
/// Six digits because `metav1.MicroTime` only parses with exactly six, while
/// `metav1.Time` and CRD `date-time` fields accept any fraction; one spelling
/// is then valid for all three. `None` outside years 1 to 9999, which RFC 3339
/// cannot express and which Postgres's `infinity` falls outside of.
#[must_use]
pub fn format(pg_micros: i64) -> Option<String> {
    let unix_micros =
        pg_micros.checked_add(POSTGRES_EPOCH_UNIX_SECS.checked_mul(MICROS_PER_SEC)?)?;
    let secs = unix_micros.div_euclid(MICROS_PER_SEC);
    let micros = unix_micros.rem_euclid(MICROS_PER_SEC);
    let days = secs.div_euclid(SECS_PER_DAY);
    let tod = secs.rem_euclid(SECS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    ))
}

/// Reads `n` ASCII digits of `b` from `at` as a number.
fn digits(b: &[u8], at: usize, n: usize) -> Option<i64> {
    b.get(at..at + n)?.iter().try_fold(0, |acc, d| {
        d.is_ascii_digit().then(|| acc * 10 + i64::from(d - b'0'))
    })
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_postgres_epoch_is_zero() {
        assert_eq!(parse("2000-01-01T00:00:00Z"), Some(0));
        assert_eq!(format(0).as_deref(), Some("2000-01-01T00:00:00.000000Z"));
    }

    #[test]
    fn parses_the_spellings_kubernetes_writes() {
        // Microseconds since 2000-01-01 UTC, computed independently with Python's
        // datetime.
        let cases = [
            // metav1.Time
            ("2024-05-01T10:00:00Z", 767_872_800_000_000),
            // metav1.MicroTime
            ("2024-05-01T10:00:00.123456Z", 767_872_800_123_456),
            // A CRD date-time with an offset: the same instant.
            ("2024-05-01T15:30:00+05:30", 767_872_800_000_000),
            ("2024-05-01T05:00:00-05:00", 767_872_800_000_000),
            // Before the Postgres epoch.
            ("1999-12-31T23:59:59Z", -1_000_000),
            // Leap day.
            ("2024-02-29T00:00:00Z", 762_480_000_000_000),
        ];
        for (s, want) in cases {
            assert_eq!(parse(s), Some(want), "{s}");
        }
    }

    #[test]
    fn fractions_are_padded_or_truncated_to_microseconds() {
        assert_eq!(parse("2000-01-01T00:00:00.5Z"), Some(500_000));
        assert_eq!(parse("2000-01-01T00:00:00.123456789Z"), Some(123_456));
    }

    #[test]
    fn rejects_what_is_not_rfc_3339() {
        for s in [
            "",
            "2024-05-01",
            "2024-05-01T10:00:00",
            "2024-05-01 10:00:00Z",
            "2024-13-01T10:00:00Z",
            "2023-02-29T10:00:00Z",
            "2024-04-31T10:00:00Z",
            "2024-05-01T24:00:00Z",
            "2024-05-01T10:60:00Z",
            "2024-05-01T10:00:60Z",
            "2024-05-01T10:00:00.Z",
            "2024-05-01T10:00:00+0530",
            "2024-05-01T10:00:00+24:00",
            "2024-05-01T10:00:00Zjunk",
            "yesterday",
            "２０２４-05-01T10:00:00Z",
        ] {
            assert_eq!(parse(s), None, "{s:?}");
        }
    }

    #[test]
    fn format_round_trips_through_parse() {
        for us in [
            0,
            1,
            -1,
            767_872_800_123_456,
            -946_684_800_000_000,
            // 9999-12-31T23:59:59.999999Z
            252_455_615_999_999_999,
        ] {
            let s = format(us).expect("in range");
            assert_eq!(parse(&s), Some(us), "{s}");
        }
    }

    #[test]
    fn format_refuses_what_rfc_3339_cannot_say() {
        // Postgres's infinity and -infinity.
        assert_eq!(format(i64::MAX), None);
        assert_eq!(format(i64::MIN), None);
        // 10000-01-01.
        assert_eq!(format(252_455_616_000_000_000), None);
    }
}
