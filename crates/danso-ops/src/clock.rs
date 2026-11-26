//! Wall-clock and RFC 3339 helpers on top of `jiff` (issue #228).
//!
//! danso stores timestamps as RFC 3339 strings in its own state, marker and
//! manifest files. These helpers reproduce the exact strings the previous
//! `chrono` code wrote, so files written before and after the migration are
//! byte-for-byte comparable:
//!
//! - [`rfc3339`] is chrono's `to_rfc3339()` for a UTC instant: `+00:00`
//!   suffix, and the fraction is omitted, or 3, 6 or 9 digits — the shortest
//!   of those that is exact.
//! - [`rfc3339_opts`] is chrono's `to_rfc3339_opts(format, use_z)`.
//!
//! Parsing accepts every string these functions produce, and any RFC 3339
//! instant with an explicit offset.

pub use jiff::{SignedDuration, Timestamp};

/// The current instant.
pub fn now() -> Timestamp {
    Timestamp::now()
}

/// Fractional-second precision for [`rfc3339_opts`] (chrono `SecondsFormat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecondsFormat {
    Secs,
    Millis,
    Micros,
    Nanos,
    /// Shortest exact choice of none / 3 / 6 / 9 digits.
    AutoSi,
}

/// chrono `DateTime<Utc>::to_rfc3339()`.
pub fn rfc3339(ts: Timestamp) -> String {
    rfc3339_opts(ts, SecondsFormat::AutoSi, false)
}

/// chrono `DateTime<Utc>::to_rfc3339_opts(format, use_z)`.
pub fn rfc3339_opts(ts: Timestamp, format: SecondsFormat, use_z: bool) -> String {
    let dt = jiff::tz::Offset::UTC.to_datetime(ts);
    let nanos = dt.subsec_nanosecond();
    let fraction = match format {
        SecondsFormat::Secs => String::new(),
        SecondsFormat::Millis => format!(".{:03}", nanos / 1_000_000),
        SecondsFormat::Micros => format!(".{:06}", nanos / 1_000),
        SecondsFormat::Nanos => format!(".{nanos:09}"),
        SecondsFormat::AutoSi => {
            if nanos == 0 {
                String::new()
            } else if nanos % 1_000_000 == 0 {
                format!(".{:03}", nanos / 1_000_000)
            } else if nanos % 1_000 == 0 {
                format!(".{:06}", nanos / 1_000)
            } else {
                format!(".{nanos:09}")
            }
        }
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        fraction,
        if use_z { "Z" } else { "+00:00" }
    )
}

/// Method-style access, so call sites keep chrono's spelling.
pub trait Rfc3339 {
    fn to_rfc3339(&self) -> String;
    fn to_rfc3339_opts(&self, format: SecondsFormat, use_z: bool) -> String;
}

impl Rfc3339 for Timestamp {
    fn to_rfc3339(&self) -> String {
        rfc3339(*self)
    }
    fn to_rfc3339_opts(&self, format: SecondsFormat, use_z: bool) -> String {
        rfc3339_opts(*self, format, use_z)
    }
}

/// Parse an RFC 3339 instant (chrono `DateTime::parse_from_rfc3339`, then
/// converted to UTC). An offset is required: a bare local time is an error.
pub fn parse_rfc3339(text: &str) -> Result<Timestamp, jiff::Error> {
    text.parse::<Timestamp>()
}

/// Whole seconds since the Unix epoch (chrono `timestamp()`).
pub fn unix_seconds(ts: Timestamp) -> i64 {
    ts.as_second()
}

/// Milliseconds since the Unix epoch (chrono `timestamp_millis()`).
pub fn unix_millis(ts: Timestamp) -> i64 {
    ts.as_millisecond()
}

/// `SignedDuration` between two instants, `later - earlier` (chrono
/// `DateTime - DateTime`). Use this instead of `Timestamp - Timestamp`, which
/// yields a calendar `Span` in jiff.
pub fn since(later: Timestamp, earlier: Timestamp) -> SignedDuration {
    later.duration_since(earlier)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64, nanos: i32) -> Timestamp {
        Timestamp::new(secs, nanos).unwrap()
    }

    /// Golden strings: chrono 0.4.45 produced exactly these for the same
    /// instants (checked by a 20,000-instant differential test during #228).
    #[test]
    fn rfc3339_matches_recorded_chrono_output() {
        let cases: &[(i64, i32, &str)] = &[
            (0, 0, "1970-01-01T00:00:00+00:00"),
            (1_793_016_000, 0, "2026-10-26T12:00:00+00:00"),
            (1_793_016_000, 5_000_000, "2026-10-26T12:00:00.005+00:00"),
            (1_793_016_000, 120_000, "2026-10-26T12:00:00.000120+00:00"),
            (1_793_016_000, 7, "2026-10-26T12:00:00.000000007+00:00"),
            (
                1_793_016_000,
                999_999_999,
                "2026-10-26T12:00:00.999999999+00:00",
            ),
            (951_782_400, 0, "2000-02-29T00:00:00+00:00"),
            (-1, 500_000_000, "1969-12-31T23:59:59.500+00:00"),
        ];
        for &(secs, nanos, want) in cases {
            assert_eq!(rfc3339(ts(secs, nanos)), want, "{secs}.{nanos}");
            assert_eq!(parse_rfc3339(want).unwrap(), ts(secs, nanos), "{want}");
        }
    }

    #[test]
    fn rfc3339_opts_matches_recorded_chrono_output() {
        let t = ts(1_793_016_000, 123_456_789);
        assert_eq!(
            rfc3339_opts(t, SecondsFormat::Secs, true),
            "2026-10-26T12:00:00Z"
        );
        assert_eq!(
            rfc3339_opts(t, SecondsFormat::Millis, true),
            "2026-10-26T12:00:00.123Z"
        );
        assert_eq!(
            rfc3339_opts(t, SecondsFormat::Micros, true),
            "2026-10-26T12:00:00.123456Z"
        );
        assert_eq!(
            rfc3339_opts(t, SecondsFormat::Nanos, true),
            "2026-10-26T12:00:00.123456789Z"
        );
        assert_eq!(
            rfc3339_opts(ts(1_793_016_000, 0), SecondsFormat::Millis, true),
            "2026-10-26T12:00:00.000Z"
        );
        assert_eq!(
            rfc3339_opts(t, SecondsFormat::Secs, false),
            "2026-10-26T12:00:00+00:00"
        );
    }

    #[test]
    fn parse_accepts_offsets_and_z_and_rejects_naive() {
        let want = ts(1_793_016_000, 0);
        for text in [
            "2026-10-26T12:00:00Z",
            "2026-10-26T12:00:00z",
            "2026-10-26T21:00:00+09:00",
            "2026-10-26T12:00:00.000+00:00",
        ] {
            assert_eq!(parse_rfc3339(text).unwrap(), want, "{text}");
        }
        for text in ["2026-10-26T12:00:00", "2026-10-26", "not a time", ""] {
            assert!(parse_rfc3339(text).is_err(), "{text}");
        }
    }

    #[test]
    fn since_is_a_signed_duration() {
        let a = ts(100, 0);
        let b = ts(40, 500_000_000);
        assert_eq!(since(a, b), SignedDuration::new(59, 500_000_000));
        assert_eq!(since(b, a), -SignedDuration::new(59, 500_000_000));
        assert_eq!(unix_seconds(b), 40);
        assert_eq!(unix_millis(b), 40_500);
    }
}
