//! Runtime conformance suite (docs/unified-design.md §8): every
//! `TurnRunner` must produce the same normalized, body-free event sequence
//! over the same provider behavior. A loopback fake Anthropic server stands
//! in for the provider; no real network, credential or tool execution.
//!
//! Tests share process-wide environment (provider base URL, HOME), so they
//! serialize on one mutex and set the environment only while holding it.
use danso_runtime::{
    AgentEvent, DenyAll, ErrorCode, InProcessRunner, RunTemplate, SessionRequest, TurnInput,
    TurnRunner,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());

enum Reply {
    Text(String),
    Delayed(Duration, String),
    Status(u16, String),
}

struct FakeProvider {
    base: String,
    replies: Arc<Mutex<VecDeque<Reply>>>,
    seen: Arc<(Mutex<usize>, Condvar)>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl FakeProvider {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let replies: Arc<Mutex<VecDeque<Reply>>> = Arc::new(Mutex::new(replies.into()));
        let seen = Arc::new((Mutex::new(0usize), Condvar::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (r, s, q) = (replies.clone(), seen.clone(), requests.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let (r, s, q) = (r.clone(), s.clone(), q.clone());
                // One thread per connection: a delayed reply must not block
                // the next request, exactly like a real provider endpoint.
                std::thread::spawn(move || serve(stream, &r, &s, &q));
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            replies,
            seen,
            requests,
        }
    }

    fn push(&self, reply: Reply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    fn wait_for_requests(&self, count: usize, timeout: Duration) -> bool {
        let (seen, signal) = &*self.seen;
        let deadline = Instant::now() + timeout;
        let mut guard = seen.lock().unwrap();
        while *guard < count {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            guard = signal.wait_timeout(guard, deadline - now).unwrap().0;
        }
        true
    }
}

fn serve(
    mut stream: std::net::TcpStream,
    replies: &Mutex<VecDeque<Reply>>,
    seen: &(Mutex<usize>, Condvar),
    requests: &Mutex<Vec<Value>>,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    if let Ok(body) = serde_json::from_slice::<Value>(&buffer[header_end..]) {
        requests.lock().unwrap().push(body);
    }
    {
        let (count, signal) = seen;
        *count.lock().unwrap() += 1;
        signal.notify_all();
    }
    let reply = replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(Reply::Status(500, "unscripted".into()));
    let (status, body) = match reply {
        Reply::Text(text) => (200, message(&text)),
        Reply::Delayed(delay, text) => {
            std::thread::sleep(delay);
            (200, message(&text))
        }
        Reply::Status(status, body) => (status, body),
    };
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).ok();
}

fn message(text: &str) -> String {
    json!({
        "model": "fixture-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()
}

struct Fixture {
    _serial: MutexGuard<'static, ()>,
    _tmp: tempfile::TempDir,
    provider: FakeProvider,
    workspace: std::path::PathBuf,
    journals: std::path::PathBuf,
}

fn fixture(replies: Vec<Reply>) -> Fixture {
    let serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let workspace = tmp.path().join("workspace");
    let journals = tmp.path().join("journals");
    for dir in [&home, &workspace, &journals] {
        std::fs::create_dir(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let provider = FakeProvider::start(replies);
    // SAFETY: the serial mutex is held for the whole test, so no other test
    // thread reads the environment while it changes.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("ANTHROPIC_API_KEY", "synthetic-test-key");
        std::env::set_var("DANSO_ANTHROPIC_BASE_URL", &provider.base);
        std::env::remove_var("PIRI_BOOTSTRAP_CONTEXT_FILE");
        std::env::remove_var("DANSO_BOOTSTRAP_CONTEXT_FILE");
    }
    Fixture {
        _serial: serial,
        _tmp: tmp,
        provider,
        workspace,
        journals,
    }
}

fn template() -> RunTemplate {
    let mut template = RunTemplate::new("anthropic", "fixture-model");
    template.no_tools = true;
    template.provider_retries = 0;
    template.provider_timeout_seconds = 5;
    template.timeout_seconds = 30;
    template.max_turns = 3;
    template
}

fn collect(mut stream: danso_runtime::EventStream) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.blocking_recv() {
        events.push(event);
    }
    events
}

fn kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            AgentEvent::TextDelta { .. } => "text_delta",
            AgentEvent::MessageCompleted => "message_completed",
            AgentEvent::ReasoningDelta { .. } => "reasoning_delta",
            AgentEvent::ToolStarted { .. } => "tool_started",
            AgentEvent::ToolCompleted { .. } => "tool_completed",
            AgentEvent::ApprovalRequest { .. } => "approval_request",
            AgentEvent::ApprovalResolved { .. } => "approval_resolved",
            AgentEvent::TaskProgress { .. } => "task_progress",
            AgentEvent::Completion { .. } => "completion",
            AgentEvent::Result { .. } => "result",
            AgentEvent::Error { .. } => "error",
        })
        .collect()
}

fn runner(fx: &Fixture) -> InProcessRunner {
    InProcessRunner::new(&fx.journals, template()).unwrap()
}

#[test]
fn successful_turn_emits_text_boundary_result_completion_in_order() {
    let fx = fixture(vec![Reply::Text("done".into())]);
    let runner = runner(&fx);
    let session = runner
        .start_or_resume(SessionRequest::new(&fx.workspace))
        .unwrap();
    let id = session.session_id().to_string();
    assert!(danso_runtime::session::valid_session_id(&id));
    let events = collect(
        session
            .send_turn(TurnInput::Prompt("hello".into()), Arc::new(DenyAll))
            .unwrap(),
    );
    assert_eq!(
        kinds(&events),
        ["text_delta", "message_completed", "result", "completion"]
    );
    assert_eq!(events[0], AgentEvent::text_delta("done").unwrap());
    let AgentEvent::Result { result } = &events[2] else {
        panic!("missing result")
    };
    assert_eq!(result["text"], "done");
    assert_eq!(result["usage"]["requests"], 1);
    assert_eq!(result["usage"]["totalTokens"], 15);
    assert_eq!(events[3], AgentEvent::completion("stop").unwrap());
    // The journal is named by the session id, owner-only, and complete.
    let journal = fx.journals.join(format!("{id}.jsonl"));
    let meta = std::fs::metadata(&journal).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o600);
    danso::session::Session::open(&journal, &fx.workspace)
        .unwrap()
        .check_recovery()
        .unwrap();
    // The prompt reached the provider and nothing else leaked into events.
    let requests = fx.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].to_string().contains("hello"));
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("synthetic-test-key")
    );
}

