//! Channel-neutral in-process turn runner (#213 ①).
//!
//! One chat turn runs on its own thread through `app::run`, with the session
//! journal named by a UUID pointer below the channel's `journals/`. Moved
//! verbatim from the Telegram service. The channel owns everything visible
//! to the user (progress messages, edits, commands); this module owns only the
//! journal files and the turn thread. `label` names the channel in error text
//! and the thread name, so the Telegram output is unchanged.

use super::{
    UsageRecord, ensure_private_dir, settings::RunSettings, settings::validate_effort,
    settings::validate_model,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc, oneshot};

/// The per-turn handles a channel keeps so it can stop, pause and observe the
/// turn. `on_settled` runs on the turn thread once the turn has finished —
/// before its result is sent — exactly where the Telegram service used to
/// mark its active turn completed.
pub(crate) struct TurnControls {
    pub(crate) cancel: Arc<Notify>,
    pub(crate) pause_requested: Arc<AtomicBool>,
    pub(crate) cancellation_reason: Arc<AtomicU8>,
    pub(crate) on_settled: Box<dyn FnOnce() + Send>,
}

struct TurnSink {
    label: &'static str,
    final_text: Option<String>,
    paused: bool,
    progress: mpsc::UnboundedSender<ProgressEvent>,
    tool_started: Option<(String, Instant)>,
}

impl TurnSink {
    fn new(progress: mpsc::UnboundedSender<ProgressEvent>, label: &'static str) -> Self {
        Self {
            label,
            final_text: None,
            paused: false,
            progress,
            tool_started: None,
        }
    }

    fn final_text(self) -> Result<String> {
        self.final_text.with_context(|| {
            format!(
                "completed {} turn did not produce a final answer",
                self.label
            )
        })
    }

    fn paused(&self) -> bool {
        self.paused
    }
}

impl crate::contracts::EventSink for TurnSink {
    fn emit(&mut self, event: crate::contracts::Event<'_>) -> Result<()> {
        match event {
            crate::contracts::Event::FinalAnswer(message) => {
                let mut text = crate::contracts::text_blocks(message).join("");
                if text.is_empty()
                    && let Some(content) = message["content"].as_str()
                {
                    text = content.to_string();
                }
                ensure!(!text.is_empty(), "final {} answer is empty", self.label);
                self.final_text = Some(text);
            }
            crate::contracts::Event::ToolStarted(name) => {
                self.tool_started = Some((safe_tool_name(name), Instant::now()));
            }
            crate::contracts::Event::ToolSettled { .. } => {
                if let Some((name, started)) = self.tool_started.take() {
                    let elapsed_seconds = started.elapsed().as_secs();
                    let _ = self.progress.send(ProgressEvent::ToolFinished {
                        name,
                        elapsed_seconds,
                    });
                }
            }
            crate::contracts::Event::Task(progress) if progress["state"] == "paused" => {
                self.paused = true;
            }
            _ => {}
        }
        Ok(())
    }
}

pub(crate) struct TurnOutcome {
    pub(crate) text: Option<String>,
    pub(crate) usage: UsageRecord,
    pub(crate) paused: bool,
}

pub(crate) enum ProgressEvent {
    ToolFinished { name: String, elapsed_seconds: u64 },
}

pub(crate) struct TurnHandle {
    pub(crate) receiver: oneshot::Receiver<Result<TurnOutcome>>,
    pub(crate) progress: mpsc::UnboundedReceiver<ProgressEvent>,
}

/// A root-owned in-process adapter. danso-runtime exposes the same
/// provider-neutral runner for external embedders; the root binary cannot
/// depend on that crate because that crate intentionally depends on this
/// core package. This adapter keeps the channel services on the same
/// app::run path without introducing a cyclic Cargo dependency.
pub(crate) struct InProcessRunner {
    journals: PathBuf,
    settings: RunSettings,
    label: &'static str,
}

impl InProcessRunner {
    pub(crate) fn new(
        journals: PathBuf,
        settings: RunSettings,
        label: &'static str,
    ) -> Result<Self> {
        ensure_private_dir(&journals, label)?;
        ensure!(
            !journals.starts_with(&settings.workspace),
            "{label} journals must be outside the workspace"
        );
        Ok(Self {
            journals,
            settings,
            label,
        })
    }

    fn journal_path(&self, session_id: &str) -> PathBuf {
        self.journals.join(format!("{session_id}.jsonl"))
    }

