use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use danso::{
    context,
    session::{Session, millis},
    tools::{self, Runner},
};
use serde_json::{Value, json};
use std::{io::Read, path::PathBuf, time::Duration};

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Json,
    Text,
}

#[derive(Parser)]
#[command(version, about = "Headless worker harness with Pi session interchange")]
struct Args {
    /// Prompt for this run. Use -- to separate a prompt beginning with '-'.
    prompt: String,
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// JSONL v3 path outside the workspace. Existing linear sessions resume.
    #[arg(long)]
    session: PathBuf,
    #[arg(long)]
    model: String,
    #[arg(long, value_enum, default_value = "json")]
    mode: Mode,
    /// Final answer only; equivalent to --mode text.
    #[arg(short = 'p', long)]
    print: bool,
    /// Allow reading project AGENTS.md and skill metadata for this invocation.
    #[arg(long)]
    trust_project: bool,
    /// Explicit opt-out from Linux bubblewrap isolation. For controlled tests only.
    #[arg(long)]
    unsafe_no_sandbox: bool,
    #[arg(long, default_value_t = 16)]
    max_turns: u32,
    #[arg(long, default_value_t = 300)]
    timeout_seconds: u64,
    #[arg(long, default_value_t = 30)]
    tool_timeout_seconds: u64,
}

#[derive(Default)]
struct Usage {
    attempted: bool,
    requests: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    models: Vec<String>,
}
impl Usage {
    fn add(&mut self, u: &Value, model: &str) {
        self.requests += 1;
        self.input += u["input_tokens"].as_u64().unwrap_or(0);
        self.output += u["output_tokens"].as_u64().unwrap_or(0);
        self.cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        self.cache_write += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let pair = format!("anthropic/{model}");
        if !self.models.contains(&pair) {
            self.models.push(pair);
        }
    }
    fn report(&self) {
        let summary = json!({"requests":self.requests,"inputTokens":self.input,"outputTokens":self.output,"cacheReadTokens":self.cache_read,"cacheWriteTokens":self.cache_write,"totalTokens":self.input+self.output+self.cache_read+self.cache_write,"costUsd":0,"models":self.models});
        eprintln!("DANSO_USAGE={summary}");
        eprintln!("PIRI_USAGE={summary}");
    }
}

fn provider_messages(messages: &[Value]) -> Result<Vec<Value>> {
    let mut result = Vec::<Value>::new();
    for m in messages {
        let (role, content) = match m["role"].as_str() {
            Some("user") => {
                let content = if let Some(text) = m["content"].as_str() {
                    json!([{"type":"text","text":text}])
                } else {
                    m["content"].clone()
                };
                ensure!(
                    content
                        .as_array()
                        .is_some_and(|blocks| blocks.iter().all(|b| b["type"] == "text")),
                    "v0 supports text user messages only"
                );
                ("user", content)
            }
            Some("assistant") => {
                let mut blocks = vec![];
                for b in m["content"]
                    .as_array()
                    .context("invalid assistant content")?
                {
                    match b["type"].as_str() {
                        Some("text") => blocks.push(b.clone()),
                        Some("toolCall") => blocks.push(json!({"type":"tool_use", "id":b["id"], "name":b["name"], "input":b["arguments"]})),
                        _ => bail!("unsupported assistant block in v0"),
                    }
                }
                ("assistant", json!(blocks))
            }
            Some("toolResult") => (
                "user",
                json!([{"type":"tool_result", "tool_use_id":m["toolCallId"], "content":m["content"], "is_error":m["isError"]}]),
            ),
            _ => bail!("unsupported message role in v0"),
        };
        if let Some(last) = result.last_mut().filter(|e| e["role"] == role) {
            last["content"].as_array_mut().unwrap().extend(
                content
                    .as_array()
                    .context("invalid message content")?
                    .iter()
                    .cloned(),
            );
        } else {
            result.push(json!({"role":role, "content":content}));
        }
    }
    Ok(result)
}

