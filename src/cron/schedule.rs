//! Pure schedule parsing and occurrence scanning — a line-for-line port of
//! ccc `agent_cron_lib.py` (§6.5 file compatibility), including the DST-safe
//! jump-scan with per-hop verification.
//!
//! Supported forms (fail-closed on everything else):
//! - 5-field cron / `@shorthand`, matched in the task timezone
//! - `every <N>m|h|d` (60s..366d), phase-anchored by `anchorAt`
//! - `at <ISO8601>` / bare ISO8601, one-shot; naive stamps anchor to the task
//!   timezone; every result is truncated to the minute

use crate::cron::time::{parse_local, truncate_minute};
use danso_ops::clock::{SignedDuration, Timestamp};
use jiff::civil::{DateTime, Time};
use jiff::tz::{AmbiguousOffset, TimeZone};
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

/// Hard cap on scanned occurrences per plan (`OCCURRENCE_SCAN_LIMIT`).
pub const OCCURRENCE_SCAN_LIMIT: usize = 1000;

const MAX_INTERVAL_SECONDS: i64 = 366 * 86400;

const SHORTHANDS: [(&str, &str); 6] = [
    ("@hourly", "0 * * * *"),
    ("@daily", "0 0 * * *"),
    ("@weekly", "0 0 * * 0"),
    ("@monthly", "0 0 1 * *"),
    ("@yearly", "0 0 1 1 *"),
    ("@annually", "0 0 1 1 *"),
];

fn field_regex() -> &'static Regex {
    static FIELD_RX: OnceLock<Regex> = OnceLock::new();
    FIELD_RX.get_or_init(|| {
        // One comma-separated term: `*`, `N`, `N-M`, or any of those with a
        // `/S` step. Ranges (`1-5`, `9-17/2`) are standard cron; alphabetic
        // names (`MON`, `JAN`) are not supported.
        let term = r"(?:\*|[0-9]+(?:-[0-9]+)?)(?:/[1-9][0-9]*)?";
        Regex::new(&format!("^{term}(?:,{term})*$")).expect("valid cron field regex")
    })
}

fn interval_regex() -> &'static Regex {
    static INTERVAL_RX: OnceLock<Regex> = OnceLock::new();
    INTERVAL_RX.get_or_init(|| {
        Regex::new(r"^every\s+([1-9][0-9]*)\s*(m|h|d)$").expect("valid interval regex")
    })
}

/// Resolve a task timezone name; unknown names fail closed with the name in
/// the error (operator decision: the tzdb embedded in the binary — jiff's
/// bundled copy, never system tzdata — and never a silent UTC fallback).
/// Names match exactly, as with the previous `chrono-tz` lookup and Python's
/// `ZoneInfo`: jiff's own lookup is case-insensitive, so a differently cased
/// name is refused here rather than silently accepted.
pub fn resolve_timezone(name: &str) -> Result<TimeZone, String> {
    let label = name.trim();
    let label = if label.is_empty() { "UTC" } else { label };
    if label.eq_ignore_ascii_case("UTC") {
        return Ok(TimeZone::UTC);
    }
    match jiff::tz::db().get(label) {
        Ok(tz) if tz.iana_name() == Some(label) => Ok(tz),
        _ => Err(format!("unknown timezone: {label}")),
    }
}

/// One cron field expanded to its value set (`*`, `N`, `N-M`, comma lists,
/// `/S` step on any of those; `N/S` counts from N to the field maximum, the
/// way Vixie cron reads it).
fn expand_field(raw: &str, min_v: i32, max_v: i32) -> Result<HashSet<i32>, String> {
    let mut vals = HashSet::new();
    for part in raw.split(',') {
        let (body, step_raw) = match part.split_once('/') {
            Some((body, step)) => (body, Some(step)),
            None => (part, None),
        };
        let mut step: i32 = 1;
        if let Some(step_raw) = step_raw {
            step = step_raw
                .parse()
                .map_err(|_| "step must be positive".to_string())?;
            if step <= 0 {
                return Err("step must be positive".to_string());
            }
        }
        let (low, high) = if body == "*" {
            (min_v, max_v)
        } else if let Some((low_raw, high_raw)) = body.split_once('-') {
            let low: i32 = low_raw
                .parse()
                .map_err(|_| format!("value {body} outside {min_v}-{max_v}"))?;
            let high: i32 = high_raw
                .parse()
                .map_err(|_| format!("value {body} outside {min_v}-{max_v}"))?;
            if low > high {
                return Err(format!("range {body} is inverted"));
            }
            (low, high)
        } else {
            let low: i32 = body
                .parse()
                .map_err(|_| format!("value {body} outside {min_v}-{max_v}"))?;
            // `N/S` counts up from N to the field maximum; a bare `N` is a
            // single value.
            (low, if step_raw.is_some() { max_v } else { low })
        };
        if low < min_v || high > max_v {
            return Err(format!("value {body} outside {min_v}-{max_v}"));
        }
        vals.extend((low..=high).step_by(step.max(1) as usize));
    }
    Ok(vals)
}

