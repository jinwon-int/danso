//! ISO8601 helpers and the process-invariant boot id, matching the ccc
//! `agent_cron_lib.py` contract bit for bit (§6.5 file compatibility).
//!
//! `parse_utc` mirrors `parse_dt`: naive stamps are UTC, every result is
//! truncated to the minute. `parse_local` mirrors `parse_local_dt`: naive
//! stamps are anchored to the task timezone. Ambiguous wall times resolve to
//! the earliest instant (fold 0) and a wall time inside a forward gap uses
//! the pre-transition offset, which is what Python's `tzinfo` replace
//! semantics produce for `ZoneInfo`.

use danso_ops::clock::{SecondsFormat, Timestamp};
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};
use jiff::{RoundMode, TimestampRound, Unit};
use std::sync::OnceLock;

/// ccc `fmt_dt`: `YYYY-MM-DDTHH:MM:SSZ`, `None` passes through.
pub fn fmt_dt(dt: Option<Timestamp>) -> Option<String> {
    dt.map(|dt| danso_ops::clock::rfc3339_opts(dt, SecondsFormat::Secs, true))
}

/// Floor to the minute, matching Python's `.replace(second=0, microsecond=0)`.
pub fn truncate_minute(dt: Timestamp) -> Timestamp {
    dt.round(
        TimestampRound::new()
            .smallest(Unit::Minute)
            .mode(RoundMode::Floor),
    )
    .expect("flooring to a minute stays in range")
}

struct IsoStamp {
    naive: DateTime,
    offset_sec: Option<i32>,
}

/// `datetime.fromisoformat`-compatible subset: `YYYY-MM-DD`, an optional
/// `T`/space-separated `HH[:MM[:SS[.frac]]]`, and an optional `Z` or
/// `+HH[:MM[:SS]]` / `+HHMM` offset. Date-only means midnight.
fn parse_iso(value: &str) -> Result<IsoStamp, ()> {
    let text = value.trim();
    // Every accepted stamp is ASCII, and the slicing below is by byte
    // offset: a multibyte character straddling an offset must be an error,
    // not a char-boundary panic (`--at`, `lastRunAt`, a lock's `acquiredAt`
    // all reach here unchecked).
    if !text.is_ascii() {
        return Err(());
    }
    let bytes = text.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(());
    }
    let date = parse_date(&text[..10])?;
    let (time_part, offset_sec) = split_offset(text.get(10..).unwrap_or_default())?;
    let time = match time_part {
        Some(core) => parse_time(core)?,
        // Date-only means midnight.
        None => Time::midnight(),
    };
    Ok(IsoStamp {
        naive: date.to_datetime(time),
        offset_sec,
    })
}

/// `YYYY-MM-DD` with four, two and two ASCII digits and a real calendar date.
fn parse_date(text: &str) -> Result<Date, ()> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Result<i32, ()> {
        let part = &bytes[range];
        if part.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(part)
                .map_err(|_| ())?
                .parse()
                .map_err(|_| ())
        } else {
            Err(())
        }
    };
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    Date::new(year as i16, month as i8, day as i8).map_err(|_| ())
}

fn split_offset(rest: &str) -> Result<(Option<&str>, Option<i32>), ()> {
    let mut chars = rest.char_indices();
    match chars.next() {
        None => Ok((None, None)),
        Some((_, sep)) if sep == 'T' || sep == 't' || sep == ' ' => {
            let time_part = &rest[1..];
            if time_part.is_empty() {
                return Err(());
            }
            // An offset, when present, is the first `+`/`-`/`Z` in the time.
            let split = time_part
                .char_indices()
                .find(|(_, c)| *c == '+' || *c == '-' || *c == 'Z' || *c == 'z');
            match split {
                None => Ok((Some(time_part), None)),
                Some((idx, 'Z')) | Some((idx, 'z')) => {
                    if idx + 1 != time_part.len() {
                        return Err(());
                    }
                    Ok((Some(&time_part[..idx]), Some(0)))
                }
                Some((idx, sign)) => {
                    let (core, offset_text) = (&time_part[..idx], &time_part[idx..]);
                    let offset = parse_offset(offset_text, sign == '+')?;
                    Ok((Some(core), Some(offset)))
                }
            }
        }
        Some(_) => Err(()),
    }
}

/// `+HH`, `+HHMM`, or `+HH:MM` (sign already stripped). Python rejects an
/// offset of exactly ±24h, so hour 23 is the ceiling.
fn parse_offset(text: &str, positive: bool) -> Result<i32, ()> {
    let digits = &text[1..];
    let (hours, minutes) = match digits.len() {
        2 => (digits, "00"),
        4 => (&digits[..2], &digits[2..4]),
        5 => (&digits[..2], &digits[3..5]),
        _ => return Err(()),
    };
    if digits.len() == 5 && digits.as_bytes()[2] != b':' {
        return Err(());
    }
    let hours: i32 = hours.parse().map_err(|_| ())?;
    let minutes: i32 = minutes.parse().map_err(|_| ())?;
    if hours > 23 || minutes > 59 {
        return Err(());
    }
    let total = hours * 3600 + minutes * 60;
    Ok(if positive { total } else { -total })
}

