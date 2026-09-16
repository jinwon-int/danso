//! Telegram B3 service: Bot API transport, process-wide token ownership,
//! allowlist admission, durable per-chat conversation state, progress edits,
//! follow-up queueing, and the command/update loop.
#![allow(dead_code)]

mod access;
mod client;
mod lock;
mod service;
mod store;

pub use access::{AccessControl, Allowlist};
pub use client::{
    API_BASE_URL_ENV, BotApi, Chat, DEFAULT_API_BASE_URL, DEFAULT_POLL_TIMEOUT_SECONDS, Message,
    Update, User,
};
pub use lock::{TOKEN_LOCK_FILE_NAME, TokenLock};
pub use service::{TelegramArgs, TelegramService, run};
pub use store::{
    ActiveTaskRecord, ConversationRecord, ConversationStore, MAX_PREVIOUS_SESSIONS, UsageRecord,
};

use anyhow::{Context, Result, ensure};
use std::{
    fmt,
    path::{Path, PathBuf},
};

pub const BOT_TOKEN_ENV: &str = "DANSO_TELEGRAM_BOT_TOKEN";
pub const ALLOWED_USER_IDS_ENV: &str = "DANSO_TELEGRAM_ALLOWED_USER_IDS";
pub const DATA_DIR_ENV: &str = "DANSO_TELEGRAM_DATA_DIR";
pub const DEFAULT_DATA_DIR_SUFFIX: &str = ".danso/telegram";
pub const WORKSPACE_ENV: &str = "DANSO_TELEGRAM_WORKSPACE";
pub const PROVIDER_ENV: &str = "DANSO_TELEGRAM_PROVIDER";
pub const MODEL_ENV: &str = "DANSO_TELEGRAM_MODEL";
pub const EFFORT_ENV: &str = "DANSO_TELEGRAM_EFFORT";

/// Resolve the Telegram state root from process configuration.
pub fn data_dir_from_env() -> Result<PathBuf> {
    if let Some(raw) = std::env::var_os(DATA_DIR_ENV) {
        ensure!(!raw.is_empty(), "{DATA_DIR_ENV} must not be empty");
        let path = PathBuf::from(raw);
        ensure!(
            path.is_absolute(),
            "{DATA_DIR_ENV} must be an absolute path"
        );
        return Ok(path);
    }

    let home = std::env::var_os("HOME").context("HOME is required for Telegram state")?;
    ensure!(!home.is_empty(), "HOME must not be empty");
    let home = PathBuf::from(home);
    ensure!(home.is_absolute(), "HOME must be an absolute path");
    Ok(home.join(DEFAULT_DATA_DIR_SUFFIX))
}

/// Make a private, owner-only directory and fail closed on an unsafe existing
/// directory. This is kept here so the lock and store share one filesystem
/// boundary without coupling Telegram to the memory module.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "Telegram data paths must be absolute");
    let missing = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.is_dir(), "Telegram data path must be a directory");
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };

    if missing {
        std::fs::create_dir_all(path)
            .with_context(|| format!("create Telegram data directory: {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir(), "Telegram data path must be a directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Telegram data directory must be owned by the current user"
        );
        ensure!(
            metadata.mode() & 0o777 == 0o700,
            "Telegram data directory must have mode 0700"
        );
    }
    Ok(())
}

/// Environment-only configuration for the Telegram service. Provider settings
/// remain in the normal Danso environment; Telegram-specific values only
/// select the service endpoint, workspace, and per-process defaults.
#[derive(Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub allowed_user_ids: Allowlist,
    pub data_dir: PathBuf,
    pub api_base_url: String,
    pub poll_timeout_seconds: u64,
    pub retries: u32,
}

impl fmt::Debug for TelegramConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TelegramConfig")
            .field("bot_token", &"[redacted]")
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("data_dir", &"[redacted]")
            .field("api_base_url", &"[redacted]")
            .field("poll_timeout_seconds", &self.poll_timeout_seconds)
            .field("retries", &self.retries)
            .finish()
    }
}

impl TelegramConfig {
    pub fn from_env() -> Result<Self> {
        let bot_token =
            std::env::var(BOT_TOKEN_ENV).context("DANSO_TELEGRAM_BOT_TOKEN is required")?;
        ensure!(
            !bot_token.trim().is_empty(),
            "DANSO_TELEGRAM_BOT_TOKEN must not be empty"
        );
        let api_base_url = std::env::var(client::API_BASE_URL_ENV)
            .unwrap_or_else(|_| client::DEFAULT_API_BASE_URL.to_string());
        let poll_timeout_seconds = parse_u64_env(
            client::POLL_TIMEOUT_ENV,
            client::DEFAULT_POLL_TIMEOUT_SECONDS,
            300,
        )?;
        let retries = parse_u32_env(client::RETRIES_ENV, client::RETRIES_DEFAULT, 5)?;
        Ok(Self {
            bot_token,
            allowed_user_ids: Allowlist::from_env()?,
            data_dir: data_dir_from_env()?,
            api_base_url,
            poll_timeout_seconds,
            retries,
        })
    }
}