/// A parsed cron expression (all five fields expanded) plus the task
/// timezone it is matched in.
#[derive(Debug, Clone)]
pub struct CronFields {
    pub minute: HashSet<i32>,
    pub hour: HashSet<i32>,
    pub dom: HashSet<i32>,
    pub month: HashSet<i32>,
    pub dow: HashSet<i32>,
    pub dom_any: bool,
    pub dow_any: bool,
}

impl CronFields {
    /// Cron day semantics: with both day-of-month and day-of-week
    /// restricted, either may match; otherwise the restricted field
    /// (if any) controls. DOW 0/7 are both Sunday.
    fn day_matches(&self, local: DateTime) -> bool {
        let dow = i32::from(local.weekday().to_sunday_zero_offset());
        let dom_match = self.dom.contains(&(i32::from(local.day())));
        let dow_match = self.dow.contains(&dow) || (dow == 0 && self.dow.contains(&7));
        if !self.dom_any && !self.dow_any {
            dom_match || dow_match
        } else {
            dom_match && dow_match
        }
    }

    fn matches(&self, local: DateTime) -> bool {
        self.minute.contains(&(i32::from(local.minute())))
            && self.hour.contains(&(i32::from(local.hour())))
            && self.day_matches(local)
            && self.month.contains(&(i32::from(local.month())))
    }

    /// Earliest naive wall-clock minute after `naive` that could match. A
    /// month mismatch caps at the next local midnight — hops never exceed one
    /// local day, so zone-aware callers can verify each hop against a
    /// constant UTC offset.
    fn next_candidate_local(&self, naive: DateTime) -> DateTime {
        if !self.month.contains(&(i32::from(naive.month()))) {
            return next_local_midnight(naive);
        }
        if !self.day_matches(naive) {
            return next_local_midnight(naive);
        }
        if !self.hour.contains(&(i32::from(naive.hour()))) {
            let next_hour = self
                .hour
                .iter()
                .copied()
                .filter(|h| *h > i32::from(naive.hour()))
                .min();
            return match next_hour {
                Some(hour) => naive
                    .with()
                    .hour(hour as i8)
                    .minute(0)
                    .build()
                    .expect("valid hour and minute"),
                None => next_local_midnight(naive),
            };
        }
        let next_minute = self
            .minute
            .iter()
            .copied()
            .filter(|m| *m > i32::from(naive.minute()))
            .min();
        match next_minute {
            Some(minute) => naive
                .with()
                .minute(minute as i8)
                .build()
                .expect("valid minute"),
            None => naive
                .with()
                .minute(0)
                .build()
                .expect("valid minute")
                .checked_add(SignedDuration::from_hours(1))
                .expect("valid next hour"),
        }
    }
}

fn next_local_midnight(naive: DateTime) -> DateTime {
    naive
        .date()
        .tomorrow()
        .expect("valid successor date")
        .to_datetime(Time::midnight())
}

/// A parsed schedule; `parse_schedule` is the only constructor.
#[derive(Debug, Clone)]
pub enum Schedule {
    Cron {
        fields: Box<CronFields>,
        tz: TimeZone,
    },
    Interval {
        seconds: i64,
    },
    Once {
        run_at: Timestamp,
    },
}

impl Schedule {
    /// The `scheduleKind` reported in `due` rows.
    pub fn kind(&self) -> &'static str {
        match self {
            Schedule::Cron { .. } => "cron",
            Schedule::Interval { .. } => "interval",
            Schedule::Once { .. } => "once",
        }
    }
}

