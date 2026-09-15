//! Loopback-only B1 service scenarios. No Telegram or provider network is
//! contacted; both HTTP surfaces are served by the fixtures below.

use danso::telegram::{Chat, ConversationStore, Message, TelegramService, Update, User};
use serde_json::Value;
use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::Notify;

#[derive(Clone, Debug)]
struct Request {
    path: String,
    body: Vec<u8>,
}

enum ServerMode {
    Bot {
        update: Option<String>,
    },
    Anthropic {
        release: Option<Arc<AtomicBool>>,
    },
}

struct LoopbackServer {
    base_url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl LoopbackServer {
    fn bot(update: Option<Update>) -> Self {
        Self::new(ServerMode::Bot {
            update: update
                .map(|update| serde_json::to_string(&update).expect("fixture update")),
        })
    }

    fn anthropic(release: Option<Arc<AtomicBool>>) -> Self {
        Self::new(ServerMode::Anthropic { release })
    }

    fn new(mode: ServerMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("configure loopback fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let join = thread::spawn(move || {
            let mut delivered_update = false;
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                let Some((path, body)) = read_request(&mut stream) else {
                    continue;
                };
                captured.lock().expect("fixture request lock").push(Request {
                    path: path.clone(),
                    body: body.clone(),
                });
                let response = match &mode {
                    ServerMode::Bot { update } if path.contains("/getUpdates") => {
                        let result = if !delivered_update {
                            delivered_update = true;
                            update.as_deref().map_or_else(
                                || "[]".to_string(),
                                |update| format!("[{update}]"),
                            )
                        } else {
                            "[]".to_string()
                        };
                        format!(r#"{{"ok":true,"result":{result}}}"#)
                    }
                    ServerMode::Bot { .. } => {
                        r#"{"ok":true,"result":{"message_id":999,"chat":{"id":42},"text":"sent"}}"#.to_string()
                    }
                    ServerMode::Anthropic { release } => {
                        if let Some(release) = release {
                            while !release.load(Ordering::Acquire)
                                && !stopping.load(Ordering::Acquire)
                            {
                                thread::sleep(Duration::from_millis(2));
                            }
                        }
                        r#"{"model":"fixture-model","content":[{"type":"text","text":"fixture answer"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}}"#.to_string()
                    }
                };
                write_response(&mut stream, &response);
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            stop,
            join: Some(join),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().expect("fixture request lock").len()
    }

    fn sent_texts(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("fixture request lock")
            .iter()
            .filter(|request| request.path.contains("/sendMessage"))
            .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
            .filter_map(|request| request["text"].as_str().map(str::to_owned))
            .collect()
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("fixture read timeout");
    let clone = stream.try_clone().expect("fixture stream clone");
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).ok()?;
    Some((path, body))
}

fn write_response(stream: &mut TcpStream, body: &str) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn update(update_id: i64, user_id: i64, text: &str) -> Update {
    Update {
        update_id,
        message: Some(Message {
            message_id: update_id,
            from: Some(User { id: user_id }),
            chat: Chat { id: 42 },
            date: None,
            text: Some(text.to_string()),
        }),
    }
}

struct Environment {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl Environment {
    fn new(provider: &str, bot: &LoopbackServer, model: &str, data_dir: &Path) -> Self {
        const NAMES: &[&str] = &[
            "HOME",
            "DANSO_TELEGRAM_BOT_TOKEN",
            "DANSO_TELEGRAM_ALLOWED_USER_IDS",
            "DANSO_TELEGRAM_DATA_DIR",
            "DANSO_TELEGRAM_API_BASE_URL",
            "DANSO_TELEGRAM_POLL_TIMEOUT_SECONDS",
            "DANSO_TELEGRAM_RETRIES",
            "DANSO_TELEGRAM_WORKSPACE",
            "DANSO_TELEGRAM_PROVIDER",
            "DANSO_TELEGRAM_MODEL",
            "DANSO_TELEGRAM_EFFORT",
            "DANSO_TELEGRAM_NO_TOOLS",
            "DANSO_TELEGRAM_MAX_TURNS",
            "DANSO_TELEGRAM_TIMEOUT_SECONDS",
            "DANSO_TELEGRAM_PROVIDER_TIMEOUT_SECONDS",
            "DANSO_TELEGRAM_TOOL_TIMEOUT_SECONDS",
            "DANSO_TELEGRAM_PROVIDER_RETRIES",
            "DANSO_TELEGRAM_MAX_OUTPUT_TOKENS",
            "DANSO_TELEGRAM_COMPACT_AT_BYTES",
            "DANSO_PROVIDER",
            "DANSO_MODEL",
            "DANSO_REASONING_EFFORT",
            "DANSO_TRUST_PROJECT",
            "DANSO_NO_TOOLS",
            "DANSO_TIMEOUT_SECONDS",
            "DANSO_PROVIDER_TIMEOUT_SECONDS",
            "DANSO_TOOL_TIMEOUT_SECONDS",
            "DANSO_PROVIDER_RETRIES",
            "DANSO_MAX_OUTPUT_TOKENS",
            "DANSO_COMPACT_AT_BYTES",
            "DANSO_PROVIDER_STREAM",
            "PIRI_BOOTSTRAP_CONTEXT_FILE",
            "DANSO_BOOTSTRAP_CONTEXT_FILE",
            "DANSO_HOME",
            "DANSO_ANTHROPIC_MODEL",
            "DANSO_ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "DANSO_OPENAI_MODEL",
            "DANSO_OPENAI_BASE_URL",
            "OPENAI_API_KEY",
            "DANSO_OPENAI_CODEX_MODEL",
            "DANSO_CHATGPT_AUTH_FILE",
            "DANSO_CHATGPT_BASE_URL",
            "DANSO_GLM_MODEL",
            "DANSO_GLM_BASE_URL",
            "DANSO_GLM_ENDPOINT",
            "DANSO_GLM_THINKING",
            "ZAI_API_KEY",
        ];
        let saved = NAMES
            .iter()
            .map(|name| (*name, env::var_os(name)))
            .collect();
        let environment = Self { saved };
        for name in NAMES {
            unsafe { env::remove_var(name) };
        }
        let workspace = data_dir.join("workspace");
        fs::create_dir_all(&workspace).expect("test workspace");
        unsafe {
            env::set_var("HOME", &workspace);
            env::set_var("DANSO_TELEGRAM_BOT_TOKEN", "TEST_TOKEN");
            env::set_var("DANSO_TELEGRAM_ALLOWED_USER_IDS", "42");
            env::set_var("DANSO_TELEGRAM_DATA_DIR", data_dir);
            env::set_var("DANSO_TELEGRAM_API_BASE_URL", &bot.base_url);
            env::set_var("DANSO_TELEGRAM_POLL_TIMEOUT_SECONDS", "0");
            env::set_var("DANSO_TELEGRAM_RETRIES", "0");
            env::set_var("DANSO_TELEGRAM_WORKSPACE", &workspace);
            env::set_var("DANSO_TELEGRAM_PROVIDER", provider);
            env::set_var("DANSO_TELEGRAM_MODEL", model);
            env::set_var("DANSO_TELEGRAM_NO_TOOLS", "1");
        }
        environment
    }

    fn set(&self, name: &str, value: impl AsRef<std::ffi::OsStr>) {
        unsafe { env::set_var(name, value) };
    }

    fn set_provider_base(&self, provider: &str, server: &LoopbackServer) {
        unsafe {
            match provider {
                "anthropic" => {
                    env::set_var("ANTHROPIC_API_KEY", "fixture-provider-key");
                    env::set_var("DANSO_ANTHROPIC_BASE_URL", &server.base_url);
                }
                "glm" => {
                    env::set_var("ZAI_API_KEY", "fixture-provider-key");
                    env::set_var("DANSO_GLM_BASE_URL", &server.base_url);
                }
                _ => panic!("fixture provider unsupported"),
            }
        }
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            unsafe {
                match value {
                    Some(value) => env::set_var(name, value),
                    None => env::remove_var(name),
                }
            }
        }
    }
}

fn environment_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().expect("env lock")
}

async fn wait_for<F>(timeout: Duration, mut condition: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "loopback fixture condition timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_message(server: &LoopbackServer, expected: &str) {
    wait_for(Duration::from_secs(5), || {
        server.sent_texts().iter().any(|text| text.contains(expected))
    })
    .await;
}

#[tokio::test]
async fn service_loop_answers_one_authorized_message_in_process() {
    let _lock = environment_lock();
    let root = tempfile::tempdir().expect("test state");
    let message = update(1, 42, "hello from Telegram");
    let bot = LoopbackServer::bot(Some(message));
    let provider = LoopbackServer::anthropic(None);
    let environment = Environment::new("anthropic", &bot, "fixture-model", root.path());
    environment.set_provider_base("anthropic", &provider);
    let service = TelegramService::from_env().expect("Telegram service config");
    let shutdown = Arc::new(Notify::new());
    let task = tokio::spawn(service.run_until(Arc::clone(&shutdown)));

    wait_for_message(&bot, "fixture answer").await;
    shutdown.notify_one();
    task.await.expect("service task join").expect("service loop");

    let record = ConversationStore::new(root.path())
        .expect("conversation store")
        .load(42)
        .expect("load conversation")
        .expect("conversation record");
    assert_eq!(record.last_update_id, 1);
    assert!(record.session_pointer.is_some());
    assert_eq!(record.provider.as_deref(), Some("anthropic"));
    assert_eq!(record.model.as_deref(), Some("fixture-model"));
    assert_eq!(record.last_turn_usage.as_ref().map(|usage| usage.requests), Some(1));
    let session = record.session_pointer.expect("session pointer");
    let journal = fs::read_to_string(root.path().join("journals").join(format!("{session}.jsonl")))
        .expect("read session journal");
    assert!(journal.contains(r#""role":"assistant""#));
    drop(environment);
}

#[tokio::test]
async fn new_command_replaces_the_chat_session_pointer() {
    let _lock = environment_lock();
    let root = tempfile::tempdir().expect("test state");
    let bot = LoopbackServer::bot(None);
    let provider = LoopbackServer::anthropic(None);
    let environment = Environment::new("anthropic", &bot, "fixture-model", root.path());
    environment.set_provider_base("anthropic", &provider);
    let service = TelegramService::from_env().expect("Telegram service config");

    service
        .handle_update(update(1, 42, "/new"))
        .await
        .expect("first /new");
    wait_for_message(&bot, "Started a new Danso session").await;
    let store = ConversationStore::new(root.path()).expect("conversation store");
    let first = store
        .load(42)
        .expect("load first record")
        .expect("first record")
        .session_pointer
        .expect("first pointer");

    service
        .handle_update(update(2, 42, "/new"))
        .await
        .expect("second /new");
    wait_for_sent_count(&bot, 2).await;
    let second = store
        .load(42)
        .expect("load second record")
        .expect("second record")
        .session_pointer
        .expect("second pointer");
    assert_ne!(first, second);
    drop(environment);
}

#[tokio::test]
async fn stop_cancels_an_in_flight_turn_without_a_replay_or_final_answer() {
    let _lock = environment_lock();
    let root = tempfile::tempdir().expect("test state");
    let bot = LoopbackServer::bot(None);
    let release = Arc::new(AtomicBool::new(false));
    let provider = LoopbackServer::anthropic(Some(Arc::clone(&release)));
    let environment = Environment::new("anthropic", &bot, "fixture-model", root.path());
    environment.set_provider_base("anthropic", &provider);
    let service = TelegramService::from_env().expect("Telegram service config");

    service
        .handle_update(update(1, 42, "wait for cancellation"))
        .await
        .expect("prompt update");
    wait_for(Duration::from_secs(5), || {
        provider
            .requests
            .lock()
            .expect("provider request lock")
            .iter()
            .any(|request| request.path.contains("/v1/messages"))
    })
    .await;
    service
        .handle_update(update(2, 42, "/stop"))
        .await
        .expect("stop update");
    wait_for_message(&bot, "Stopping the active turn").await;
    release.store(true, Ordering::Release);

    wait_for(Duration::from_secs(5), || {
        ConversationStore::new(root.path())
            .ok()
            .and_then(|store| store.load(42).ok().flatten())
            .is_some_and(|record| record.last_update_id == 2)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let texts = bot.sent_texts();
    assert_eq!(texts.iter().filter(|text| text.contains("fixture answer")).count(), 0);
    let record = ConversationStore::new(root.path())
        .expect("conversation store")
        .load(42)
        .expect("load conversation")
        .expect("conversation record");
    let session = record.session_pointer.expect("session pointer");
    let journal = fs::read_to_string(root.path().join("journals").join(format!("{session}.jsonl")))
        .expect("read session journal");
    assert!(journal.contains("wait for cancellation"));
    assert!(!journal.contains(r#""role":"assistant""#));
    drop(environment);
}

#[tokio::test]
async fn unauthorized_users_are_not_answered() {
    let _lock = environment_lock();
    let root = tempfile::tempdir().expect("test state");
    let bot = LoopbackServer::bot(None);
    let provider = LoopbackServer::anthropic(None);
    let environment = Environment::new("anthropic", &bot, "fixture-model", root.path());
    environment.set_provider_base("anthropic", &provider);
    let service = TelegramService::from_env().expect("Telegram service config");

    service
        .handle_update(update(1, 999, "private message"))
        .await
        .expect("unauthorized update");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(bot.request_count(), 0);
    assert!(!root.path().join("conversations").join("42.json").exists());
    drop(environment);
}

#[tokio::test]
async fn model_and_effort_overrides_survive_a_service_restart() {
    let _lock = environment_lock();
    let root = tempfile::tempdir().expect("test state");
    let bot = LoopbackServer::bot(None);
    let provider = LoopbackServer::anthropic(None);
    let environment = Environment::new("glm", &bot, "fixture-model", root.path());
    environment.set_provider_base("glm", &provider);
    let service = TelegramService::from_env().expect("first Telegram service config");
    service
        .handle_update(update(1, 42, "/model per-chat-model"))
        .await
        .expect("set model");
    service
        .handle_update(update(2, 42, "/effort high"))
        .await
        .expect("set effort");
    wait_for_sent_count(&bot, 2).await;
    drop(service);

    let bot_after_restart = LoopbackServer::bot(None);
    environment.set("DANSO_TELEGRAM_API_BASE_URL", &bot_after_restart.base_url);
    let restarted = TelegramService::from_env().expect("restarted Telegram service config");
    restarted
        .handle_update(update(3, 42, "/model"))
        .await
        .expect("show model");
    restarted
        .handle_update(update(4, 42, "/effort"))
        .await
        .expect("show effort");
    wait_for_message(&bot_after_restart, "per-chat-model").await;
    wait_for_message(&bot_after_restart, "high").await;
    let record = ConversationStore::new(root.path())
        .expect("conversation store")
        .load(42)
        .expect("load conversation")
        .expect("conversation record");
    assert_eq!(record.model.as_deref(), Some("per-chat-model"));
    assert_eq!(record.effort.as_deref(), Some("high"));
    drop(environment);
}

async fn wait_for_sent_count(server: &LoopbackServer, count: usize) {
    wait_for(Duration::from_secs(5), || server.sent_texts().len() >= count).await;
}
