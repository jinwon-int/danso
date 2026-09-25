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
use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDateTime, Offset, TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;
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
/// the error (operator decision: embedded `chrono-tz` tzdb, never system
/// tzdata, and never a silent UTC fallback).
pub fn resolve_timezone(name: &str) -> Result<Tz, String> {
    let label = name.trim();
    let label = if label.is_empty() { "UTC" } else { label };
    if label.eq_ignore_ascii_case("UTC") {
        return Ok(Tz::UTC);
    }
    label
        .parse::<Tz>()
        .map_err(|_| format!("unknown timezone: {label}"))
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
    fn day_matches(&self, local: NaiveDateTime) -> bool {
        let dow = local.weekday().num_days_from_sunday() as i32;
        let dom_match = self.dom.contains(&(local.day() as i32));
        let dow_match = self.dow.contains(&dow) || (dow == 0 && self.dow.contains(&7));
        if !self.dom_any && !self.dow_any {
            dom_match || dow_match
        } else {
            dom_match && dow_match
        }
    }

    fn matches(&self, local: NaiveDateTime) -> bool {
        self.minute.contains(&(local.minute() as i32))
            && self.hour.contains(&(local.hour() as i32))
            && self.day_matches(local)
            && self.month.contains(&(local.month() as i32))
    }

    /// Earliest naive wall-clock minute after `naive` that could match.
    /// With `month_jump = false` a month mismatch caps at the next local
    /// midnight so zone-aware callers can verify each hop against a constant
    /// UTC offset.
    fn next_candidate_local(&self, naive: NaiveDateTime, month_jump: bool) -> NaiveDateTime {
        if !self.month.contains(&(naive.month() as i32)) {
            if !month_jump {
                return next_local_midnight(naive);
            }
            let next_month = self
                .month
                .iter()
                .copied()
                .filter(|m| *m > naive.month() as i32)
                .min();
            return match next_month {
                Some(month) => month_start(naive.year(), month),
                None => month_start(
                    naive.year() + 1,
                    *self.month.iter().min().expect("non-empty"),
                ),
            };
        }
        if !self.day_matches(naive) {
            return next_local_midnight(naive);
        }
        if !self.hour.contains(&(naive.hour() as i32)) {
            let next_hour = self
                .hour
                .iter()
                .copied()
                .filter(|h| *h > naive.hour() as i32)
                .min();
            return match next_hour {
                Some(hour) => naive
                    .with_hour(hour as u32)
                    .expect("valid hour")
                    .with_minute(0)
                    .expect("valid minute"),
                None => next_local_midnight(naive),
            };
        }
        let next_minute = self
            .minute
            .iter()
            .copied()
            .filter(|m| *m > naive.minute() as i32)
            .min();
        match next_minute {
            Some(minute) => naive.with_minute(minute as u32).expect("valid minute"),
            None => naive.with_minute(0).expect("valid minute") + Duration::hours(1),
        }
    }
}

fn month_start(year: i32, month: i32) -> NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(year, month as u32, 1)
        .expect("valid date")
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
}

fn next_local_midnight(naive: NaiveDateTime) -> NaiveDateTime {
    naive
        .date()
        .succ_opt()
        .expect("valid successor date")
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
}