/// Parse a schedule expression in the task timezone. Unknown timezones and
/// malformed expressions fail closed.
pub fn parse_schedule(expr: &str, tz_name: &str) -> Result<Schedule, String> {
    let expr = expr.trim();
    let tz = resolve_timezone(tz_name)?;
    if expr == "@reboot" {
        return Err("@reboot is not supported by dry-run due resolver".to_string());
    }
    if let Some(captures) = interval_regex().captures(expr) {
        let count: i64 = captures[1]
            .parse()
            .map_err(|_| "invalid interval".to_string())?;
        let unit = match &captures[2] {
            "m" => 60i64,
            "h" => 3600,
            _ => 86400,
        };
        let seconds = count * unit;
        if seconds < 60 {
            return Err("interval must be at least 1 minute".to_string());
        }
        if seconds > MAX_INTERVAL_SECONDS {
            return Err("interval must be at most 366 days".to_string());
        }
        return Ok(Schedule::Interval { seconds });
    }
    if let Some(rest) = expr.strip_prefix("at ") {
        let run_at = parse_local(rest, &tz, "schedule")?;
        return Ok(Schedule::Once { run_at });
    }
    if expr.contains('T') && !expr.contains(' ') {
        let run_at = parse_local(expr, &tz, "schedule")?;
        return Ok(Schedule::Once { run_at });
    }
    let resolved = SHORTHANDS
        .iter()
        .find(|(name, _)| *name == expr)
        .map(|(_, expansion)| *expansion)
        .unwrap_or(expr);
    let parts: Vec<&str> = resolved.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(
            "schedule must be a supported @shorthand, 5-field cron, \"every <N>m|h|d\", or \"at <ISO8601>\""
                .to_string(),
        );
    }
    for part in &parts {
        if !field_regex().is_match(part) {
            return Err(format!("unsupported cron field: {part}"));
        }
    }
    Ok(Schedule::Cron {
        fields: Box::new(CronFields {
            minute: expand_field(parts[0], 0, 59)?,
            hour: expand_field(parts[1], 0, 23)?,
            dom: expand_field(parts[2], 1, 31)?,
            month: expand_field(parts[3], 1, 12)?,
            dow: expand_field(parts[4], 0, 7)?,
            dom_any: parts[2] == "*",
            dow_any: parts[4] == "*",
        }),
        tz,
    })
}

/// Zone-aware scan: jump on the local calendar, verify each hop in UTC.
///
/// A hop is trusted only when the UTC offset is unchanged across it, the
/// elapsed time equals the wall-clock delta, and the landing instant shows
/// exactly the targeted wall time; otherwise (a DST or other offset change
/// inside the hop, an ambiguous or nonexistent target) fall back to minute
/// stepping so no matching minute can be skipped. Hops are capped at one
/// local day, and tzdata has no two offset changes within one such window,
/// so the endpoint checks see every transition. The earliest instant for an
/// ambiguous jump target (fold 0) enters a repeated local hour on its first
/// pass; the second pass re-scans minute by minute.
fn first_match_zone(
    fields: &CronFields,
    tz: &TimeZone,
    mut cur: Timestamp,
    end: Timestamp,
) -> Option<Timestamp> {
    let minute = SignedDuration::from_mins(1);
    loop {
        if cur > end {
            return None;
        }
        let loc_offset = tz.to_offset(cur);
        let local_naive = loc_offset.to_datetime(cur);
        if fields.matches(local_naive) {
            return Some(cur);
        }
        let target = fields.next_candidate_local(local_naive);
        // Earliest instant on ambiguity (fold 0), matching Python's
        // `replace(tzinfo=tz)`. A nonexistent target always fails the landing
        // check below in the Python reference, so skipping straight to the
        // minute step is equivalent.
        let target_offset = match tz.to_ambiguous_timestamp(target).offset() {
            AmbiguousOffset::Unambiguous { offset } => offset,
            AmbiguousOffset::Fold { before, .. } => before,
            AmbiguousOffset::Gap { .. } => {
                cur += minute;
                continue;
            }
        };
        let Ok(resolved) = target_offset.to_timestamp(target) else {
            cur += minute;
            continue;
        };
        let cand = truncate_minute(resolved);
        let cand_offset = tz.to_offset(cand);
        if cand <= cur
            || cand.duration_since(cur) != target.duration_since(local_naive)
            || cand_offset.to_datetime(cand) != target
            || cand_offset != loc_offset
        {
            cur += minute;
            continue;
        }
        cur = cand;
    }
}