#[test]
fn resume_reuses_the_exact_journal_and_rejects_unknown_ids() {
    let fx = fixture(vec![Reply::Text("one".into()), Reply::Text("two".into())]);
    let runner = runner(&fx);
    let first = runner
        .start_or_resume(SessionRequest::new(&fx.workspace))
        .unwrap();
    let id = first.session_id().to_string();
    collect(
        first
            .send_turn(TurnInput::Prompt("first".into()), Arc::new(DenyAll))
            .unwrap(),
    );
    drop(first);
    let mut request = SessionRequest::new(&fx.workspace);
    request.session_id = Some(id.clone());
    let second = runner.start_or_resume(request).unwrap();
    assert_eq!(second.session_id(), id);
    let events = collect(
        second
            .send_turn(TurnInput::Prompt("second".into()), Arc::new(DenyAll))
            .unwrap(),
    );
    assert_eq!(kinds(&events).last(), Some(&"completion"));
    let journal =
        danso::session::Session::open(&fx.journals.join(format!("{id}.jsonl")), &fx.workspace)
            .unwrap();
    let users = journal
        .messages()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .count();
    assert_eq!(users, 2);
    // Resume validates identity strictly: no guessed paths, no other spellings.
    let mut unknown = SessionRequest::new(&fx.workspace);
    unknown.session_id = Some(uuid::Uuid::new_v4().hyphenated().to_string());
    assert!(runner.start_or_resume(unknown).is_err());
    let mut malformed = SessionRequest::new(&fx.workspace);
    malformed.session_id = Some(id.to_uppercase());
    assert!(runner.start_or_resume(malformed).is_err());
}

#[test]
fn interrupt_during_provider_wait_is_cancelled_and_journal_stays_recoverable() {
    let fx = fixture(vec![Reply::Delayed(
        Duration::from_secs(20),
        "too late".into(),
    )]);
    let runner = runner(&fx);
    let session = runner
        .start_or_resume(SessionRequest::new(&fx.workspace))
        .unwrap();
    // Idle interrupt is a no-op.
    session.interrupt();
    assert!(!session.request_pause());
    let stream = session
        .send_turn(TurnInput::Prompt("slow".into()), Arc::new(DenyAll))
        .unwrap();
    assert!(fx.provider.wait_for_requests(1, Duration::from_secs(10)));
    let started = Instant::now();
    session.interrupt();
    let events = collect(stream);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancellation must not wait for the provider"
    );
    assert_eq!(kinds(&events), ["error"]);
    assert!(matches!(
        &events[0],
        AgentEvent::Error {
            code: ErrorCode::Cancelled,
            retryable: false,
            ..
        }
    ));
    // No provider response was acted on: the journal holds only the prompt
    // and recovery is clean, so the next turn can proceed.
    let journal = danso::session::Session::open(
        &fx.journals.join(format!("{}.jsonl", session.session_id())),
        &fx.workspace,
    )
    .unwrap();
    journal.check_recovery().unwrap();
    assert_eq!(journal.messages().unwrap().len(), 1);
    drop(journal);
    fx.provider.push(Reply::Text("after".into()));
    let events = collect(
        session
            .send_turn(TurnInput::Prompt("again".into()), Arc::new(DenyAll))
            .unwrap(),
    );
    assert_eq!(kinds(&events).last(), Some(&"completion"));
}