fn assistant(response: &Value, model: &str) -> Result<Value> {
    let mut content = vec![];
    for b in response["content"]
        .as_array()
        .context("provider response lacks content")?
    {
        match b["type"].as_str() {
            Some("text") => {
                ensure!(b["text"].is_string(), "invalid provider text");
                content.push(b.clone());
            }
            Some("tool_use") => {
                ensure!(
                    b["id"].is_string() && b["name"].is_string() && b["input"].is_object(),
                    "invalid provider tool call"
                );
                content.push(json!({"type":"toolCall", "id":b["id"], "name":b["name"], "arguments":b["input"]}));
            }
            _ => bail!("unsupported provider content block"),
        }
    }
    let stop = match response["stop_reason"].as_str() {
        Some("end_turn" | "stop_sequence") => "stop",
        Some("tool_use") => "toolUse",
        Some("max_tokens") => "length",
        _ => bail!("unsupported provider stop reason"),
    };
    ensure!(!content.is_empty(), "empty provider response");
    let has_calls = content.iter().any(|b| b["type"] == "toolCall");
    ensure!(
        has_calls == (stop == "toolUse"),
        "inconsistent provider stop reason"
    );
    let u = &response["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Ok(
        json!({"role":"assistant", "content":content,"api":"anthropic-messages","provider":"anthropic","model":model,"timestamp":millis(),"stopReason":stop,"usage":{"input":n("input_tokens"),"output":n("output_tokens"),"cacheRead":n("cache_read_input_tokens"),"cacheWrite":n("cache_creation_input_tokens"),"totalTokens":n("input_tokens")+n("output_tokens")+n("cache_read_input_tokens")+n("cache_creation_input_tokens"),"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}),
    )
}

fn emit(mode: Mode, entry: &Value) {
    if matches!(mode, Mode::Json) {
        println!("{entry}");
    }
}

async fn run(args: &Args, usage: &mut Usage) -> Result<()> {
    ensure!(
        (1..=128).contains(&args.max_turns),
        "max-turns must be 1..128"
    );
    ensure!(
        (1..=3600).contains(&args.timeout_seconds)
            && (1..=300).contains(&args.tool_timeout_seconds),
        "invalid timeout"
    );
    ensure!(
        !args.prompt.trim().is_empty() && args.prompt.len() <= context::CONTEXT_LIMIT,
        "prompt must be 1..65536 bytes"
    );
    ensure!(
        std::env::var_os("PIRI_BOOTSTRAP_CONTEXT_FILE").is_none()
            && std::env::var_os("DANSO_BOOTSTRAP_CONTEXT_FILE").is_none(),
        "wrapper bootstrap injection is unsupported; use discovered context exactly once"
    );
    let cwd = args
        .cwd
        .canonicalize()
        .context("workspace does not exist")?;
    ensure!(
        cwd.is_dir() && cwd.parent().is_some(),
        "workspace must be a non-root directory"
    );
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is required")?);
    let session_path = if args.session.is_absolute() {
        args.session.clone()
    } else {
        std::env::current_dir()?.join(&args.session)
    };
    let session_parent = session_path
        .parent()
        .context("missing session parent")?
        .canonicalize()
        .context("session parent must already exist")?;
    ensure!(
        !session_parent.starts_with(&cwd),
        "session must live outside writable workspace"
    );
    let ctx = context::discover(&cwd, &home, args.trust_project)?;
    let mut session = Session::open(&session_path, &cwd)?;
    session.check_recovery()?;
    let mut messages = session.messages()?;
    provider_messages(&messages)?;
    let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is required")?;
    ensure!(!key.is_empty(), "ANTHROPIC_API_KEY is empty");
    // Only HTTPS or literal loopback HTTP is allowed; never follow redirects
    // carrying a credential to a different host.
    let base = std::env::var("DANSO_ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".into());
    let url = reqwest::Url::parse(&format!("{}/v1/messages", base.trim_end_matches('/')))?;
    ensure!(
        url.scheme() == "https"
            || (url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))),
        "provider endpoint requires HTTPS (loopback HTTP is allowed for tests)"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "endpoint must not contain credentials"
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let runner = Runner {
        cwd,
        readable: ctx.readable,
        unsafe_no_sandbox: args.unsafe_no_sandbox,
        timeout: Duration::from_secs(args.tool_timeout_seconds),
    };
    let (_, failed) = runner
        .run(&json!({"name":"bash","arguments":{"command":"true"}}))
        .await?;
    ensure!(
        !failed,
        "sandbox preflight failed; no provider request sent"
    );
    let mode = if args.print { Mode::Text } else { args.mode };
    emit(mode, &session.entries[0]);
    let user = json!({"role":"user","content":args.prompt,"timestamp":millis()});
    emit(mode, &session.message(user.clone())?);
    messages.push(user);
    for _ in 0..args.max_turns {
        let body = json!({"model":args.model,"max_tokens":4096,"system":format!("You are a headless coding worker. Use only read, bash, edit, write. Skills are loaded using read.{}",ctx.prompt),"messages":provider_messages(&messages)?,"tools":tools::definitions()});
        ensure!(
            serde_json::to_vec(&body)?.len() <= 512 * 1024,
            "request context exceeds 512 KiB; start a new session"
        );
        usage.attempted = true;
        let response = client
            .post(url.clone())
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .context("provider request failed")?;
        ensure!(
            response.status().is_success(),
            "provider request failed: HTTP {}",
            response.status().as_u16()
        );
        // Bounded response accumulation, including chunked responses.
        let mut response = response;
        let mut bytes = vec![];
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 1024 * 1024,
                "provider response exceeds 1 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        let response: Value = serde_json::from_slice(&bytes)?;
        let response_model = response["model"].as_str().unwrap_or(&args.model);
        usage.add(&response["usage"], response_model);
        let message = assistant(&response, response_model)?;
        let calls: Vec<Value> = message["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "toolCall")
            .cloned()
            .collect();
        // Validate IDs across the entire transcript before any side effect.
        let mut ids = std::collections::HashSet::new();
        for m in messages.iter().chain(std::iter::once(&message)) {
            if m["role"] == "assistant" {
                for b in m["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|b| b["type"] == "toolCall")
                {
                    ensure!(
                        ids.insert(b["id"].as_str().context("invalid tool id")?),
                        "duplicate tool call id"
                    );
                }
            }
        }
        emit(mode, &session.message(message.clone())?);
        messages.push(message.clone());
        if calls.is_empty() {
            ensure!(
                message["stopReason"] == "stop",
                "provider response truncated"
            );
            if matches!(mode, Mode::Text) {
                for b in message["content"].as_array().unwrap() {
                    if let Some(text) = b["text"].as_str() {
                        println!("{text}");
                    }
                }
            }
            return Ok(());
        }
        for call in calls {
            let id = call["id"].as_str().unwrap();
            session.operation(id, "started")?;
            let (output, failed) = match runner.run(&call).await {
                Ok(result) => result,
                Err(e) => (e.to_string(), true),
            };
            let result = json!({"role":"toolResult","toolCallId":id,"toolName":call["name"],"content":[{"type":"text","text":output}],"isError":failed,"timestamp":millis()});
            emit(mode, &session.message(result.clone())?);
            session.operation(id, "settled")?;
            messages.push(result);
        }
    }
    bail!("turn budget exhausted")
}

async fn interrupted() -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
    tokio::select! { _ = int.recv() => 130, _ = term.recv() => 143, _ = hup.recv() => 129 }
}

// The tool worker must not initialize Tokio: RLIMIT_AS intentionally leaves
// room for a shell, not a multithreaded async runtime with many thread stacks.
fn main() {
    if std::env::args().nth(1).as_deref() == Some("__tool") {
        let result = (|| {
            let mut input = String::new();
            std::io::stdin()
                .take(1024 * 1024)
                .read_to_string(&mut input)?;
            tools::worker(serde_json::from_str(&input)?)
        })();
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(e) => {
            let code = e.exit_code();
            e.print().ok();
            if code != 0 {
                Usage::default().report();
            }
            std::process::exit(code);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut usage = Usage::default();
    let code = runtime.block_on(async {
        tokio::select! {
            code = interrupted() => { eprintln!("run interrupted"); code },
            result = tokio::time::timeout(Duration::from_secs(args.timeout_seconds), run(&args, &mut usage)) => {
                match result {
                    Ok(Ok(())) => 0,
                    Ok(Err(e)) => { eprintln!("{e:#}"); if usage.attempted { 3 } else { 2 } },
                    Err(_) => { eprintln!("run timed out"); 124 },
                }
            }
        }
    });
    usage.report();
    std::process::exit(code);
}