fn parse_time(text: &str) -> Result<Time, ()> {
    let mut parts = text.split('.');
    let clock = parts.next().ok_or(())?;
    let frac = parts.next();
    if parts.next().is_some() {
        return Err(());
    }
    let numbers: Vec<&str> = clock.split(':').collect();
    if numbers.is_empty() || numbers.len() > 3 {
        return Err(());
    }
    let mut fields = [0i32; 3];
    for (index, number) in numbers.iter().enumerate() {
        if number.is_empty() || number.len() > 2 || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
        fields[index] = number.parse().map_err(|_| ())?;
    }
    let (hour, minute, second) = (fields[0], fields[1], fields[2]);
    if hour > 23 || minute > 59 || second > 59 {
        return Err(());
    }
    let nanos = match frac {
        // Python allows a fractional part only after the seconds component.
        None => 0u32,
        Some(digits) if numbers.len() == 3 => {
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(());
            }
            // Truncate toward zero beyond nanosecond precision, like Python.
            let padded = format!("{:0<9}", &digits[..digits.len().min(9)]);
            padded.parse().map_err(|_| ())?
        }
        Some(_) => return Err(()),
    };
    Time::new(hour as i8, minute as i8, second as i8, nanos as i32).map_err(|_| ())
}

/// ccc `parse_dt`: `None`/empty passes through, naive stamps are UTC, and the
/// result is truncated to the minute. Errors name the field and the raw value
/// (`{field} is not valid ISO8601: {value}`).
pub fn parse_utc(value: Option<&str>, field: &str) -> Result<Option<Timestamp>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    let raw = value.to_string();
    let stamp = parse_iso(value).map_err(|_| format!("{field} is not valid ISO8601: {raw}"))?;
    let dt = to_utc(stamp)?;
    Ok(Some(truncate_minute(dt)))
}

/// ccc `parse_local_dt`: naive stamps are anchored to `tz`.
pub fn parse_local(value: &str, tz: &TimeZone, field: &str) -> Result<Timestamp, String> {
    let raw = value.to_string();
    let stamp = parse_iso(value).map_err(|_| format!("{field} is not valid ISO8601: {raw}"))?;
    let dt = match stamp.offset_sec {
        // An explicit offset pins the wall time: the instant is
        // wall - offset (Python `astimezone(utc)` on an aware datetime).
        Some(sec) => Offset::from_seconds(sec)
            .and_then(|offset| offset.to_timestamp(stamp.naive))
            .map_err(|_| format!("{field} is not valid ISO8601: {raw}"))?,
        None => local_to_utc(tz, stamp.naive)
            .ok_or_else(|| format!("{field} is not valid ISO8601: {raw}"))?,
    };
    Ok(truncate_minute(dt))
}

fn to_utc(stamp: IsoStamp) -> Result<Timestamp, String> {
    let offset = match stamp.offset_sec {
        Some(sec) => Offset::from_seconds(sec).map_err(|_| "invalid UTC offset".to_string())?,
        None => Offset::UTC,
    };
    offset
        .to_timestamp(stamp.naive)
        .map_err(|_| "invalid UTC offset".to_string())
}
/// Python `naive.replace(tzinfo=tz).astimezone(utc)` for a `ZoneInfo`: the
/// earliest instant for an ambiguous wall time (fold 0), and the
/// pre-transition offset for a wall time inside a forward gap. In a fold the
/// pre-transition offset is also the one that gives the earliest instant, so
/// both cases use `before`. `None` only outside the supported instant range.
pub fn local_to_utc(tz: &TimeZone, naive: DateTime) -> Option<Timestamp> {
    let offset = match tz.to_ambiguous_timestamp(naive).offset() {
        AmbiguousOffset::Unambiguous { offset } => offset,
        AmbiguousOffset::Fold { before, .. } | AmbiguousOffset::Gap { before, .. } => before,
    };
    offset.to_timestamp(naive).ok()
}

static BOOT_ID: OnceLock<String> = OnceLock::new();

/// Process-invariant boot id for lock staleness; empty when unreadable, which
/// disables boot-id staleness exactly like the Python reference. One reader
/// per binary: this caches `danso_ops::probe::boot_id`, the same source
/// `danso service` scopes its pid records to.
pub fn boot_id() -> &'static str {
    BOOT_ID.get_or_init(|| danso_ops::probe::boot_id().unwrap_or_default())
}