fn first_match(schedule: &Schedule, cur: Timestamp, end: Timestamp) -> Option<Timestamp> {
    if cur > end {
        return None;
    }
    match schedule {
        Schedule::Cron { fields, tz, .. } => first_match_zone(fields, tz, cur, end),
        // Only cron schedules scan; interval/once have closed-form answers.
        _ => None,
    }
}

/// All matching minutes in `[start_exclusive, end_inclusive]` (UTC,
/// ascending), capped at `cap` with a truncation flag.
pub fn iter_occurrences(
    schedule: &Schedule,
    start_exclusive: Timestamp,
    end_inclusive: Timestamp,
    cap: usize,
) -> (Vec<Timestamp>, bool) {
    let mut cur = truncate_minute(start_exclusive + SignedDuration::from_mins(1));
    let end = truncate_minute(end_inclusive);
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(hit) = first_match(schedule, cur, end) {
        if out.len() >= cap {
            truncated = true;
            break;
        }
        out.push(hit);
        cur = hit + SignedDuration::from_mins(1);
    }
    (out, truncated)
}

/// First phase-aligned interval occurrence strictly after `floor` (UTC).
fn interval_next_after(seconds: i64, floor: Timestamp, anchor: Option<Timestamp>) -> Timestamp {
    let Some(anchor) = anchor else {
        return floor + SignedDuration::from_secs(seconds);
    };
    let delta = floor.duration_since(anchor).as_secs();
    let k = if delta >= 0 { delta / seconds + 1 } else { 0 };
    let mut candidate = anchor + SignedDuration::from_secs(k * seconds);
    while candidate <= floor {
        candidate += SignedDuration::from_secs(seconds);
    }
    candidate
}

/// Due occurrences (UTC, ascending) up to `at`, plus a truncation flag.
///
/// - once: due exactly when `run_at <= at` and it has not yet run at/after
///   `run_at`.
/// - interval: phase-anchored to `anchor` (or free-running from `last`); a
///   never-run task without an anchor is due once immediately.
/// - cron: minute-scan matched in the task timezone.
pub fn schedule_occurrences(
    schedule: &Schedule,
    last: Option<Timestamp>,
    at: Timestamp,
    anchor: Option<Timestamp>,
    cap: usize,
) -> (Vec<Timestamp>, bool) {
    match schedule {
        Schedule::Once { run_at, .. } => {
            if *run_at <= at && last.is_none_or(|last| last < *run_at) {
                (vec![*run_at], false)
            } else {
                (Vec::new(), false)
            }
        }
        Schedule::Interval { seconds, .. } => {
            if anchor.is_none() && last.is_none() {
                return (vec![at], false);
            }
            let floor = match (anchor, last) {
                (Some(anchor), Some(last)) if last > anchor => last,
                (Some(anchor), _) => anchor,
                (None, Some(last)) => last,
                (None, None) => return (Vec::new(), false),
            };
            let step = SignedDuration::from_secs(*seconds);
            let mut cur = interval_next_after(*seconds, floor, anchor);
            let mut out = Vec::new();
            let mut truncated = false;
            while cur <= at {
                if out.len() >= cap {
                    truncated = true;
                    break;
                }
                out.push(cur);
                cur += step;
            }
            (out, truncated)
        }
        Schedule::Cron { .. } => {
            let horizon = last.unwrap_or(at - SignedDuration::from_hours(366 * 24));
            iter_occurrences(schedule, horizon, at, cap)
        }
    }
}

/// Next scheduled occurrence strictly after `at` (UTC), or `None` when no
/// occurrence exists within the 366-day horizon.
pub fn next_after(
    schedule: &Schedule,
    at: Timestamp,
    anchor: Option<Timestamp>,
) -> Option<Timestamp> {
    match schedule {
        Schedule::Once { run_at, .. } => (*run_at > at).then_some(*run_at),
        Schedule::Interval { seconds, .. } => Some(interval_next_after(*seconds, at, anchor)),
        Schedule::Cron { .. } => {
            let cur = truncate_minute(at + SignedDuration::from_mins(1));
            let end = cur + SignedDuration::from_mins(366 * 24 * 60 - 1);
            first_match(schedule, cur, end)
        }
    }
}