fn parse_u64_env(name: &str, default: u64, max: u64) -> Result<u64> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(default);
    };
    let raw = raw.to_string_lossy();
    ensure!(!raw.trim().is_empty(), "{name} must not be empty");
    let value = raw
        .trim()
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("{name} must be an integer"))?;
    ensure!(value <= max, "{name} must be 0..={max}");
    Ok(value)
}

fn parse_u32_env(name: &str, default: u32, max: u32) -> Result<u32> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(default);
    };
    let raw = raw.to_string_lossy();
    ensure!(!raw.trim().is_empty(), "{name} must not be empty");
    let value = raw
        .trim()
        .parse::<u32>()
        .map_err(|_| anyhow::anyhow!("{name} must be an integer"))?;
    ensure!(value <= max, "{name} must be 0..={max}");
    Ok(value)
}

/// The transport, admission, store, and process-lifetime lock assembled for
/// the Telegram service. Holding the token lock in this value keeps ownership
/// for the lifetime of the consumer.
pub struct TelegramFoundation {
    pub api: BotApi,
    pub access: AccessControl,
    pub conversations: ConversationStore,
    pub token_lock: TokenLock,
    pub poll_timeout_seconds: u64,
}

impl TelegramFoundation {
    pub fn from_env() -> Result<Self> {
        Self::from_config(TelegramConfig::from_env()?)
    }

    pub fn from_config(config: TelegramConfig) -> Result<Self> {
        let token_lock = TokenLock::acquire(&config.data_dir)?;
        let api = BotApi::with_base_url(&config.bot_token, &config.api_base_url)?
            .with_retries(config.retries)?;
        let access = AccessControl::new(config.allowed_user_ids);
        let conversations = ConversationStore::new(&config.data_dir)?;
        Ok(Self {
            api,
            access,
            conversations,
            token_lock,
            poll_timeout_seconds: config.poll_timeout_seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::store::{ConversationRecord, UsageRecord};
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    #[derive(Clone)]
    struct Response {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    }

    impl Response {
        fn new(status: u16, body: impl AsRef<[u8]>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.as_ref().to_vec(),
            }
        }

        fn with_retry_after(mut self, seconds: u64) -> Self {
            self.headers.push(("Retry-After", seconds.to_string()));
            self
        }
    }

    #[derive(Clone, Debug)]
    struct SeenRequest {
        path: String,
        body: Vec<u8>,
    }

    struct FakeBotApi {
        base_url: String,
        requests: Arc<Mutex<Vec<SeenRequest>>>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl FakeBotApi {
        fn new(responses: Vec<Response>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let join = thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let (path, body) = read_request(&stream);
                    captured.lock().unwrap().push(SeenRequest { path, body });
                    write_response(&mut stream, &response);
                }
            });
            Self {
                base_url: format!("http://127.0.0.1:{}", address.port()),
                requests,
                join: Some(join),
            }
        }

        fn finish(mut self) -> Vec<SeenRequest> {
            self.join.take().unwrap().join().unwrap();
            self.requests.lock().unwrap().clone()
        }
    }

    fn read_request(stream: &TcpStream) -> (String, Vec<u8>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let path = request_line.split_whitespace().nth(1).unwrap().to_string();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        (path, body)
    }

