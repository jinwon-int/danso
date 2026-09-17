//! Resolving a pending activation (`docs/unified-design.md` §6.3; ccc-node #1527).
//!
//! `install` replaces a binary and records that it did. Nothing in that step
//! proves the replacement is *serving*. #1527 is the incident where that gap
//! swallowed a failed activation: the unit restarted, systemd reported success,
//! and the generation had not changed, because a restart that re-executes the
//! same image looks exactly like a restart that picked up a new one.
//!
//! So the question this module answers is deliberately narrow — **is the image
//! identified by the pending record the image that is serving?** — and the only
//! acceptable evidence is a digest, never a process lifecycle event.
//!
//! ## Where the digest comes from
//!
//! Two cases, because they really are different installations:
//!
//! * The record names services. Something long-running is meant to be serving
//!   the new image, so the evidence is the `runtime_generation` that process
//!   publishes in `health.json`. It must be **fresh**: a document older than
//!   [`crate::health::STALE_AFTER_SECONDS`] describes a process that may no
//!   longer exist, and stale evidence is not evidence.
//! * The record names no services — a CLI-only install. There is no process to
//!   ask, and the installed file *is* the image every future invocation will
//!   run, so the installed binary's digest is the honest answer.
//!
//! ## Evidence has to be *about this activation*
//!
//! Freshness alone is not enough. The document the old process wrote seconds
//! before the swap is perfectly fresh and says the old digest — reading it as
//! "the replacement is not serving" condemns a generation that has simply not
//! been restarted yet. So a document written before the record's `started_at`
//! is [`Serving::Premature`], not a verdict. The ordinary operator sequence is
//! `apply; activate; restart; activate`, and the first `activate` must not
//! decide anything.
//!
//! ## What absent evidence means
//!
//! Not "failed". A missing or stale health document means the question cannot
//! be answered yet, which is [`Activation::Unverified`] and exit 3 — the same
//! `unverified` discipline `service status` follows, for the same reason: a
//! false negative here marks a perfectly good generation as failed and throws
//! away the operator's confidence in the record.
//!
//! Only a fresh digest that *disagrees* with the target is a failure. That
//! record is marked `failed` and left in place with its snapshot path intact,
//! because the evidence and the rollback target are the two things a person
//! investigating needs and the two things a retry would destroy.

use crate::health::{HealthDocument, STALE_AFTER_SECONDS};
use crate::install::UpdateLock;
use crate::update::{self, Outcome, PendingActivation};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

/// Exit code for "the question cannot be answered yet".
pub const EXIT_UNVERIFIED: i32 = 3;
/// Exit code for "the replacement is not serving".
pub const EXIT_NOT_ACTIVATED: i32 = 1;

/// What is known about the image that is actually serving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "evidence", rename_all = "snake_case")]
pub enum Serving {
    /// A digest this module is willing to judge against.
    Known {
        binary_sha256: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        health_age_seconds: Option<i64>,
    },
    /// The health document is outside the freshness window, in either
    /// direction. A document from the future is not evidence for a
    /// destructive judgement even though `service status` tolerates it.
    Stale { age_seconds: Option<i64> },
    /// The document predates the activation it would be judging.
    Premature { age_seconds: Option<i64> },
    /// No health document, or one that cannot be read.
    Missing,
    /// A health document that names no usable image digest.
    Unidentified,
}

/// A digest this module is willing to compare.
///
/// Anything else is [`Serving::Unidentified`]: a disagreement should mean "a
/// different image is serving", not "the document used a format we do not
/// read". Comparing unvalidated strings byte-for-byte would turn an uppercase
/// or prefixed digest into a *failed* activation.
fn usable_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// First 16 characters — characters, not bytes: the digest comes from a file
/// this module does not control, and slicing it by byte index panics on any
/// multi-byte character.
fn short(digest: &str) -> String {
    digest.chars().take(16).collect()
}