/// A parsed schedule; `parse_schedule` is the only constructor.
#[derive(Debug, Clone)]
pub enum Schedule {
    Cron {
        fields: Box<CronFields>,
        tz: Tz,
        expr: String,
    },
    Interval {
        seconds: i64,
        expr: String,
    },
    Once {
        run_at: DateTime<Utc>,
        expr: String,
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
        return Ok(Schedule::Interval {
            seconds,
            expr: expr.to_string(),
        });
    }
    if let Some(rest) = expr.strip_prefix("at ") {
        let run_at = parse_local(rest, tz, "schedule")?;
        return Ok(Schedule::Once {
            run_at,
            expr: expr.to_string(),
        });
    }
    if expr.contains('T') && !expr.contains(' ') {
        let run_at = parse_local(expr, tz, "schedule")?;
        return Ok(Schedule::Once {
            run_at,
            expr: expr.to_string(),
        });
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
        expr: resolved.to_string(),
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
    tz: Tz,
    mut cur: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    loop {
        if cur > end {
            return None;
        }
        let loc = cur.with_timezone(&tz);
        let local_naive = loc.naive_local();
        if fields.matches(local_naive) {
            return Some(cur);
        }
        let target = fields.next_candidate_local(local_naive, false);
        // Earliest instant on ambiguity (fold 0), matching Python's
        // `replace(tzinfo=tz)`. A nonexistent target always fails the landing
        // check below in the Python reference, so skipping straight to the
        // minute step is equivalent.
        let resolved = match tz.from_local_datetime(&target) {
            LocalResult::Single(dt) => dt,
            LocalResult::Ambiguous(earliest, _) => earliest,
            LocalResult::None => {
                cur += Duration::minutes(1);
                continue;
            }
        };
        let cand = truncate_minute(resolved.with_timezone(&Utc));
        let cand_loc = cand.with_timezone(&tz);
        if cand <= cur
            || cand - cur != target - local_naive
            || cand_loc.naive_local() != target
            || cand_loc.offset().fix() != loc.offset().fix()
        {
            cur += Duration::minutes(1);
            continue;
        }
        cur = cand;
    }
}

fn first_match(
    schedule: &Schedule,
    cur: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if cur > end {
        return None;
    }
    match schedule {
        Schedule::Cron { fields, tz, .. } => first_match_zone(fields, *tz, cur, end),
        // Only cron schedules scan; interval/once have closed-form answers.
        _ => None,
    }
}

/// All matching minutes in `[start_exclusive, end_inclusive]` (UTC,
/// ascending), capped at `cap` with a truncation flag.
pub fn iter_occurrences(
    schedule: &Schedule,
    start_exclusive: DateTime<Utc>,
    end_inclusive: DateTime<Utc>,
    cap: usize,
) -> (Vec<DateTime<Utc>>, bool) {
    let mut cur = truncate_minute(start_exclusive + Duration::minutes(1));
    let end = truncate_minute(end_inclusive);
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(hit) = first_match(schedule, cur, end) {
        if out.len() >= cap {
            truncated = true;
            break;
        }
        out.push(hit);
        cur = hit + Duration::minutes(1);
    }
    (out, truncated)
}

/// First phase-aligned interval occurrence strictly after `floor` (UTC).
fn interval_next_after(
    seconds: i64,
    floor: DateTime<Utc>,
    anchor: Option<DateTime<Utc>>,
) -> DateTime<Utc> {
    let Some(anchor) = anchor else {
        return floor + Duration::seconds(seconds);
    };
    let delta = (floor - anchor).num_seconds();
    let k = if delta >= 0 { delta / seconds + 1 } else { 0 };
    let mut candidate = anchor + Duration::seconds(k * seconds);
    while candidate <= floor {
        candidate += Duration::seconds(seconds);
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
    last: Option<DateTime<Utc>>,
    at: DateTime<Utc>,
    anchor: Option<DateTime<Utc>>,
    cap: usize,
) -> (Vec<DateTime<Utc>>, bool) {
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
            let step = Duration::seconds(*seconds);
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
            let horizon = last.unwrap_or(at - Duration::days(366));
            iter_occurrences(schedule, horizon, at, cap)
        }
    }
}

/// Next scheduled occurrence strictly after `at` (UTC), or `None` when no
/// occurrence exists within the 366-day horizon.
pub fn next_after(
    schedule: &Schedule,
    at: DateTime<Utc>,
    anchor: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match schedule {
        Schedule::Once { run_at, .. } => (*run_at > at).then_some(*run_at),
        Schedule::Interval { seconds, .. } => Some(interval_next_after(*seconds, at, anchor)),
        Schedule::Cron { .. } => {
            let cur = truncate_minute(at + Duration::minutes(1));
            let end = cur + Duration::minutes(366 * 24 * 60 - 1);
            first_match(schedule, cur, end)
        }
    }
}
