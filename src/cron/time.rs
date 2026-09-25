//! ISO8601 helpers and the process-invariant boot id, matching the ccc
//! `agent_cron_lib.py` contract bit for bit (§6.5 file compatibility).
//!
//! `parse_utc` mirrors `parse_dt`: naive stamps are UTC, every result is
//! truncated to the minute. `parse_local` mirrors `parse_local_dt`: naive
//! stamps are anchored to the task timezone. Ambiguous wall times resolve to
//! the earliest instant (fold 0) and a wall time inside a forward gap uses
//! the pre-transition offset, which is what Python's `tzinfo` replace
//! semantics produce for `ZoneInfo`.

use chrono::{
    DateTime, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, Offset,
    TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;
use std::sync::OnceLock;

/// ccc `fmt_dt`: `YYYY-MM-DDTHH:MM:SSZ`, `None` passes through.
pub fn fmt_dt(dt: Option<DateTime<Utc>>) -> Option<String> {
    dt.map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Floor to the minute, matching Python's `.replace(second=0, microsecond=0)`.
pub fn truncate_minute(dt: DateTime<Utc>) -> DateTime<Utc> {
    dt - Duration::seconds(dt.second() as i64)
        - Duration::nanoseconds(dt.timestamp_subsec_nanos() as i64)
}

struct IsoStamp {
    naive: NaiveDateTime,
    offset_sec: Option<i32>,
}

/// `datetime.fromisoformat`-compatible subset: `YYYY-MM-DD`, an optional
/// `T`/space-separated `HH[:MM[:SS[.frac]]]`, and an optional `Z` or
/// `+HH[:MM[:SS]]` / `+HHMM` offset. Date-only means midnight.
fn parse_iso(value: &str) -> Result<IsoStamp, ()> {
    let text = value.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(());
    }
    let date = NaiveDate::parse_from_str(&text[..10], "%Y-%m-%d").map_err(|_| ())?;
    let (time_part, offset_sec) = split_offset(text.get(10..).unwrap_or_default())?;
    let time = match time_part {
        Some(core) => parse_time(core)?,
        // Date-only means midnight.
        None => NaiveTime::MIN,
    };
    Ok(IsoStamp {
        naive: date.and_time(time),
        offset_sec,
    })
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

fn parse_time(text: &str) -> Result<NaiveTime, ()> {
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
    NaiveTime::from_hms_nano_opt(hour as u32, minute as u32, second as u32, nanos).ok_or(())
}

/// ccc `parse_dt`: `None`/empty passes through, naive stamps are UTC, and the
/// result is truncated to the minute. Errors name the field and the raw value
/// (`{field} is not valid ISO8601: {value}`).
pub fn parse_utc(value: Option<&str>, field: &str) -> Result<Option<DateTime<Utc>>, String> {
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
pub fn parse_local(value: &str, tz: Tz, field: &str) -> Result<DateTime<Utc>, String> {
    let raw = value.to_string();
    let stamp = parse_iso(value).map_err(|_| format!("{field} is not valid ISO8601: {raw}"))?;
    let dt = match stamp.offset_sec {
        // An explicit offset pins the wall time: the instant is
        // wall - offset (Python `astimezone(utc)` on an aware datetime).
        Some(sec) => FixedOffset::east_opt(sec)
            .ok_or_else(|| format!("{field} is not valid ISO8601: {raw}"))?
            .from_local_datetime(&stamp.naive)
            .earliest()
            .ok_or_else(|| format!("{field} is not valid ISO8601: {raw}"))?
            .with_timezone(&Utc),
        None => local_to_utc(tz, stamp.naive),
    };
    Ok(truncate_minute(dt))
}

fn to_utc(stamp: IsoStamp) -> Result<DateTime<Utc>, String> {
    match stamp.offset_sec {
        Some(sec) => {
            let offset =
                FixedOffset::east_opt(sec).ok_or_else(|| "invalid UTC offset".to_string())?;
            Ok(offset
                .from_local_datetime(&stamp.naive)
                .earliest()
                .ok_or_else(|| "invalid UTC offset".to_string())?
                .with_timezone(&Utc))
        }
        None => Ok(stamp.naive.and_utc()),
    }
}
/// Python `naive.replace(tzinfo=tz).astimezone(utc)` for a `ZoneInfo`: the
/// earliest instant for an ambiguous wall time (fold 0), and the
/// pre-transition offset for a wall time inside a forward gap. tzdata never
/// places two transitions within 48 hours, so the offset sampled 48 hours
/// earlier is the pre-transition one.
pub fn local_to_utc(tz: Tz, naive: NaiveDateTime) -> DateTime<Utc> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt.with_timezone(&Utc),
        LocalResult::Ambiguous(earliest, _) => earliest.with_timezone(&Utc),
        LocalResult::None => {
            let before = tz.offset_from_utc_datetime(&(naive - Duration::hours(48)));
            (naive - before.fix()).and_utc()
        }
    }
}

static BOOT_ID: OnceLock<String> = OnceLock::new();

/// Process-invariant boot id for lock staleness; empty when unreadable, which
/// disables boot-id staleness exactly like the Python reference.
pub fn boot_id() -> &'static str {
    BOOT_ID.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|text| text.trim().to_string())
            .unwrap_or_default()
    })
}