/// The result of asking whether a pending activation is satisfied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Activation {
    /// There is no pending record; nothing to resolve.
    Nothing,
    /// The record was already resolved, by this or an earlier run.
    AlreadyResolved { outcome: Outcome },
    /// The serving image is the recorded target. The record is now `activated`.
    Activated { binary_sha256: String },
    /// The serving image is something else. The record is now `failed`.
    NotActivated {
        expected: String,
        serving: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
    },
    /// The evidence does not support a judgement. Nothing was written.
    Unverified { serving: Serving },
}

impl Activation {
    pub fn exit_code(&self) -> i32 {
        match self {
            Activation::Nothing | Activation::Activated { .. } => 0,
            // A failed activation does not become acceptable by being asked
            // about twice; reporting 0 here is how it disappears from a cron
            // wrapper's view after the first run.
            Activation::AlreadyResolved { outcome } => match outcome {
                Outcome::Failed => EXIT_NOT_ACTIVATED,
                _ => 0,
            },
            Activation::NotActivated { .. } => EXIT_NOT_ACTIVATED,
            Activation::Unverified { .. } => EXIT_UNVERIFIED,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Activation::Nothing => "no pending activation".to_string(),
            Activation::AlreadyResolved { outcome } => {
                format!("activation already resolved: {outcome:?}")
            }
            Activation::Activated { binary_sha256 } => {
                format!("activated {}", short(binary_sha256))
            }
            Activation::NotActivated {
                expected, serving, ..
            } => format!(
                "not activated: serving {} but expected {}",
                short(serving),
                short(expected)
            ),
            Activation::Unverified { .. } => "cannot verify which image is serving".to_string(),
        }
    }
}

/// Read the serving image's digest from a published health document.
///
/// `None` is not an error: a CLI-only installation has no service data
/// directory to resolve, and a record that names services then reports
/// `unverified` rather than failing on a path it never needed.
pub fn serving_from_health(
    health_path: Option<&Path>,
    not_before: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Serving {
    let Some(health_path) = health_path else {
        return Serving::Missing;
    };
    let Ok(raw) = std::fs::read(health_path) else {
        return Serving::Missing;
    };
    let Ok(document) = serde_json::from_slice::<HealthDocument>(&raw) else {
        // An unreadable document is not a missing process; it is a question
        // that cannot be answered, which is the same outcome here.
        return Serving::Missing;
    };
    let Some(age) = document.age_seconds(now) else {
        return Serving::Stale { age_seconds: None };
    };
    // Both directions. `service status` tolerates a future timestamp so a clock
    // adjustment never reports a live service as down; here the judgement is
    // destructive, and a document from the future is not something to condemn a
    // generation with.
    if age.abs() > STALE_AFTER_SECONDS {
        return Serving::Stale {
            age_seconds: Some(age),
        };
    }
    if let Some(not_before) = not_before
        && written_before(document.observed_at(), not_before)
    {
        return Serving::Premature {
            age_seconds: Some(age),
        };
    }
    match document.runtime_generation.binary_sha256 {
        Some(digest) if usable_digest(&digest) => Serving::Known {
            binary_sha256: digest,
            health_age_seconds: Some(age),
        },
        _ => Serving::Unidentified,
    }
}

/// Whether `observed` is earlier than `boundary`.
///
/// An unparseable timestamp on either side is treated as "before": the point of
/// the check is to refuse evidence that cannot be shown to postdate the
/// activation, and a timestamp nobody can read shows nothing.
fn written_before(observed: &str, boundary: &str) -> bool {
    let parse = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|stamp| stamp.with_timezone(&chrono::Utc))
    };
    match (parse(observed), parse(boundary)) {
        (Some(observed), Some(boundary)) => observed < boundary,
        _ => true,
    }
}

/// Read the serving image's digest for a CLI-only installation.
fn serving_from_installed_binary(danso_home: &Path) -> Serving {
    match crate::install::read_no_follow(&update::installed_binary(danso_home)) {
        Ok(bytes) => Serving::Known {
            binary_sha256: crate::release::hex_digest(&bytes),
            health_age_seconds: None,
        },
        Err(_) => Serving::Missing,
    }
}

