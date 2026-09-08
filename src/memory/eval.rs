//! Built-in memory evaluation (issue #52 §9/§11): the golden and scenario
//! suites run the real recall path against synthetic fixtures with a pinned
//! clock, so the gates are deterministic forever. Fixtures are copied from
//! the ccc-node `ccc-memory-eval.sh` suites (see tests/fixtures/memory/
//! README.md for the origin commit); the golden sources are re-mapped onto
//! the Danso document universe (memory / structured / state — no cache
//! documents). No network, no provider.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

use super::paths::{self, Route};
use super::recall::{self, SearchOptions};

/// The evaluation clock is pinned AFTER every fixture date so the temporal
/// cases never flip with wall time (fixture evaluation, not a live check).
pub const EVAL_NOW: &str = "2026-09-08T12:00:00Z";

const SCENARIO_MEMORY: &str = include_str!("../../tests/fixtures/memory/scenario/MEMORY.md");
const SCENARIO_USER: &str = include_str!("../../tests/fixtures/memory/scenario/USER.md");
const SCENARIO_FACTS: &str = include_str!("../../tests/fixtures/memory/scenario/facts.jsonl");
const GOLDEN_MEMORY: &str = include_str!("../../tests/fixtures/memory/golden/MEMORY.md");
const GOLDEN_USER: &str = include_str!("../../tests/fixtures/memory/golden/USER.md");
const GOLDEN_RESUME: &str = include_str!("../../tests/fixtures/memory/golden/resume.md");
const GOLDEN_WORKING_STATE: &str =
    include_str!("../../tests/fixtures/memory/golden/working-state.md");

/// Which built-in suite to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Golden,
    Scenario,
}

struct Case {
    id: &'static str,
    query: &'static str,
    /// Substrings that must appear in result paths (ccc `exp in p` rule).
    expected: &'static [&'static str],
    /// Substrings that must never be the top result.
    not_top: &'static [&'static str],
    /// Substrings that must not appear in any result.
    absent: &'static [&'static str],
    as_of: Option<&'static str>,
}

struct CaseResult {
    id: &'static str,
    paths: Vec<String>,
    ranks: Vec<Option<usize>>,
    forbidden_top: bool,
    absent_hit: bool,
}