#[test]
fn provider_failure_maps_to_a_body_free_error() {
    let fx = fixture(vec![Reply::Status(
        503,
        json!({"error": {"message": "SECRET-PROVIDER-PROSE"}}).to_string(),
    )]);
    let runner = runner(&fx);
    let session = runner
        .start_or_resume(SessionRequest::new(&fx.workspace))
        .unwrap();
    let events = collect(
        session
            .send_turn(TurnInput::Prompt("hi".into()), Arc::new(DenyAll))
            .unwrap(),
    );
    assert_eq!(kinds(&events), ["error"]);
    let AgentEvent::Error { code, message, .. } = &events[0] else {
        unreachable!()
    };
    assert_eq!(*code, ErrorCode::Provider);
    assert!(message.contains("category=provider"), "{message}");
    assert!(message.contains("http_status=503"), "{message}");
    assert!(!message.contains("SECRET-PROVIDER-PROSE"));
    assert!(message.ends_with("No automatic replay."));
}

#[test]
fn concurrent_turns_on_one_session_are_refused_and_input_is_validated() {
    let fx = fixture(vec![Reply::Delayed(Duration::from_secs(3), "ok".into())]);
    let runner = runner(&fx);
    let session = runner
        .start_or_resume(SessionRequest::new(&fx.workspace))
        .unwrap();
    assert!(
        session
            .send_turn(TurnInput::Prompt("   ".into()), Arc::new(DenyAll))
            .is_err()
    );
    assert!(
        session
            .send_turn(TurnInput::ResumeTask, Arc::new(DenyAll))
            .is_err(),
        "resume requires long-task mode"
    );
    let first = session
        .send_turn(TurnInput::Prompt("one".into()), Arc::new(DenyAll))
        .unwrap();
    assert!(
        session
            .send_turn(TurnInput::Prompt("two".into()), Arc::new(DenyAll))
            .is_err()
    );
    session.interrupt();
    collect(first);
}

#[test]
fn runner_construction_and_session_requests_fail_closed() {
    let fx = fixture(vec![]);
    // Journal root must be private and outside the workspace.
    let open = fx._tmp.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(InProcessRunner::new(&open, template()).is_err());
    let inside = fx.workspace.join("journals");
    std::fs::create_dir(&inside).unwrap();
    std::fs::set_permissions(&inside, std::fs::Permissions::from_mode(0o700)).unwrap();
    let nested = InProcessRunner::new(&inside, template()).unwrap();
    assert!(
        nested
            .start_or_resume(SessionRequest::new(&fx.workspace))
            .is_err()
    );
    // Template validation.
    let mut bad = template();
    bad.provider = "unknown".into();
    assert!(InProcessRunner::new(&fx.journals, bad).is_err());
    let mut bad = template();
    bad.reasoning_effort = Some("high".into());
    assert!(
        InProcessRunner::new(&fx.journals, bad).is_err(),
        "anthropic refuses reasoning effort"
    );
    // Session request validation.
    let runner = runner(&fx);
    let mut other_model = SessionRequest::new(&fx.workspace);
    other_model.model = Some("other-model".into());
    assert!(runner.start_or_resume(other_model).is_err());
    let mut effort = SessionRequest::new(&fx.workspace);
    effort.effort = Some("high".into());
    assert!(runner.start_or_resume(effort).is_err());
    assert!(
        runner
            .start_or_resume(SessionRequest::new(fx._tmp.path().join("missing")))
            .is_err()
    );
    assert!(
        runner
            .start_or_resume(SessionRequest::new(Path::new("relative")))
            .is_err()
    );
}

#[test]
fn list_models_reports_the_configured_model_only() {
    let fx = fixture(vec![]);
    let models = runner(&fx).list_models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "fixture-model");
    assert!(models[0].is_default);
    assert!(models[0].supported_reasoning_efforts.is_empty());
    let mut glm = template();
    glm.provider = "glm".into();
    glm.reasoning_effort = Some("medium".into());
    let models = InProcessRunner::new(&fx.journals, glm)
        .unwrap()
        .list_models();
    assert_eq!(models[0].supported_reasoning_efforts.len(), 7);
    assert_eq!(
        models[0].default_reasoning_effort.as_deref(),
        Some("medium")
    );
}