/// Which evidence applies to this record.
///
/// Exposed so `update status` and this module cannot drift on the rule.
pub fn serving_for(
    record: &PendingActivation,
    danso_home: &Path,
    health_path: Option<&Path>,
    now: chrono::DateTime<chrono::Utc>,
) -> Serving {
    if record.services.is_empty() {
        serving_from_installed_binary(danso_home)
    } else {
        serving_from_health(health_path, Some(&record.started_at), now)
    }
}

/// Resolve the pending activation, if the evidence allows it.
pub fn activate(
    danso_home: &Path,
    health_path: Option<&Path>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Activation> {
    judge(danso_home, health_path, now, false)
}

/// Judge a record again, including one already marked `failed`.
///
/// Without this an operator whose activation failed for an environmental
/// reason — the unit was down, the health document was missing — has no way
/// forward: `apply` refuses past the failed record and `rollback` needs a
/// snapshot a first install never had. Re-judging is safe because evidence
/// must postdate the record to count at all, so a retry cannot be satisfied by
/// the same stale document that produced the failure.
pub fn retry(
    danso_home: &Path,
    health_path: Option<&Path>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Activation> {
    judge(danso_home, health_path, now, true)
}

fn judge(
    danso_home: &Path,
    health_path: Option<&Path>,
    now: chrono::DateTime<chrono::Utc>,
    rejudge_failed: bool,
) -> Result<Activation> {
    let state = update::state_dir(danso_home);
    // The same lock `apply` takes: resolving a record while another process is
    // writing one would judge a target that is already being replaced.
    let lock = UpdateLock::acquire(&state)?;

    let Some(mut record) = update::read_pending(&state)? else {
        return Ok(Activation::Nothing);
    };
    let settled = match record.outcome {
        Outcome::Pending => false,
        Outcome::Failed => !rejudge_failed,
        Outcome::Activated => true,
    };
    if settled {
        return Ok(Activation::AlreadyResolved {
            outcome: record.outcome,
        });
    }

    let serving = serving_for(&record, danso_home, health_path, now);
    let Serving::Known { binary_sha256, .. } = &serving else {
        // Nothing is written. A record that cannot be judged must stay
        // judgeable on the next run.
        return Ok(Activation::Unverified { serving });
    };

    let outcome = if *binary_sha256 == record.target.binary_sha256 {
        Activation::Activated {
            binary_sha256: binary_sha256.clone(),
        }
    } else {
        Activation::NotActivated {
            expected: record.target.binary_sha256.clone(),
            serving: binary_sha256.clone(),
            snapshot: record.snapshot.clone(),
        }
    };

    record.outcome = match outcome {
        Activation::Activated { .. } => Outcome::Activated,
        _ => Outcome::Failed,
    };
    record.updated_at = now.to_rfc3339();
    update::write_pending(&state, &record).context("record the activation result")?;
    drop(lock);

    crate::install::log_generation_event(
        danso_home,
        match record.outcome {
            Outcome::Activated => "activated",
            _ => "activation_failed",
        },
        Some(&record.target.version),
        Some(&record.target.binary_sha256),
        record.previous.as_ref().map(|r| r.binary_sha256.as_str()),
    );
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::{RUNTIME_GENERATION_SCHEMA, RuntimeGeneration};
    use crate::update::GenerationRef;

    const NOW: &str = "2026-09-17T00:00:00+00:00";

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(NOW)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    /// A record whose activation started a minute before `NOW`, so a health
    /// document stamped `NOW` legitimately postdates it.
    fn pending(target: &str, services: Vec<String>) -> PendingActivation {
        let mut record = pending_at(target, services);
        record.started_at = (now() - chrono::Duration::seconds(60)).to_rfc3339();
        record
    }

    fn pending_at(target: &str, services: Vec<String>) -> PendingActivation {
        PendingActivation::new(
            GenerationRef {
                version: "9.9.9".into(),
                binary_sha256: target.to_string(),
            },
            Some(GenerationRef {
                version: String::new(),
                binary_sha256: "a".repeat(64),
            }),
            services,
            Some("/x/bin/danso.prev".to_string()),
        )
    }

    /// Build the document with the real types, not hand-written JSON: a
    /// literal drifts from the schema silently, and this module would then be
    /// tested against a shape nothing publishes.
    fn write_health(path: &Path, updated_at: &str, sha: Option<&str>) {
        use crate::health::{
            HEALTH_SCHEMA_VERSION, HealthDocument, ProcessHealth, RunMode, ServiceHealth,
            TelegramHealth, TurnOccupancy, WorkloadHealth,
        };
        use crate::status::ServiceState;
        let document = HealthDocument {
            schema_version: HEALTH_SCHEMA_VERSION,
            started_at: NOW.to_string(),
            last_poll_at: updated_at.to_string(),
            active_turn_count: 0,
            queued_counts: Default::default(),
            service_pid: 1,
            process: ProcessHealth {
                pid: 1,
                started_at: NOW.to_string(),
                mode: RunMode::Run,
            },
            service: ServiceHealth {
                state: ServiceState::Available,
                reason: None,
            },
            telegram: TelegramHealth {
                state: ServiceState::Available,
                last_ok_at: Some(updated_at.to_string()),
                last_error_at: None,
                consecutive_failures: 0,
            },
            workload: WorkloadHealth {
                active_requests: 0,
                waiting_for_turn: 0,
                turn_occupancy: TurnOccupancy::Idle,
                oldest_request_age_seconds: None,
            },
            runtime_generation: RuntimeGeneration {
                schema: RUNTIME_GENERATION_SCHEMA.to_string(),
                binary_sha256: sha.map(str::to_string),
                version: "9.9.9".into(),
                exe_path: None,
                observed_at: NOW.to_string(),
            },
            updated_at: updated_at.to_string(),
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        home: std::path::PathBuf,
        health: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let health = dir.path().join("data/health.json");
            std::fs::create_dir_all(&home).unwrap();
            Self {
                _dir: dir,
                home,
                health,
            }
        }

        fn state(&self) -> std::path::PathBuf {
            update::state_dir(&self.home)
        }

        fn put_pending(&self, record: &PendingActivation) {
            update::write_pending(&self.state(), record).unwrap();
        }

        fn install(&self, body: &str) -> String {
            let target = update::installed_binary(&self.home);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, body).unwrap();
            crate::release::hex_digest(body.as_bytes())
        }

        fn run(&self) -> Activation {
            activate(&self.home, Some(&self.health), now()).unwrap()
        }
    }

    #[test]
    fn nothing_to_resolve_is_success() {
        let fixture = Fixture::new();
        assert_eq!(fixture.run(), Activation::Nothing);
        assert_eq!(Activation::Nothing.exit_code(), 0);
    }

    #[test]
    fn a_serving_target_resolves_the_record() {
        let fixture = Fixture::new();
        let target = "b".repeat(64);
        fixture.put_pending(&pending(&target, vec!["danso.service".into()]));
        write_health(&fixture.health, NOW, Some(&target));

        let outcome = fixture.run();
        assert_eq!(
            outcome,
            Activation::Activated {
                binary_sha256: target.clone()
            }
        );
        assert_eq!(outcome.exit_code(), 0);
        assert_eq!(outcome.summary(), format!("activated {}", &target[..16]));
        let record = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert_eq!(record.outcome, Outcome::Activated);
        assert!(!record.is_unresolved());
    }

    #[test]
    fn a_different_serving_image_fails_and_keeps_the_snapshot() {
        let fixture = Fixture::new();
        let target = "b".repeat(64);
        fixture.put_pending(&pending(&target, vec!["danso.service".into()]));
        // The unit restarted and came back on the *old* image — exactly #1527.
        write_health(&fixture.health, NOW, Some(&"a".repeat(64)));

        let outcome = fixture.run();
        assert_eq!(outcome.exit_code(), EXIT_NOT_ACTIVATED);
        assert_eq!(
            outcome.summary(),
            format!(
                "not activated: serving {} but expected {}",
                "a".repeat(16),
                "b".repeat(16)
            )
        );
        let Activation::NotActivated { snapshot, .. } = &outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(
            snapshot.as_deref(),
            Some("/x/bin/danso.prev"),
            "the rollback target must survive the failure that needs it"
        );
        let record = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert_eq!(record.outcome, Outcome::Failed);
        assert_eq!(record.snapshot.as_deref(), Some("/x/bin/danso.prev"));
    }

    #[test]
    fn stale_or_missing_evidence_is_unverified_and_writes_nothing() {
        let target = "b".repeat(64);
        let old = "2026-09-16T23:00:00+00:00"; // an hour before NOW
        for (label, setup) in [
            ("no health document", None),
            ("stale health document", Some((old, Some(target.clone())))),
            ("health names no image", Some((NOW, None))),
        ] {
            let fixture = Fixture::new();
            fixture.put_pending(&pending(&target, vec!["danso.service".into()]));
            if let Some((at, sha)) = setup {
                write_health(&fixture.health, at, sha.as_deref());
            }

            let outcome = fixture.run();
            assert_eq!(outcome.exit_code(), EXIT_UNVERIFIED, "{label}");
            let record = update::read_pending(&fixture.state()).unwrap().unwrap();
            assert!(
                record.is_unresolved(),
                "{label}: an unanswerable question must not mark the record; \
                 a false failure throws away a good generation"
            );
        }
    }

    #[test]
    fn the_freshness_window_is_the_health_documents_own() {
        // The record has to predate both documents, or the premature rule
        // decides these cases instead of the freshness rule.
        let started = (now() - chrono::Duration::seconds(STALE_AFTER_SECONDS + 10)).to_rfc3339();
        let target = "b".repeat(64);

        let fixture = Fixture::new();
        let mut record = pending(&target, vec!["danso.service".into()]);
        record.started_at = started.clone();
        fixture.put_pending(&record);
        // Exactly on the boundary, which is inside.
        let inside = (now() - chrono::Duration::seconds(STALE_AFTER_SECONDS)).to_rfc3339();
        write_health(&fixture.health, &inside, Some(&target));
        assert_eq!(fixture.run().exit_code(), 0);

        let fixture = Fixture::new();
        let mut record = pending(&target, vec!["danso.service".into()]);
        record.started_at = started;
        fixture.put_pending(&record);
        let outside = (now() - chrono::Duration::seconds(STALE_AFTER_SECONDS + 1)).to_rfc3339();
        write_health(&fixture.health, &outside, Some(&target));
        assert_eq!(fixture.run().exit_code(), EXIT_UNVERIFIED);
    }

    #[test]
    fn evidence_written_before_the_activation_is_not_a_verdict() {
        let fixture = Fixture::new();
        let target = "b".repeat(64);
        let record = pending(&target, vec!["danso.service".into()]);
        fixture.put_pending(&record);
        // The document the not-yet-restarted process wrote, 10 seconds before
        // the activation began. It is fresh, and it says the old digest.
        let before = (now() - chrono::Duration::seconds(70)).to_rfc3339();
        write_health(&fixture.health, &before, Some(&"a".repeat(64)));

        let outcome = fixture.run();
        assert_eq!(
            outcome,
            Activation::Unverified {
                serving: Serving::Premature {
                    age_seconds: Some(70)
                }
            },
            "`apply; activate; restart; activate` must not condemn the \
             generation on the first activate"
        );
        assert!(
            update::read_pending(&fixture.state())
                .unwrap()
                .unwrap()
                .is_unresolved()
        );

        // And once the process restarts and republishes, the same record
        // resolves cleanly.
        write_health(&fixture.health, NOW, Some(&target));
        assert_eq!(fixture.run().exit_code(), 0);
    }

    #[test]
    fn a_health_document_from_the_future_is_not_evidence() {
        let fixture = Fixture::new();
        let target = "b".repeat(64);
        fixture.put_pending(&pending(&target, vec!["danso.service".into()]));
        let ahead = (now() + chrono::Duration::seconds(STALE_AFTER_SECONDS + 1)).to_rfc3339();
        write_health(&fixture.health, &ahead, Some(&"a".repeat(64)));

        let outcome = fixture.run();
        assert_eq!(
            outcome.exit_code(),
            EXIT_UNVERIFIED,
            "`service status` tolerates a future stamp so a clock change never \
             reports a live service down; condemning a generation is a \
             different kind of decision"
        );
    }

    #[test]
    fn a_digest_this_module_cannot_compare_is_unidentified_not_a_mismatch() {
        for digest in [
            "\u{65e5}\u{672c}\u{8a9e}\u{65e5}\u{672c}\u{8a9e}",
            "B".repeat(64).as_str(),
            "sha256:aaaa",
            "",
        ] {
            let fixture = Fixture::new();
            let target = "b".repeat(64);
            fixture.put_pending(&pending(&target, vec!["danso.service".into()]));
            write_health(&fixture.health, NOW, Some(digest));

            let outcome = fixture.run();
            assert_eq!(
                outcome,
                Activation::Unverified {
                    serving: Serving::Unidentified
                },
                "{digest:?} is a format this module does not read, not proof \
                 that a different image is serving"
            );
            // And the human rendering must survive whatever was in the file.
            assert_eq!(outcome.summary(), "cannot verify which image is serving");
        }
    }

    #[test]
    fn a_failed_activation_stays_visible_and_can_be_re_judged() {
        let fixture = Fixture::new();
        let target = "b".repeat(64);
        fixture.put_pending(&pending(&target, vec!["danso.service".into()]));
        write_health(&fixture.health, NOW, Some(&"a".repeat(64)));
        assert_eq!(fixture.run().exit_code(), EXIT_NOT_ACTIVATED);

        // Asking again must not make it look fine.
        assert_eq!(
            fixture.run().exit_code(),
            EXIT_NOT_ACTIVATED,
            "a failed activation does not become acceptable by being asked \
             about twice"
        );

        // The environment is fixed; re-judging is the way forward that does
        // not require hand-editing the record.
        write_health(&fixture.health, NOW, Some(&target));
        let outcome = retry(&fixture.home, Some(&fixture.health), now()).unwrap();
        assert_eq!(outcome.exit_code(), 0);
        assert_eq!(
            update::read_pending(&fixture.state())
                .unwrap()
                .unwrap()
                .outcome,
            Outcome::Activated
        );
    }

    #[test]
    fn the_activation_log_names_the_generation_it_judged() {
        let fixture = Fixture::new();
        let target = "b".repeat(64);
        fixture.put_pending(&pending(&target, vec!["danso.service".into()]));
        write_health(&fixture.health, NOW, Some(&"a".repeat(64)));
        fixture.run();

        let log = std::fs::read_to_string(fixture.state().join(update::UPDATE_LOG_FILE))
            .expect("the judgement is logged");
        let line: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(line["event"], "activation_failed");
        assert_eq!(
            line["target_sha256"], target,
            "which generation failed must be recoverable after the record it \
             referred to has been superseded"
        );
        assert_eq!(line["previous_sha256"], "a".repeat(64));
    }

    #[test]
    fn the_evidence_taxonomy_is_reported_not_flattened() {
        let target = "b".repeat(64);
        let old = (now() - chrono::Duration::seconds(STALE_AFTER_SECONDS + 1)).to_rfc3339();
        /// (label, optional (updated_at, digest) for the health document, expected)
        type Case = (&'static str, Option<(String, Option<String>)>, Serving);
        let cases: Vec<Case> = vec![
            ("missing", None, Serving::Missing),
            (
                "stale",
                Some((old, Some(target.clone()))),
                Serving::Stale {
                    age_seconds: Some(STALE_AFTER_SECONDS + 1),
                },
            ),
            (
                "unidentified",
                Some((NOW.to_string(), None)),
                Serving::Unidentified,
            ),
        ];
        for (label, setup, expected) in cases {
            let fixture = Fixture::new();
            fixture.put_pending(&pending(&target, vec!["danso.service".into()]));
            if let Some((at, sha)) = setup {
                write_health(&fixture.health, &at, sha.as_deref());
            }
            assert_eq!(
                fixture.run(),
                Activation::Unverified { serving: expected },
                "{label}: `activate --json` reports why it could not decide, \
                 and collapsing the cases makes that output useless"
            );
        }
    }

    #[test]
    fn a_resolved_records_exit_code_matches_its_outcome() {
        assert_eq!(
            Activation::AlreadyResolved {
                outcome: Outcome::Activated
            }
            .exit_code(),
            0
        );
        assert_eq!(
            Activation::AlreadyResolved {
                outcome: Outcome::Failed
            }
            .exit_code(),
            EXIT_NOT_ACTIVATED
        );
    }

    #[test]
    fn resolving_stamps_when_the_decision_was_made() {
        let fixture = Fixture::new();
        let target = "b".repeat(64);
        let record = pending(&target, vec!["danso.service".into()]);
        let before = record.updated_at.clone();
        fixture.put_pending(&record);
        write_health(&fixture.health, NOW, Some(&target));
        fixture.run();

        let after = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert_ne!(
            after.updated_at, before,
            "\"when was this decided\" is part of the record"
        );
    }

    #[test]
    fn a_cli_only_install_is_judged_by_the_installed_binary() {
        let fixture = Fixture::new();
        let installed = fixture.install("#!/bin/sh\necho new\n");
        // No services: there is no process to ask, and the installed file is
        // what every future invocation runs.
        fixture.put_pending(&pending(&installed, vec![]));
        assert!(!fixture.health.exists(), "and no health document exists");

        let outcome = fixture.run();
        assert_eq!(
            outcome,
            Activation::Activated {
                binary_sha256: installed
            }
        );
    }

    #[test]
    fn a_cli_only_install_that_did_not_land_fails() {
        let fixture = Fixture::new();
        fixture.install("#!/bin/sh\necho something else\n");
        fixture.put_pending(&pending(&"b".repeat(64), vec![]));
        assert_eq!(fixture.run().exit_code(), EXIT_NOT_ACTIVATED);
    }

    #[test]
    fn a_resolved_record_is_not_judged_again() {
        let fixture = Fixture::new();
        let target = "b".repeat(64);
        let mut record = pending(&target, vec!["danso.service".into()]);
        record.outcome = Outcome::Failed;
        fixture.put_pending(&record);
        // Evidence that would otherwise say "activated".
        write_health(&fixture.health, NOW, Some(&target));

        assert_eq!(
            fixture.run(),
            Activation::AlreadyResolved {
                outcome: Outcome::Failed
            },
            "a decision that was already recorded is not re-litigated by a \
             later run that happens to see different evidence"
        );
        let record = update::read_pending(&fixture.state()).unwrap().unwrap();
        assert_eq!(record.outcome, Outcome::Failed);
    }

    #[test]
    fn a_service_record_without_a_health_path_is_unverified_not_failed() {
        let fixture = Fixture::new();
        let target = "b".repeat(64);
        fixture.put_pending(&pending(&target, vec!["danso.service".into()]));
        // No service data directory could be resolved. That is a configuration
        // gap, not evidence that the replacement is not serving.
        let outcome = activate(&fixture.home, None, now()).unwrap();
        assert_eq!(outcome.exit_code(), EXIT_UNVERIFIED);
        assert!(
            update::read_pending(&fixture.state())
                .unwrap()
                .unwrap()
                .is_unresolved()
        );
    }

    #[test]
    fn health_evidence_is_ignored_for_a_cli_only_record() {
        let fixture = Fixture::new();
        let installed = fixture.install("#!/bin/sh\necho new\n");
        fixture.put_pending(&pending(&installed, vec![]));
        // A health document from some *other* danso on this host must not
        // decide a CLI-only installation's outcome.
        write_health(&fixture.health, NOW, Some(&"c".repeat(64)));

        assert_eq!(fixture.run().exit_code(), 0);
    }
}