    pub(crate) fn require_session_file(&self, session_id: &str) -> Result<PathBuf> {
        let label = self.label;
        let parsed = uuid::Uuid::parse_str(session_id)
            .with_context(|| format!("invalid {label} session pointer"))?;
        ensure!(
            parsed.hyphenated().to_string() == session_id,
            "invalid {label} session pointer"
        );
        let path = self.journal_path(session_id);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("{label} session pointer has no journal"))?;
        ensure!(
            metadata.is_file(),
            "{label} session journal must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.nlink() == 1
                    && metadata.mode() & 0o077 == 0,
                "{label} session journal has unsafe ownership or permissions"
            );
        }
        Ok(path)
    }

    pub(crate) fn new_session(&self) -> Result<String> {
        for _ in 0..8 {
            let session_id = uuid::Uuid::new_v4().hyphenated().to_string();
            let path = self.journal_path(&session_id);
            if std::fs::symlink_metadata(&path).is_ok() {
                continue;
            }
            let session = crate::session::Session::open(&path, &self.settings.workspace)?;
            drop(session);
            return Ok(session_id);
        }
        bail!("could not allocate a {} session", self.label)
    }

    pub(crate) fn session_status(&self, session_id: &str) -> Result<serde_json::Value> {
        let journal = self.require_session_file(session_id)?;
        crate::session::Session::read_status(&journal)
    }

    /// Read only the two journal timestamps needed for `/history`. The
    /// parser never retains or returns message values, and malformed stamps
    /// become the fixed `unknown` label rather than being echoed.
    pub(crate) fn session_timestamps(&self, session_id: &str) -> Result<SessionTimestamps> {
        let journal = self.require_session_file(session_id)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(journal)?;
        ensure!(file.metadata()?.len() <= 16 * 1024 * 1024);
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut total_bytes = 0_u64;
        let mut started_at = None;
        let mut updated_at = None;
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                break;
            }
            total_bytes = total_bytes
                .checked_add(read as u64)
                .with_context(|| format!("{} session journal size overflow", self.label))?;
            ensure!(total_bytes <= 16 * 1024 * 1024);
            let entry: JournalTimestamp<'_> = serde_json::from_str(&line)?;
            let Some(raw) = entry.timestamp else {
                continue;
            };
            let timestamp = safe_timestamp(raw);
            if started_at.is_none() {
                started_at = Some(timestamp.clone());
            }
            updated_at = Some(timestamp);
        }
        Ok(SessionTimestamps {
            started_at: started_at.unwrap_or_else(|| "unknown".to_string()),
            updated_at: updated_at.unwrap_or_else(|| "unknown".to_string()),
        })
    }

    pub(crate) fn start_turn(
        &self,
        controls: TurnControls,
        session_id: String,
        model: String,
        effort: Option<String>,
        prompt: String,
        long_task: Option<crate::runtime::LongTaskRun>,
    ) -> Result<TurnHandle> {
        let label = self.label;
        let parsed = uuid::Uuid::parse_str(&session_id)
            .with_context(|| format!("invalid {label} session pointer"))?;
        ensure!(
            parsed.hyphenated().to_string() == session_id,
            "invalid {label} session pointer"
        );
        validate_model(&model, label)?;
        validate_effort(effort.as_deref(), &self.settings.provider, label)?;
        let journal = self.require_session_file(&session_id)?;
        let TurnControls {
            cancel,
            pause_requested,
            cancellation_reason,
            on_settled,
        } = controls;
        let config = self.settings.config(
            prompt,
            journal,
            model,
            effort,
            long_task,
            Some(pause_requested),
            cancellation_reason,
        );
        let (sender, receiver) = oneshot::channel();
        let (progress_sender, progress_receiver) = mpsc::unbounded_channel();
        let thread_name = format!(
            "danso-{}-turn-{}",
            label.to_ascii_lowercase(),
            &session_id[..8]
        );
        let _thread = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_turn(config, cancel, progress_sender, label)
                }))
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "{label} turn panicked; journal retained. No automatic replay."
                    ))
                });
                on_settled();
                let _ = sender.send(result);
            })
            .with_context(|| format!("could not start {label} turn"))?;
        Ok(TurnHandle {
            receiver,
            progress: progress_receiver,
        })
    }
}

pub(crate) struct SessionTimestamps {
    pub(crate) started_at: String,
    pub(crate) updated_at: String,
}

#[derive(serde::Deserialize)]
struct JournalTimestamp<'a> {
    #[serde(borrow)]
    timestamp: Option<&'a str>,
}

fn safe_timestamp(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|timestamp| {
            timestamp
                .with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

fn run_turn(
    config: crate::app::RunConfig,
    cancel: Arc<Notify>,
    progress: mpsc::UnboundedSender<ProgressEvent>,
    label: &'static str,
) -> Result<TurnOutcome> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .with_context(|| format!("could not start {label} turn runtime"))?;
    let mut usage = crate::usage::Usage::default();
    let mut sink = TurnSink::new(progress, label);
    let result = runtime.block_on(async {
        tokio::select! {
            biased;
            _ = cancel.notified() => Err(anyhow::anyhow!("{label} turn cancelled")),
            result = tokio::time::timeout(
                Duration::from_secs(config.timeout_seconds),
                crate::app::run(&config, &mut sink, &mut usage),
            ) => match result {
                Ok(result) => result,
                Err(_) => {
                    if let Some(reason) = &config.cancellation_reason {
                        let _ = reason.compare_exchange(
                            0,
                            3,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                    Err(anyhow::anyhow!("{label} turn timed out"))
                }
            },
        }
    });
    let paused = sink.paused();
    if !paused {
        result?;
    }
    let text = if paused {
        None
    } else {
        Some(sink.final_text()?)
    };
    let usage = UsageRecord::from_summary(&usage.summary())?;
    Ok(TurnOutcome {
        text,
        usage,
        paused,
    })
}

pub(crate) fn safe_tool_name(name: &str) -> String {
    let mut safe: String = name
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
        })
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
        .take(64)
        .collect();
    if safe.is_empty() {
        safe.push_str("unknown");
    }
    safe
}
