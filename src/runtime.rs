//! Agent-loop policy. No CLI, environment lookup, HTTP client or shell spawning.
use crate::{
    contracts::{Event, EventSink, OperationState, SessionStore, ToolCall, ToolExecutor},
    failure::{Kind, at},
    provider::{ModelRequest, Provider},
    session::millis,
    usage::{Usage, UsageSnapshot},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

#[derive(Clone, Copy, Debug)]
pub struct LongTaskRun {
    pub limits: crate::long_task::Limits,
    /// Bit mask: wall=1, stage=2, requests=4, tokens=8, repeat=16.
    /// Resume may fill omitted fields from the immutable journal record.
    pub explicit_limits: u8,
    pub resume: bool,
    pub pause_after_stage: Option<u64>,
}

pub struct RunInput<'a> {
    pub no_tools: bool,
    pub prompt: &'a str,
    pub context: &'a str,
    pub execution_context: &'a str,
    pub max_turns: u32,
    pub compact_at_bytes: Option<usize>,
    /// Optional hook re-resolving the context string (e.g. per-request
    /// memory refresh, §5.3). Called only right after a compaction; the
    /// hook owns every policy — the runtime stays memory-agnostic.
    pub refresh_context: Option<&'a dyn Fn() -> Result<String>>,
    pub long_task: Option<LongTaskRun>,
    /// Set by the CLI's graceful SIGUSR1 handler. The runtime only observes
    /// this flag at settled boundaries; it never interrupts a provider or
    /// tool operation.
    pub pause_requested: Option<&'a AtomicBool>,
}

impl<'a> RunInput<'a> {
    /// The static form used by every non-refreshing caller.
    pub fn with_static_context(
        no_tools: bool,
        prompt: &'a str,
        context: &'a str,
        execution_context: &'a str,
        max_turns: u32,
        compact_at_bytes: Option<usize>,
    ) -> RunInput<'a> {
        RunInput {
            no_tools,
            prompt,
            context,
            execution_context,
            max_turns,
            compact_at_bytes,
            refresh_context: None,
            long_task: None,
            pause_requested: None,
        }
    }
}

#[derive(Clone, Copy)]
enum GateMode {
    Action,
    Summary,
}

/// Provider gate used only by long tasks.  It writes a provider reservation
/// before dispatch.  An action response stays pending until the runtime has
/// durably appended the assistant message; summary responses can be settled
/// immediately because they have no side effects.
struct LongTaskProvider<'a, P, S> {
    provider: &'a mut P,
    session: &'a mut S,
    ledger: &'a mut crate::long_task::Ledger,
    mode: GateMode,
    started: Instant,
    base_elapsed_ms: u64,
}