    fn write_response(stream: &mut TcpStream, response: &Response) {
        let reason = match response.status {
            200 => "OK",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Test Response",
        };
        let mut headers = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            reason,
            response.body.len()
        );
        for (name, value) in &response.headers {
            headers.push_str(&format!("{name}: {value}\r\n"));
        }
        headers.push_str("\r\n");
        stream.write_all(headers.as_bytes()).unwrap();
        stream.write_all(&response.body).unwrap();
        stream.flush().unwrap();
    }

    fn ok(body: &str) -> Response {
        Response::new(200, body)
    }

    #[tokio::test]
    async fn poll_message_reply_advances_offset() {
        let first = ok(
            r#"{"ok":true,"result":[{"update_id":10,"message":{"message_id":1,"from":{"id":42},"chat":{"id":-100},"text":"hello"}}]}"#,
        );
        let sent = ok(r#"{"ok":true,"result":{"message_id":2,"chat":{"id":-100},"text":"reply"}}"#);
        let empty = ok(r#"{"ok":true,"result":[]}"#);
        let server = FakeBotApi::new(vec![first, sent, empty]);
        let client = BotApi::with_base_url("TEST_TOKEN", &server.base_url)
            .unwrap()
            .with_retries(1)
            .unwrap();
        let mut poller = client.poller(None, 0).unwrap();
        let updates = poller.next().await.unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(poller.offset(), Some(11));

        let access = AccessControl::new(Allowlist::from_ids([42]));
        let chat_id = access.authorized_chat_id(&updates[0]).unwrap();
        let reply = client.send_message(chat_id, "reply").await.unwrap();
        assert_eq!(reply.chat.id, -100);

        assert!(poller.next().await.unwrap().is_empty());
        let requests = server.finish();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].path.contains("/botTEST_TOKEN/getUpdates"));
        assert!(requests[1].path.contains("/botTEST_TOKEN/sendMessage"));
        assert!(
            requests[1]
                .body
                .windows(b"chat_id".len())
                .any(|window| window == b"chat_id")
        );
        assert!(requests[2].path.contains("offset=11"));
    }

    #[tokio::test]
    async fn rate_limit_retries_with_bounded_backoff_then_recovers() {
        let limited = Response::new(
            429,
            r#"{"ok":false,"error_code":429,"description":"retry"}"#,
        )
        .with_retry_after(1);
        let server = FakeBotApi::new(vec![limited, ok(r#"{"ok":true,"result":[]}"#)]);
        let client = BotApi::with_base_url("TEST_TOKEN", &server.base_url)
            .unwrap()
            .with_retries(1)
            .unwrap();
        assert!(client.get_updates(None, 0).await.unwrap().is_empty());
        let requests = server.finish();
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn rate_limit_during_a_reply_recovers_without_duplicate_body_parts() {
        let limited = Response::new(
            429,
            r#"{"ok":false,"error_code":429,"description":"retry"}"#,
        )
        .with_retry_after(0);
        let sent = ok(r#"{"ok":true,"result":{"message_id":2,"chat":{"id":-100},"text":"reply"}}"#);
        let server = FakeBotApi::new(vec![limited, sent]);
        let client = BotApi::with_base_url("TEST_TOKEN", &server.base_url)
            .unwrap()
            .with_retries(1)
            .unwrap();
        client.send_message(-100, "reply").await.unwrap();
        let requests = server.finish();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.path.contains("/botTEST_TOKEN/sendMessage"))
        );
    }

    #[tokio::test]
    async fn progress_edits_use_the_same_message_id() {
        let initial =
            ok(r#"{"ok":true,"result":{"message_id":7,"chat":{"id":-100},"text":"working"}}"#);
        let edited =
            ok(r#"{"ok":true,"result":{"message_id":7,"chat":{"id":-100},"text":"done"}}"#);
        let server = FakeBotApi::new(vec![initial, edited]);
        let client = BotApi::with_base_url("TEST_TOKEN", &server.base_url).unwrap();
        let message = client.send_message(-100, "working").await.unwrap();
        client
            .edit_message_text(-100, message.message_id, "done")
            .await
            .unwrap();
        let requests = server.finish();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].path.contains("/sendMessage"));
        assert!(requests[1].path.contains("/editMessageText"));
        assert!(String::from_utf8_lossy(&requests[1].body).contains("\"message_id\":7"));
    }

    #[tokio::test]
    async fn unauthorized_update_is_rejected_without_an_api_reply() {
        let update = ok(
            r#"{"ok":true,"result":[{"update_id":3,"message":{"message_id":1,"from":{"id":999},"chat":{"id":55},"text":"private"}}]}"#,
        );
        let server = FakeBotApi::new(vec![update]);
        let client = BotApi::with_base_url("TEST_TOKEN", &server.base_url).unwrap();
        let updates = client.get_updates(None, 0).await.unwrap();
        let access = AccessControl::new(Allowlist::default());
        assert!(!access.authorize(&updates[0]));
        assert!(access.authorized_chat_id(&updates[0]).is_none());
        let requests = server.finish();
        assert_eq!(requests.len(), 1, "rejected updates must not send a reply");
    }

    /// tempdir's directory mode follows the process umask on some platforms;
    /// the data-dir invariant is exactly 0700, so pin it for the test.
    #[cfg(unix)]
    fn force_private_mode(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(not(unix))]
    fn force_private_mode(_path: &std::path::Path) {}

    #[test]
    fn token_lock_blocks_a_second_consumer() {
        let dir = tempfile::tempdir().unwrap();
        force_private_mode(dir.path());
        let first = TokenLock::acquire(dir.path()).unwrap();
        let second = TokenLock::acquire(dir.path()).unwrap_err();
        assert!(second.to_string().contains("already held"));
        drop(first);
        let _released = TokenLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn conversation_store_round_trips_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        force_private_mode(dir.path());
        let expected = ConversationRecord::new(55, 19, Some("session-pointer".to_string()));
        let store = ConversationStore::new(dir.path()).unwrap();
        store.save(&expected).unwrap();
        let path = store.record_path(55);
        assert!(path.exists());
        drop(store);

        let restarted = ConversationStore::new(dir.path()).unwrap();
        assert_eq!(restarted.load(55).unwrap(), Some(expected));
    }

    #[test]
    fn poll_offset_round_trips_monotonically_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        force_private_mode(dir.path());
        let store = ConversationStore::new(dir.path()).unwrap();
        assert_eq!(store.load_poll_offset().unwrap(), None);
        store.save_poll_offset(12).unwrap();
        assert_eq!(store.load_poll_offset().unwrap(), Some(12));
        store.save_poll_offset(13).unwrap();
        assert!(store.save_poll_offset(11).is_err());
        drop(store);
        let restarted = ConversationStore::new(dir.path()).unwrap();
        assert_eq!(restarted.load_poll_offset().unwrap(), Some(13));
    }

    #[test]
    fn old_conversation_records_read_with_new_defaults() {
        let dir = tempfile::tempdir().unwrap();
        force_private_mode(dir.path());
        let store = ConversationStore::new(dir.path()).unwrap();
        let path = store.record_path(55);
        std::fs::write(
            &path,
            br#"{"chat_id":55,"last_update_id":19,"session_pointer":"old-session"}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let record = store.load(55).unwrap().unwrap();
        assert_eq!(record.provider, None);
        assert_eq!(record.model, None);
        assert_eq!(record.effort, None);
        assert_eq!(record.last_turn_usage, None);
        assert!(record.usage.is_zero());
    }

    #[test]
    fn session_history_is_bounded_and_resume_swaps_the_head() {
        let mut record = ConversationRecord::new(55, 19, Some("session-0".to_string()));
        for index in 1..=7 {
            record.remember_current_session();
            record.session_pointer = Some(format!("session-{index}"));
        }
        assert_eq!(record.previous_sessions.len(), 5);
        assert_eq!(
            record.previous_sessions,
            vec![
                "session-6".to_string(),
                "session-5".to_string(),
                "session-4".to_string(),
                "session-3".to_string(),
                "session-2".to_string(),
            ]
        );
        let previous = record.swap_previous_session().expect("previous session");
        assert_eq!(previous, "session-6");
        assert_eq!(record.session_pointer.as_deref(), Some("session-6"));
        assert_eq!(record.previous_sessions[0], "session-7");
        let store_dir = tempfile::tempdir().unwrap();
        force_private_mode(store_dir.path());
        ConversationStore::new(store_dir.path())
            .unwrap()
            .save(&record)
            .unwrap();
    }

    #[test]
    fn active_long_task_metadata_survives_record_round_trip() {
        let mut record = ConversationRecord::new(55, 19, Some("session-pointer".to_string()));
        record.mark_long_task_active(
            "session-pointer".to_string(),
            "2026-09-15T00:00:00.000Z".to_string(),
        );
        let encoded = serde_json::to_vec(&record).unwrap();
        let decoded: ConversationRecord = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.active_task, record.active_task);
        assert!(decoded.turn_active);
    }

    #[test]
    fn completed_usage_is_aggregated_and_serialized_without_provider_text() {
        let mut record = ConversationRecord::new(55, 19, None);
        let usage = UsageRecord {
            requests: 1,
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        };
        record
            .record_turn("anthropic", "fixture-model", None, usage.clone())
            .unwrap();
        record
            .record_turn("anthropic", "fixture-model", None, usage)
            .unwrap();
        assert_eq!(record.usage.requests, 2);
        assert_eq!(record.usage.total_tokens, 30);
        assert_eq!(record.last_turn_usage.as_ref().unwrap().total_tokens, 15);
        let encoded = serde_json::to_string(&record).unwrap();
        assert!(encoded.contains("\"model\":\"fixture-model\""));
        assert!(encoded.contains("\"totalTokens\":30"));
    }
}