fn scenario_cases() -> Vec<Case> {
    vec![
        Case {
            id: "accurate-retrieval",
            query: "Korean practical evidence reports",
            expected: &["USER.md"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
        Case {
            id: "incremental-structured",
            query: "benchmark adapter experiments synthetic scenario fixtures",
            expected: &["benchmark-adapter"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
        Case {
            id: "temporal-current",
            query: "current editor preference Helix",
            expected: &["new-editor"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
        Case {
            id: "temporal-history",
            query: "historical editor Vim before Helix",
            expected: &["old-editor"],
            not_top: &[],
            absent: &[],
            as_of: Some("2026-02-01T00:00:00Z"),
        },
        Case {
            id: "volatile-demotion",
            query: "durable operating policy no-network startup",
            expected: &["MEMORY.md"],
            not_top: &["volatile-pr"],
            absent: &[],
            as_of: None,
        },
        Case {
            id: "temporal-current-semantic",
            query: "team standup time",
            expected: &["standup-new"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
        Case {
            id: "temporal-as-of",
            query: "team standup time",
            expected: &["standup-old"],
            not_top: &[],
            absent: &["standup-new"],
            as_of: Some("2026-01-15T00:00:00Z"),
        },
        Case {
            id: "temporal-future-excluded",
            query: "deploy freeze window Friday production deploys",
            expected: &[],
            not_top: &[],
            absent: &["freeze-future"],
            as_of: None,
        },
        Case {
            id: "constraint-no-age-expiry",
            query: "raw secrets memory artifacts",
            expected: &["secrets-constraint"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
    ]
}

fn golden_cases() -> Vec<Case> {
    vec![
        Case {
            id: "a2a-brokers",
            query: "Team1 Seoseo broker Team2 Gwakga",
            expected: &["MEMORY.md"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
        Case {
            id: "startup-boundary",
            query: "SessionStart no-network fail-open",
            expected: &["MEMORY.md"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
        Case {
            id: "korean-report",
            query: "Korean practical evidence reports",
            expected: &["USER.md"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
        Case {
            id: "state-resume",
            query: "memory cache TTL stale warning 임계값",
            expected: &["resume.md"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
        Case {
            id: "state-working",
            query: "recall 인덱스 검색 게이트 검증",
            expected: &["working-state.md"],
            not_top: &[],
            absent: &[],
            as_of: None,
        },
    ]
}

/// Write one 0600 fixture file (memory reads enforce owner-only modes).
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Materialize one suite's fixtures into a fresh route.
fn setup(route: &Route, mode: Mode) -> Result<()> {
    paths::ensure_private_dir(&route.memories_dir())?;
    paths::ensure_private_dir(&route.state_dir())?;
    match mode {
        Mode::Scenario => {
            write_private(&route.memories_dir().join("MEMORY.md"), SCENARIO_MEMORY)?;
            write_private(&route.memories_dir().join("USER.md"), SCENARIO_USER)?;
            write_private(&route.facts_file(), SCENARIO_FACTS)?;
        }
        Mode::Golden => {
            write_private(&route.memories_dir().join("MEMORY.md"), GOLDEN_MEMORY)?;
            write_private(&route.memories_dir().join("USER.md"), GOLDEN_USER)?;
            write_private(&route.resume_file(), GOLDEN_RESUME)?;
            write_private(&route.working_state_file(), GOLDEN_WORKING_STATE)?;
        }
    }
    Ok(())
}

fn run_suite(route: &Route, cases: &[Case]) -> Result<(Vec<CaseResult>, Value)> {
    let now = super::facts::parse_timestamp(EVAL_NOW)?;
    let mut results = Vec::new();
    for case in cases.iter() {
        let options = SearchOptions {
            query: case.query,
            as_of: case.as_of,
            limit: 5,
            now,
        };
        let output = recall::search(route, &options)?;
        let paths: Vec<String> = output["results"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| item["path"].as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let ranks = case
            .expected
            .iter()
            .map(|expected| {
                paths
                    .iter()
                    .position(|path| path.contains(expected))
                    .map(|p| p + 1)
            })
            .collect();
        let forbidden_top = !case.not_top.is_empty()
            && paths
                .first()
                .is_some_and(|top| case.not_top.iter().any(|needle| top.contains(needle)));
        let absent_hit = !case.absent.is_empty()
            && paths
                .iter()
                .any(|path| case.absent.iter().any(|needle| path.contains(needle)));
        results.push(CaseResult {
            id: case.id,
            paths,
            ranks,
            forbidden_top,
            absent_hit,
        });
    }

    let mut metrics = json!({});
    for k in [1usize, 3, 5] {
        let mut precisions = Vec::new();
        let mut recalls = Vec::new();
        for (case, result) in cases.iter().zip(&results) {
            if case.expected.is_empty() {
                continue; // absence-assertion cases carry no retrieval target
            }
            let top = result.paths.iter().take(k);
            let hits = case
                .expected
                .iter()
                .filter(|expected| top.clone().any(|path| path.contains(*expected)))
                .count();
            precisions.push(hits as f64 / k.min(result.paths.len().max(1)) as f64);
            recalls.push(hits as f64 / case.expected.len() as f64);
        }
        let average = |values: &[f64]| {
            if values.is_empty() {
                0.0
            } else {
                values.iter().sum::<f64>() / values.len() as f64
            }
        };
        metrics[format!("precision_at_{k}")] = json!(round4(&average(&precisions)));
        metrics[format!("recall_at_{k}")] = json!(round4(&average(&recalls)));
    }
    let reciprocal: Vec<f64> = results
        .iter()
        .map(|result| {
            result
                .ranks
                .iter()
                .filter_map(|rank| *rank)
                .min()
                .map(|rank| 1.0 / rank as f64)
                .unwrap_or(0.0)
        })
        .collect();
    let mrr = if reciprocal.is_empty() {
        0.0
    } else {
        reciprocal.iter().sum::<f64>() / reciprocal.len() as f64
    };
    metrics["mrr"] = json!(round4(&mrr));

    let rank_of = |id: &str, index: usize| -> Option<usize> {
        results
            .iter()
            .find(|result| result.id == id)
            .and_then(|result| result.ranks.get(index))
            .copied()
            .flatten()
    };
    metrics["temporal_current_accuracy"] =
        json!(usize::from(rank_of("temporal-current", 0) == Some(1)) as f64);
    metrics["volatile_exclusion_accuracy"] = json!(usize::from(
        !results
            .iter()
            .find(|result| result.id == "volatile-demotion")
            .map(|result| result.forbidden_top)
            .unwrap_or(true)
    ) as f64);
    let semantic_ids = [
        "temporal-current-semantic",
        "temporal-as-of",
        "temporal-future-excluded",
        "constraint-no-age-expiry",
    ];
    let semantic_ok = semantic_ids.iter().all(|id| {
        let result = results.iter().find(|result| result.id == *id);
        match result {
            None => false,
            Some(result) => {
                let case = cases.iter().find(|case| case.id == *id);
                let ranks_ok = case
                    .map(|case| {
                        case.expected
                            .iter()
                            .enumerate()
                            .all(|(index, _)| result.ranks.get(index) == Some(&Some(1)))
                    })
                    .unwrap_or(true);
                ranks_ok && !result.absent_hit
            }
        }
    });
    metrics["temporal_semantic_accuracy"] = json!(usize::from(semantic_ok) as f64);

    Ok((results, metrics))
}

fn round4(value: &f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn case_json(case: &Case, result: &CaseResult) -> Value {
    json!({
        "id": case.id,
        "query": case.query,
        "expected": case.expected,
        "paths": result.paths,
        "ranks": result.ranks,
        "forbidden_top": result.forbidden_top,
        "absent_hit": result.absent_hit,
    })
}

/// Run one suite in a fresh temporary tree and return the report JSON.
/// The `ok` flag follows the design gate (§10 M1): recall@5 ≥ 0.8, p@1 ≥ 0.6,
/// and the temporal/volatile accuracies at exactly 1.0. The report is
/// returned either way so callers can print it before failing the gate.
pub fn run(mode: Mode, work_dir: &Path) -> Result<Value> {
    let suite_name = match mode {
        Mode::Golden => "golden",
        Mode::Scenario => "scenario",
    };
    let run_dir = work_dir.join(format!(
        "danso-memory-eval-{suite_name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    paths::ensure_private_dir(&run_dir)?;
    let route = Route::new(&run_dir, "global")?;
    setup(&route, mode)?;

    let cases = match mode {
        Mode::Golden => golden_cases(),
        Mode::Scenario => scenario_cases(),
    };
    let (results, metrics) = run_suite(&route, &cases)?;
    let base_ok = metrics["recall_at_5"].as_f64().unwrap_or(0.0) >= 0.8
        && metrics["precision_at_1"].as_f64().unwrap_or(0.0) >= 0.6;
    let ok = base_ok
        && match mode {
            // The golden gate is the ccc recall/precision gate; the temporal
            // accuracies are scenario competencies (§10 M1).
            Mode::Golden => true,
            Mode::Scenario => {
                metrics["temporal_current_accuracy"].as_f64().unwrap_or(0.0) == 1.0
                    && metrics["volatile_exclusion_accuracy"]
                        .as_f64()
                        .unwrap_or(0.0)
                        == 1.0
                    && metrics["temporal_semantic_accuracy"]
                        .as_f64()
                        .unwrap_or(0.0)
                        == 1.0
            }
        };

    let cases_json: Vec<Value> = cases
        .iter()
        .zip(&results)
        .map(|(case, result)| case_json(case, result))
        .collect();
    let report = json!({
        "ok": ok,
        "mode": suite_name,
        "cases": cases_json,
        "metrics": metrics,
        "now": EVAL_NOW,
    });
    let _ = std::fs::remove_dir_all(&run_dir);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_and_golden_suites_pass_their_gates() {
        let dir = tempfile::tempdir().unwrap();
        let scenario = run(Mode::Scenario, dir.path()).unwrap();
        assert_eq!(scenario["ok"], json!(true));
        assert_eq!(scenario["cases"].as_array().unwrap().len(), 9);
        let golden = run(Mode::Golden, dir.path()).unwrap();
        assert_eq!(golden["ok"], json!(true));
        assert_eq!(golden["cases"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn eval_clock_is_pinned_after_every_fixture_date() {
        let now = super::super::facts::parse_timestamp(EVAL_NOW).unwrap();
        assert_eq!(now.to_rfc3339(), "2026-09-08T12:00:00+00:00");
    }
}