impl<'a, P, S> LongTaskProvider<'a, P, S> {
    fn new(
        provider: &'a mut P,
        session: &'a mut S,
        ledger: &'a mut crate::long_task::Ledger,
        mode: GateMode,
        started: Instant,
        base_elapsed_ms: u64,
    ) -> Self {
        Self {
            provider,
            session,
            ledger,
            mode,
            started,
            base_elapsed_ms,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.base_elapsed_ms
            .saturating_add(self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
    }
}

impl<P: Provider, S: SessionStore> Provider for LongTaskProvider<'_, P, S> {
    fn validate_history(&self, messages: &[Value]) -> Result<()> {
        self.provider.validate_history(messages)
    }

    fn request_bytes(&self, request: &ModelRequest<'_>) -> Result<usize> {
        self.provider.request_bytes(request)
    }

    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        if let Some(limits) = self.ledger.limits
            && (self.ledger.requests >= limits.max_requests
                || self.ledger.reported_tokens >= limits.max_tokens)
        {
            return Err(crate::failure::at(Kind::RequestBudget)(anyhow::anyhow!(
                "long-task budget exhausted"
            )));
        }
        let _sequence = self.ledger.begin_request(self.session, self.elapsed_ms())?;
        let before = usage.snapshot();
        let response = self.provider.complete(request, usage).await;
        let delta = token_delta(before, usage.snapshot())?;
        match response {
            Ok(response) => match self.mode {
                GateMode::Summary => {
                    self.ledger.settle_request(
                        self.session,
                        "summary",
                        self.elapsed_ms(),
                        delta,
                    )?;
                    if self
                        .ledger
                        .limits
                        .is_some_and(|limits| self.ledger.reported_tokens >= limits.max_tokens)
                    {
                        self.ledger.pause(self.session, "request_budget")?;
                        return Err(crate::failure::at(Kind::RequestBudget)(anyhow::anyhow!(
                            "long-task token budget exceeded"
                        )));
                    }
                    Ok(response)
                }
                GateMode::Action => {
                    let sequence = self
                        .ledger
                        .pending_sequence
                        .context("long-task action reservation disappeared")?;
                    self.ledger.pending_response = Some(crate::long_task::PendingResponse {
                        sequence,
                        elapsed_ms: self.elapsed_ms(),
                        tokens: delta,
                    });
                    Ok(response)
                }
            },
            Err(error) => {
                let category = match crate::failure::category(&error) {
                    Some(Kind::ProviderTimeout) => "provider_timeout",
                    _ => "provider",
                };
                self.ledger
                    .fail_request(self.session, category, self.elapsed_ms(), delta)
                    .map_err(crate::failure::at(Kind::Session))?;
                Err(error)
            }
        }
    }
}

fn token_delta(
    before: UsageSnapshot,
    after: UsageSnapshot,
) -> Result<crate::long_task::TokenDelta> {
    Ok(crate::long_task::TokenDelta {
        input: after
            .input
            .checked_sub(before.input)
            .context("provider usage moved backwards")?,
        output: after
            .output
            .checked_sub(before.output)
            .context("provider usage moved backwards")?,
        cache_read: after
            .cache_read
            .checked_sub(before.cache_read)
            .context("provider usage moved backwards")?,
        cache_write: after
            .cache_write
            .checked_sub(before.cache_write)
            .context("provider usage moved backwards")?,
    })
}

fn task_elapsed_ms(base_elapsed_ms: u64, started: Instant) -> u64 {
    base_elapsed_ms.saturating_add(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
}

fn fail_pending_action<S: SessionStore>(
    ledger: &mut crate::long_task::Ledger,
    session: &mut S,
    category: &str,
) -> Result<()> {
    let pending = ledger
        .pending_response
        .clone()
        .context("long-task action reservation disappeared")?;
    ledger.fail_request(session, category, pending.elapsed_ms, pending.tokens)?;
    ledger.pending_response = None;
    Ok(())
}

fn settle_pending_action<S: SessionStore>(
    ledger: &mut crate::long_task::Ledger,
    session: &mut S,
    response_kind: &str,
) -> Result<()> {
    let pending = ledger
        .pending_response
        .clone()
        .context("long-task action reservation disappeared")?;
    ledger.settle_request(session, response_kind, pending.elapsed_ms, pending.tokens)?;
    ledger.pending_response = None;
    Ok(())
}

/// Observe a graceful pause request only after the current operation has
/// settled. A paused ledger is already safe, so a second request simply keeps
/// the process at that boundary without appending another record.
fn pause_if_requested<S: SessionStore>(
    ledger: &mut crate::long_task::Ledger,
    session: &mut S,
    requested: Option<&AtomicBool>,
) -> Result<bool> {
    if !requested.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Ok(false);
    }
    match ledger.state {
        crate::long_task::State::Ready => {
            ledger.pause(session, "operator")?;
            Ok(true)
        }
        crate::long_task::State::Paused => Ok(true),
        _ => Ok(false),
    }
}

