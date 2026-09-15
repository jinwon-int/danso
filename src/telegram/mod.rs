//! Telegram B1 foundation: Bot API transport, process-wide token ownership,
//! allowlist admission, and durable per-chat conversation pointers.
//!
//! This module is intentionally not connected to the agent runtime yet. The
//! later Telegram lane will own command handling and session/model wiring.
#![allow(dead_code)]

mod access;
mod client;
mod lock;
mod store;

pub use access::{AccessControl, Allowlist};
pub use client::{BotApi, Chat, Message, PollResult, Poller, Update, User};
pub use lock::TokenLock;
pub use store::{ConversationRecord, ConversationStore};

use anyhow::{Context, Result, ensure};
use std::{
    fmt,
    path::{Path, PathBuf},
};

pub const BOT_TOKEN_ENV: &str = "DANSO_TELEGRAM_BOT_TOKEN";
pub const ALLOWED_USER_IDS_ENV: &str = "DANSO_TELEGRAM_ALLOWED_USER_IDS";
pub const DATA_DIR_ENV: &str = "DANSO_TELEGRAM_DATA_DIR";
pub const DEFAULT_DATA_DIR_SUFFIX: &str = ".danso/telegram";

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
pub(crate) fn ensure_private_dir(path: &Path) -> Result<()> {
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

/// Environment-only configuration for the B1 foundation. There is no config
/// file or runtime/model selection here until the later RunTemplate lane.
#[derive(Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub allowed_user_ids: Allowlist,
    pub data_dir: PathBuf,
}

impl fmt::Debug for TelegramConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TelegramConfig")
            .field("bot_token", &"[redacted]")
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("data_dir", &self.data_dir)
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
        Ok(Self {
            bot_token,
            allowed_user_ids: Allowlist::from_env()?,
            data_dir: data_dir_from_env()?,
        })
    }
}

/// The four B1 pieces assembled for a future Telegram process entry point.
/// Holding the token lock in this value keeps the lock for the process
/// lifetime of the consumer.
pub struct TelegramFoundation {
    pub api: BotApi,
    pub access: AccessControl,
    pub conversations: ConversationStore,
    pub token_lock: TokenLock,
}

impl TelegramFoundation {
    pub fn from_env() -> Result<Self> {
        let config = TelegramConfig::from_env()?;
        let token_lock = TokenLock::acquire(&config.data_dir)?;
        let api = BotApi::new(&config.bot_token)?;
        let access = AccessControl::new(config.allowed_user_ids);
        let conversations = ConversationStore::new(&config.data_dir)?;
        Ok(Self {
            api,
            access,
            conversations,
            token_lock,
        })
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn token_lock_blocks_a_second_consumer() {
        let dir = tempfile::tempdir().unwrap();
        let first = TokenLock::acquire(dir.path()).unwrap();
        let second = TokenLock::acquire(dir.path()).unwrap_err();
        assert!(second.to_string().contains("already held"));
        drop(first);
        let _released = TokenLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn conversation_store_round_trips_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let expected = ConversationRecord::new(55, 19, Some("session-pointer".to_string()));
        let store = ConversationStore::new(dir.path()).unwrap();
        store.save(&expected).unwrap();
        let path = store.record_path(55);
        assert!(path.exists());
        drop(store);

        let restarted = ConversationStore::new(dir.path()).unwrap();
        assert_eq!(restarted.load(55).unwrap(), Some(expected));
    }
}