pub async fn run(
    input: RunInput<'_>,
    provider: &mut impl Provider,
    executor: &impl ToolExecutor,
    session: &mut impl SessionStore,
    sink: &mut impl EventSink,
    usage: &mut Usage,
) -> Result<()> {
    (|| {
        ensure!(
            (1..=128).contains(&input.max_turns),
            "max-turns must be 1..128"
        );
        ensure!(
            (input.long_task.as_ref().is_some_and(|task| task.resume)
                || !input.prompt.trim().is_empty())
                && input.prompt.len() <= crate::context::CONTEXT_LIMIT,
            "prompt must be 1..65536 bytes (resume-task takes no prompt)"
        );
        ensure!(
            input.context.len() <= crate::context::CONTEXT_LIMIT,
            "bootstrap/skills context exceeds 65536 bytes"
        );
        ensure!(
            input.execution_context.len() <= crate::context::EXECUTION_CONTEXT_LIMIT,
            "execution context exceeds 32768 bytes"
        );
        if let Some(limit) = input.compact_at_bytes {
            ensure!(
                (crate::compaction::MIN_THRESHOLD..=crate::compaction::MAX_THRESHOLD)
                    .contains(&limit),
                "compact-at-bytes must be 8192..393216"
            );
            ensure!(
                session.supports_compaction(),
                "session store does not support compaction"
            );
        }
        Ok::<(), anyhow::Error>(())
    })()
    .map_err(at(Kind::Configuration))?;
    session.check_recovery().map_err(at(Kind::Session))?;
    let task_id = session.header()["id"]
        .as_str()
        .context("invalid session id")?
        .to_owned();
    let mut long_ledger = crate::long_task::Ledger::from_records(
        &session.long_task_records().map_err(at(Kind::Session))?,
        Some(&task_id),
    )
    .map_err(at(Kind::Session))?;
    let mut create_long_task = false;
    if let Some(task) = input.long_task {
        ensure!(
            task.explicit_limits & !31 == 0,
            "invalid long-task limit mask"
        );
        task.limits.validate().map_err(at(Kind::Configuration))?;
        if long_ledger.has_task() {
            if task.resume {
                ensure!(
                    long_ledger.limits == Some(task.limits),
                    "resume-task limits must match the immutable task limits"
                );
                long_ledger
                    .ensure_safe_resume()
                    .map_err(at(Kind::Session))?;
                ensure!(
                    input.prompt.trim().is_empty(),
                    "resume-task takes no prompt"
                );
            } else {
                ensure!(
                    matches!(
                        long_ledger.state,
                        crate::long_task::State::Completed | crate::long_task::State::Failed
                    ),
                    "unfinished long-task requires explicit --resume-task"
                );
                ensure!(
                    !input.prompt.trim().is_empty(),
                    "new long-task requires a prompt"
                );
                create_long_task = true;
                long_ledger = crate::long_task::Ledger::default();
            }
        } else {
            ensure!(!task.resume, "resume-task requires an existing long-task");
            create_long_task = true;
        }
    } else if long_ledger.has_task()
        && !matches!(
            long_ledger.state,
            crate::long_task::State::Completed | crate::long_task::State::Failed
        )
    {
        bail!("unfinished long-task requires explicit --resume-task");
    }
    let task_started = Instant::now();
    let task_base_elapsed_ms = long_ledger.elapsed_ms;
    let mut ids = session.tool_call_ids().map_err(at(Kind::Session))?;
    let mut messages = session.messages().map_err(at(Kind::Session))?;
    provider
        .validate_history(&messages)
        .map_err(at(Kind::Provider))?;
    let definitions = if input.no_tools {
        Vec::new()
    } else {
        executor.preflight().await.map_err(at(Kind::Sandbox))?;
        executor.definitions()
    };
    let names = definitions
        .iter()
        .map(|d| d.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut base_system = format!(
        "You are a headless coding worker. Use only {names}. Skills are loaded using read. Prefer targeted line-range reads and searches over whole-file dumps. After compaction, continue from recorded progress; re-read only missing or changed information.{}{}",
        input.context, input.execution_context
    );
    sink.emit(Event::Session(session.header()))
        .map_err(at(Kind::Output))?;
    let mut created_user_entry_id = None;
    if !input.long_task.is_some_and(|task| task.resume) {
        let user = json!({"role":"user","content":input.prompt,"timestamp":millis()});
        let entry = session
            .append_message(user.clone())
            .map_err(at(Kind::Session))?;
        created_user_entry_id = entry["id"].as_str().map(str::to_owned);
        sink.emit(Event::Message(&entry))
            .map_err(at(Kind::Output))?;
        messages.push(user);
    }
    if create_long_task {
        let task = input.long_task.context("missing long-task configuration")?;
        let user_entry_id = created_user_entry_id
            .as_deref()
            .context("long-task creation requires a durable user prompt")?;
        session
            .record_long_task(task.limits.json(&task_id, user_entry_id))
            .map_err(at(Kind::Session))?;
        long_ledger = crate::long_task::Ledger::from_records(
            &session.long_task_records().map_err(at(Kind::Session))?,
            Some(&task_id),
        )
        .map_err(at(Kind::Session))?;
    }
    if input.long_task.is_some() {
        // This body-free handshake is emitted only after the user entry and
        // creation record are durable. CCC may send SIGUSR1 after observing
        // it; the runtime then pauses before dispatching the first request.
        let event = crate::long_task::progress_event("checkpoint", &long_ledger);
        sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
        if pause_if_requested(&mut long_ledger, session, input.pause_requested)
            .map_err(at(Kind::Session))?
        {
            let event = crate::long_task::progress_event("paused", &long_ledger);
            sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
            return Err(at(Kind::RequestBudget)(anyhow::anyhow!(
                "long-task paused at a settled boundary"
            )));
        }
    }
    let task_budget = input
        .long_task
        .and_then(|task| task.limits.max_requests.try_into().ok())
        .unwrap_or(input.max_turns);
    let mut remaining = if input.long_task.is_some() {
        task_budget.saturating_sub(long_ledger.requests as u32)
    } else {
        input.max_turns
    };
    let budget_total = if input.long_task.is_some() {
        task_budget
    } else {
        input.max_turns
    };
    let mut summary_requests = 0;
    while remaining > 0 {
        if let Some(task) = input.long_task {
            if pause_if_requested(&mut long_ledger, session, input.pause_requested)
                .map_err(at(Kind::Session))?
            {
                let event = crate::long_task::progress_event("paused", &long_ledger);
                sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
                return Err(at(Kind::RequestBudget)(anyhow::anyhow!(
                    "long-task paused at a settled boundary"
                )));
            }
            let elapsed = task_elapsed_ms(task_base_elapsed_ms, task_started);
            if elapsed >= task.limits.wall_seconds.saturating_mul(1000) {
                if long_ledger.state == crate::long_task::State::Ready {
                    long_ledger
                        .pause(session, "wall_timeout")
                        .map_err(at(Kind::Session))?;
                    let event = crate::long_task::progress_event("paused", &long_ledger);
                    sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
                }
                return Err(at(Kind::RunTimeout)(anyhow::anyhow!(
                    "long-task wall budget exhausted"
                )));
            }
            if long_ledger.requests >= task.limits.max_requests
                || long_ledger.reported_tokens >= task.limits.max_tokens
            {
                if long_ledger.state == crate::long_task::State::Ready {
                    long_ledger
                        .pause(session, "request_budget")
                        .map_err(at(Kind::Session))?;
                    let event = crate::long_task::progress_event("paused", &long_ledger);
                    sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
                }
                return Err(at(Kind::RequestBudget)(anyhow::anyhow!(
                    "long-task budget exhausted"
                )));
            }
        }
        let final_phase = budget_final_phase(
            input.long_task.is_some(),
            &long_ledger,
            remaining,
            budget_total,
        );
        let mut system = budget_system(
            &base_system,
            budget_total,
            remaining,
            summary_requests,
            final_phase,
        );
        async {
            if let Some(limit) = input.compact_at_bytes {
                let before = provider
                    .request_bytes(&ModelRequest {
                        system: &system,
                        messages: &messages,
                        tools: &definitions,
                    })
                    .map_err(at(Kind::Provider))?;
                if before > limit {
                    // The current request and static instructions cannot be summarized
                    // away. Reject impossible budgets before spending summary calls.
                    let latest = crate::compaction::latest_user(&messages)?;
                    let bare = provider
                        .request_bytes(&ModelRequest {
                            system: &system,
                            messages: std::slice::from_ref(&latest),
                            tools: &definitions,
                        })
                        .map_err(at(Kind::Provider))?;
                    let summary_budget = (limit / 8).min(crate::compaction::MAX_SUMMARY_BYTES);
                    ensure!(
                        bare + 2 * summary_budget + 512 <= limit,
                        "current request and instructions leave no compaction budget"
                    );
                    session.check_recovery().map_err(at(Kind::Session))?;
                    let before_summary = remaining;
                    let summary = if input.long_task.is_some() {
                        let mut gated = LongTaskProvider::new(
                            provider,
                            session,
                            &mut long_ledger,
                            GateMode::Summary,
                            task_started,
                            task_base_elapsed_ms,
                        );
                        crate::compaction::summarize(
                            &mut gated,
                            &messages,
                            limit,
                            &mut remaining,
                            usage,
                        )
                        .await?
                    } else {
                        crate::compaction::summarize(
                            provider,
                            &messages,
                            limit,
                            &mut remaining,
                            usage,
                        )
                        .await?
                    };
                    summary_requests += before_summary - remaining;
                    // Rebuild after all summary fragments/repairs, then size the
                    // exact instructions that the next action request will use.
                    if let Some(refresh) = input.refresh_context {
                        let refreshed = refresh().map_err(at(Kind::Configuration))?;
                        base_system = format!(
                            "You are a headless coding worker. Use only {names}. Skills are loaded using read. Prefer targeted line-range reads and searches over whole-file dumps. After compaction, continue from recorded progress; re-read only missing or changed information.{}{}",
                            refreshed, input.execution_context
                        );
                    }
                    let final_phase = budget_final_phase(
                        input.long_task.is_some(),
                        &long_ledger,
                        remaining,
                        budget_total,
                    );
                    system = budget_system(
                        &base_system,
                        budget_total,
                        remaining,
                        summary_requests,
                        final_phase,
                    );
                    let compacted = crate::compaction::checkpoint_messages(&summary, &messages)?;
                    let after = provider
                        .request_bytes(&ModelRequest {
                            system: &system,
                            messages: &compacted,
                            tools: &definitions,
                        })
                        .map_err(at(Kind::Provider))?;
                    ensure!(
                        after <= limit && after < before,
                        "compaction did not reduce request below threshold"
                    );
                    let entry = session
                        .record_compaction(summary)
                        .map_err(at(Kind::Session))?;
                    // Durable checkpoint before the next request; renderer failure
                    // also stops continuation, leaving a resumable journal.
                    sink.emit(Event::Compaction(&entry))
                        .map_err(at(Kind::Output))?;
                    messages = session.messages().map_err(at(Kind::Session))?;
                    ensure!(
                        messages == compacted,
                        "session store returned inconsistent compacted context"
                    );
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await
        .map_err(at(Kind::Compaction))?;
        remaining -= 1;
        if input.long_task.is_some()
            && pause_if_requested(&mut long_ledger, session, input.pause_requested)
                .map_err(at(Kind::Session))?
        {
            let event = crate::long_task::progress_event("paused", &long_ledger);
            sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
            return Err(at(Kind::RequestBudget)(anyhow::anyhow!(
                "long-task paused at a settled boundary"
            )));
        }
        let message = if input.long_task.is_some() {
            let mut gated = LongTaskProvider::new(
                provider,
                session,
                &mut long_ledger,
                GateMode::Action,
                task_started,
                task_base_elapsed_ms,
            );
            gated
                .complete(
                    ModelRequest {
                        system: &system,
                        messages: &messages,
                        tools: &definitions,
                    },
                    usage,
                )
                .await
        } else {
            provider
                .complete(
                    ModelRequest {
                        system: &system,
                        messages: &messages,
                        tools: &definitions,
                    },
                    usage,
                )
                .await
        }
        .map_err(at(Kind::Provider))?;
        let calls = (|| {
            ensure!(
                message["role"] == "assistant",
                "provider must return an assistant message"
            );
            let calls = tool_calls(&message)?;
            ensure!(
                !input.no_tools || calls.is_empty(),
                "tools are disabled for this invocation"
            );
            ensure!(
                !calls.is_empty()
                    || message["stopReason"] == "stop"
                    || message["stopReason"] == "length",
                "invalid terminal response"
            );
            ensure!(
                calls.is_empty() || message["stopReason"] == "toolUse",
                "invalid tool response"
            );
            Ok::<_, anyhow::Error>(calls)
        })();
        let calls = match calls {
            Ok(calls) => calls,
            Err(error) => {
                if input.long_task.is_some() {
                    fail_pending_action(&mut long_ledger, session, "provider")
                        .map_err(at(Kind::Session))?;
                }
                return Err(at(Kind::Provider)(error));
            }
        };
        // All adapters must pass this gate before any new side effect.
        for call in &calls {
            ensure!(ids.insert(call.id.clone()), "duplicate tool call id");
        }
        sink.emit(Event::Message(
            &session
                .append_message(message.clone())
                .map_err(at(Kind::Session))?,
        ))
        .map_err(at(Kind::Output))?;
        messages.push(message.clone());
        if input.long_task.is_some() {
            settle_pending_action(
                &mut long_ledger,
                session,
                if calls.is_empty() { "final" } else { "tools" },
            )
            .map_err(at(Kind::Session))?;
            if long_ledger
                .limits
                .is_some_and(|limits| long_ledger.reported_tokens > limits.max_tokens)
            {
                return Err(at(Kind::RequestBudget)(anyhow::anyhow!(
                    "long-task token budget exceeded"
                )));
            }
        }
        if calls.is_empty() {
            if message["stopReason"] != "stop" {
                // Terminal `length` with no tool calls (issue #69 B): the
                // response hit the configured output token cap. Keep the
                // provider category; the diagnostic carries the cap and the
                // flag that raises it.
                let cap = provider.max_output_tokens();
                return Err(at(Kind::Provider)(crate::failure::max_tokens_error(cap)));
            }
            if input.long_task.is_some() {
                long_ledger.complete(session).map_err(at(Kind::Session))?;
                let event = crate::long_task::progress_event("completed", &long_ledger);
                sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
            }
            sink.emit(Event::FinalAnswer(&message))
                .map_err(at(Kind::Output))?;
            return Ok(());
        }
        let mut batch = Vec::new();
        for call in calls {
            session
                .record_operation(&call.id, OperationState::Started)
                .map_err(at(Kind::Session))?;
            sink.emit(Event::ToolStarted(&call.name))
                .map_err(at(Kind::Output))?;
            let outcome = match executor.execute(&call).await {
                Ok(result) => result,
                Err(e) => crate::contracts::ToolOutcome {
                    output: e.to_string(),
                    is_error: true,
                },
            };
            batch.push((
                call.name.clone(),
                call.arguments.clone(),
                outcome.is_error,
                outcome.output.clone(),
            ));
            let result = json!({"role":"toolResult","toolCallId":call.id,"toolName":call.name,"content":[{"type":"text","text":outcome.output}],"isError":outcome.is_error,"timestamp":millis()});
            sink.emit(Event::Message(
                &session
                    .append_message(result.clone())
                    .map_err(at(Kind::Session))?,
            ))
            .map_err(at(Kind::Output))?;
            session
                .record_operation(&call.id, OperationState::Settled)
                .map_err(at(Kind::Session))?;
            sink.emit(Event::ToolSettled {
                is_error: outcome.is_error,
            })
            .map_err(at(Kind::Output))?;
            messages.push(result);
        }
        if input.long_task.is_some() {
            let fingerprint = crate::long_task::fingerprint_batch(&batch);
            let limits = long_ledger.limits.context("long-task was not created")?;
            let repeat_count = long_ledger
                .record_tool_batch(
                    session,
                    task_elapsed_ms(task_base_elapsed_ms, task_started),
                    &fingerprint,
                )
                .map_err(at(Kind::Session))?;
            if repeat_count >= limits.repeat_limit {
                long_ledger
                    .fail_terminal(session, "runtime")
                    .map_err(at(Kind::Session))?;
                let event = crate::long_task::progress_event("blocked", &long_ledger);
                sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
                return Err(at(Kind::Runtime)(anyhow::anyhow!(
                    "repeated identical tool batch reached safety limit"
                )));
            }
            if long_ledger.stage_requests >= limits.stage_requests
                || long_ledger.requests >= limits.max_requests
            {
                long_ledger
                    .checkpoint(
                        session,
                        task_elapsed_ms(task_base_elapsed_ms, task_started),
                        &fingerprint,
                    )
                    .map_err(at(Kind::Session))?;
                let event = crate::long_task::progress_event("checkpoint", &long_ledger);
                sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
                if input
                    .long_task
                    .and_then(|task| task.pause_after_stage)
                    .is_some_and(|target| long_ledger.stage >= target)
                {
                    long_ledger
                        .pause(session, "operator")
                        .map_err(at(Kind::Session))?;
                    let event = crate::long_task::progress_event("paused", &long_ledger);
                    sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
                    return Err(at(Kind::RequestBudget)(anyhow::anyhow!(
                        "long-task paused at a settled stage"
                    )));
                }
            }
            if pause_if_requested(&mut long_ledger, session, input.pause_requested)
                .map_err(at(Kind::Session))?
            {
                let event = crate::long_task::progress_event("paused", &long_ledger);
                sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
                return Err(at(Kind::RequestBudget)(anyhow::anyhow!(
                    "long-task paused at a settled boundary"
                )));
            }
        }
    }
    if input.long_task.is_some() && long_ledger.state == crate::long_task::State::Ready {
        long_ledger
            .pause(session, "request_budget")
            .map_err(at(Kind::Session))?;
        let event = crate::long_task::progress_event("paused", &long_ledger);
        sink.emit(Event::Task(&event)).map_err(at(Kind::Output))?;
        return Err(at(Kind::RequestBudget)(anyhow::anyhow!(
            "long-task request budget exhausted"
        )));
    }
    Err(at(Kind::RequestBudget)(anyhow::anyhow!(
        "turn budget exhausted"
    )))
}

/// Run-local guidance, never journal history: resume receives a fresh budget.
/// Two phases (issue #69 D): factual while `remaining` is above the reserve,
/// prioritize-and-report once it reaches the reserve window. Long-task runs
/// stay lenient until a stage boundary is near (`stage_final`).
fn budget_system(
    base: &str,
    total: u32,
    remaining: u32,
    summaries: u32,
    final_phase: bool,
) -> String {
    let guidance = if final_phase {
        format!(
            "Runtime request budget for this run: remaining={remaining}, total={total}, summary_requests={summaries}. Remaining includes this request; each model request consumes one slot, including checkpoint fragments and repair attempts. Future compaction also consumes these slots. This is a request limit, not a token or time allowance. Prioritize unfinished edits, required checks, and an accurate final report; avoid repeating reads unless information is missing or state changed. Reserve a request after tools to inspect their results and report. If remaining=1, there is no follow-up model request after any tools you call. Never skip required validation silently or claim unexecuted checks passed; report incomplete work and omitted checks honestly."
        )
    } else {
        format!(
            "Request budget: remaining={remaining} of {total} (summary_requests={summaries}). Each model request, including checkpoint fragments, consumes one. Use as many steps as the task needs; do not shortcut validation to save requests."
        )
    };
    format!("{base}\n{guidance}")
}

/// The reserve window (issue #69 C/D): the tail of the budget in which the
/// guidance switches to prioritize-and-report.
fn budget_reserve(total: u32) -> u32 {
    (total / 10).max(3)
}

/// Long-task runs stay lenient until a stage boundary is near; short runs
/// switch at the reserve window of the run budget (issue #69 D).
fn budget_final_phase(
    long_task: bool,
    ledger: &crate::long_task::Ledger,
    remaining: u32,
    total: u32,
) -> bool {
    if long_task {
        ledger.limits.is_some_and(|limits| {
            limits.stage_requests.saturating_sub(ledger.stage_requests)
                <= u64::from(budget_reserve(
                    limits.stage_requests.min(u32::MAX as u64) as u32
                ))
        })
    } else {
        remaining <= budget_reserve(total)
    }
}

pub(crate) fn tool_calls(message: &Value) -> Result<Vec<ToolCall>> {
    message["content"]
        .as_array()
        .context("invalid assistant content")?
        .iter()
        .filter(|b| b["type"] == "toolCall")
        .map(|b| {
            let call: ToolCall = serde_json::from_value(b.clone())?;
            ensure!(
                !call.id.is_empty() && !call.name.is_empty() && call.arguments.is_object(),
                "invalid tool call"
            );
            Ok(call)
        })
        .collect()
}

#[cfg(test)]
mod budget_guidance_tests {
    use super::*;

    #[test]
    fn reserve_is_ten_percent_with_a_floor_of_three() {
        assert_eq!(budget_reserve(16), 3);
        assert_eq!(budget_reserve(48), 4);
        assert_eq!(budget_reserve(100), 10);
    }

    #[test]
    fn short_run_is_lenient_above_reserve_and_final_within_it() {
        let base = "BASE";
        // 48-request budget: reserve is 4, so remaining=5 stays lenient.
        let lenient = budget_system(base, 48, 5, 1, false);
        assert!(lenient.starts_with("BASE\nRequest budget: remaining=5 of 48"));
        assert!(
            !lenient.contains("Prioritize"),
            "lenient phase must not rush the model"
        );
        // remaining=4 (at the reserve) switches to the prioritize-and-report text.
        let final_phase = budget_system(base, 48, 4, 1, true);
        assert!(final_phase.contains("Prioritize unfinished edits"));
        assert!(final_phase.contains("remaining=4"));
    }

    #[test]
    fn long_task_stays_lenient_until_a_stage_boundary_is_near() {
        let mut ledger = crate::long_task::Ledger {
            limits: Some(crate::long_task::Limits::defaults()),
            ..Default::default()
        };
        // Far from the 16-request stage boundary: lenient.
        ledger.stage_requests = 8;
        assert!(!budget_final_phase(true, &ledger, 900, 1024));
        // Within the reserve window of the stage boundary (16-3=13): final.
        ledger.stage_requests = 13;
        assert!(budget_final_phase(true, &ledger, 900, 1024));
        // Short runs ignore the ledger and use the run-budget reserve.
        ledger.stage_requests = 13;
        assert!(!budget_final_phase(false, &ledger, 40, 48));
        assert!(budget_final_phase(false, &ledger, 4, 48));
    }
}
