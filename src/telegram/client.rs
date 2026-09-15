use super::BOT_TOKEN_ENV;
use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, RequestBuilder, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, time::Duration};

pub const DEFAULT_API_BASE_URL: &str = "https://api.telegram.org";
pub const API_BASE_URL_ENV: &str = "DANSO_TELEGRAM_API_BASE_URL";
pub const DEFAULT_POLL_TIMEOUT_SECONDS: u64 = 25;
pub const POLL_TIMEOUT_ENV: &str = "DANSO_TELEGRAM_POLL_TIMEOUT_SECONDS";
pub const RETRIES_ENV: &str = "DANSO_TELEGRAM_RETRIES";
pub const RETRIES_DEFAULT: u32 = 3;
pub const RETRIES_MAX: u32 = 5;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Minimal Telegram Bot API adapter for the Telegram service. The token is retained only for
/// constructing the Bot API path and is never included in returned errors.
#[derive(Clone)]
pub struct BotApi {
    client: Client,
    base_url: Url,
    token: String,
    retries: u32,
}

impl BotApi {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        Self::with_base_url(token, DEFAULT_API_BASE_URL)
    }

    pub fn from_env() -> Result<Self> {
        let token = std::env::var(BOT_TOKEN_ENV).context("DANSO_TELEGRAM_BOT_TOKEN is required")?;
        let base =
            std::env::var(API_BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_API_BASE_URL.to_string());
        let retries = match std::env::var(RETRIES_ENV) {
            Ok(raw) if !raw.trim().is_empty() => raw
                .trim()
                .parse::<u32>()
                .map_err(|_| anyhow::anyhow!("{RETRIES_ENV} must be an integer"))?,
            Ok(_) | Err(std::env::VarError::NotPresent) => RETRIES_DEFAULT,
            Err(error) => return Err(error).context(RETRIES_ENV),
        };
        Self::with_settings(token, &base, retries)
    }

    /// Use a custom base only for a controlled test server or a future
    /// explicitly supported endpoint. Production HTTP is rejected.
    pub fn with_base_url(token: impl Into<String>, base_url: &str) -> Result<Self> {
        Self::with_settings(token, base_url, RETRIES_DEFAULT)
    }

    pub fn new_with_base_url(token: impl Into<String>, base_url: &str) -> Result<Self> {
        Self::with_base_url(token, base_url)
    }

    pub fn with_retries(mut self, retries: u32) -> Result<Self> {
        ensure!(
            retries <= RETRIES_MAX,
            "Telegram retries must be 0..={RETRIES_MAX}"
        );
        self.retries = retries;
        Ok(self)
    }

    /// Retain the existing adapter convention of a non-fallible setter while
    /// clamping to the hard retry bound. Environment/configuration paths use
    /// with_retries, which reports invalid values instead of guessing.
    pub fn set_retries(&mut self, retries: u32) {
        self.retries = retries.min(RETRIES_MAX);
    }

    pub fn retries(&self) -> u32 {
        self.retries
    }

    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_seconds: u64,
    ) -> Result<Vec<Update>> {
        ensure!(
            timeout_seconds <= 300,
            "Telegram polling timeout must be 0..=300 seconds"
        );
        let url = self.endpoint("getUpdates")?;
        let mut query = vec![(String::from("timeout"), timeout_seconds.to_string())];
        if let Some(offset) = offset {
            query.insert(0, (String::from("offset"), offset.to_string()));
        }
        let request_timeout = Duration::from_secs(timeout_seconds.saturating_add(10));
        let body = self
            .request_bytes(|| {
                self.client
                    .get(url.clone())
                    .query(&query)
                    .timeout(request_timeout)
            })
            .await?;
        decode_result(&body)
    }

    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<Message> {
        ensure!(!text.is_empty(), "Telegram message text must not be empty");
        ensure!(
            text.chars().count() <= 4096,
            "Telegram message text exceeds the Bot API limit of 4096 characters"
        );
        let url = self.endpoint("sendMessage")?;
        let payload = SendMessageRequest {
            chat_id,
            text: text.to_string(),
        };
        let body = self
            .request_bytes(|| self.client.post(url.clone()).json(&payload))
            .await?;
        decode_result(&body)
    }

    /// Edit the existing progress message. Progress updates deliberately use
    /// a separate method so the service cannot accidentally create a stream of
    /// status messages while a turn is running.
    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> Result<()> {
        ensure!(message_id > 0, "Telegram progress message id is invalid");
        ensure!(!text.is_empty(), "Telegram message text must not be empty");
        ensure!(
            text.chars().count() <= 4096,
            "Telegram message text exceeds the Bot API limit of 4096 characters"
        );
        let url = self.endpoint("editMessageText")?;
        let payload = EditMessageTextRequest {
            chat_id,
            message_id,
            text: text.to_string(),
        };
        let body = self
            .request_bytes_with_policy(|| self.client.post(url.clone()).json(&payload), true)
            .await?;
        if is_not_modified(&body) {
            return Ok(());
        }
        decode_result::<Message>(&body).map(|_| ())
    }

    pub fn poller(&self, offset: Option<i64>, timeout_seconds: u64) -> Result<Poller<'_>> {
        ensure!(
            timeout_seconds <= 300,
            "Telegram polling timeout must be 0..=300 seconds"
        );
        Ok(Poller {
            api: self,
            next_offset: offset,
            timeout_seconds,
        })
    }

    pub async fn poll_once(&self, offset: Option<i64>, timeout_seconds: u64) -> Result<PollResult> {
        let mut poller = self.poller(offset, timeout_seconds)?;
        let updates = poller.next().await?;
        Ok(PollResult {
            updates,
            next_offset: poller.offset(),
        })
    }

    fn with_settings(token: impl Into<String>, base_url: &str, retries: u32) -> Result<Self> {
        let token = token.into();
        ensure!(
            !token.trim().is_empty(),
            "Telegram bot token must not be empty"
        );
        ensure!(
            token == token.trim(),
            "Telegram bot token must not contain surrounding whitespace"
        );
        ensure!(
            token
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-') }),
            "Telegram bot token contains invalid characters"
        );
        ensure!(
            retries <= RETRIES_MAX,
            "Telegram retries must be 0..={RETRIES_MAX}"
        );
        let base_url = Url::parse(base_url)
            .map_err(|_| anyhow::anyhow!("invalid Telegram Bot API base URL"))?;
        ensure!(
            base_url.host_str().is_some()
                && (base_url.scheme() == "https"
                    || (base_url.scheme() == "http"
                        && matches!(
                            base_url.host_str(),
                            Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
                        ))),
            "Telegram Bot API endpoint requires HTTPS (literal loopback HTTP allowed for tests)"
        );
        ensure!(
            base_url.username().is_empty()
                && base_url.password().is_none()
                && base_url.query().is_none()
                && base_url.fragment().is_none(),
            "Telegram Bot API base URL cannot contain credentials, query, or fragment"
        );
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(310))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            base_url,
            token,
            retries,
        })
    }

    fn endpoint(&self, method: &str) -> Result<Url> {
        ensure!(
            matches!(method, "getUpdates" | "sendMessage" | "editMessageText"),
            "unsupported Telegram Bot API method"
        );
        let mut url = self.base_url.clone();
        let base_path = url.path().trim_end_matches('/');
        let path = format!("{base_path}/bot{}/{method}", self.token);
        url.set_path(&path);
        Ok(url)
    }

    async fn request_bytes<F>(&self, build: F) -> Result<Vec<u8>>
    where
        F: Fn() -> RequestBuilder,
    {
        self.request_bytes_with_policy(build, false).await
    }

    async fn request_bytes_with_policy<F>(
        &self,
        build: F,
        allow_not_modified: bool,
    ) -> Result<Vec<u8>>
    where
        F: Fn() -> RequestBuilder,
    {
        let attempts = self.retries.saturating_add(1);
        for attempt in 1..=attempts {
            let response = match build().send().await {
                Ok(response) => response,
                Err(_) if attempt < attempts => {
                    tokio::time::sleep(retry_delay(attempt, None)).await;
                    continue;
                }
                Err(_) => bail!("Telegram Bot API transport failed"),
            };
            let status = response.status();
            let retry_after = retry_after_header(response.headers());
            let body = read_body(response).await?;
            if attempt < attempts && (retryable_status(status) || retryable_body(&body)) {
                let delay = retry_after.or_else(|| retry_after_body(&body));
                tokio::time::sleep(retry_delay(attempt, delay)).await;
                continue;
            }
            if allow_not_modified && status == StatusCode::BAD_REQUEST && is_not_modified(&body) {
                return Ok(body);
            }
            if !status.is_success() {
                bail!(
                    "Telegram Bot API request failed with HTTP status {}",
                    status.as_u16()
                );
            }
            return Ok(body);
        }
        unreachable!("Telegram retry loop always returns")
    }
}

pub struct Poller<'a> {
    api: &'a BotApi,
    next_offset: Option<i64>,
    timeout_seconds: u64,
}

impl<'a> Poller<'a> {
    pub fn offset(&self) -> Option<i64> {
        self.next_offset
    }

    pub fn set_offset(&mut self, offset: Option<i64>) {
        self.next_offset = offset;
    }

    pub async fn next(&mut self) -> Result<Vec<Update>> {
        let updates = self
            .api
            .get_updates(self.next_offset, self.timeout_seconds)
            .await?;
        if let Some(last) = updates.iter().map(|update| update.update_id).max() {
            self.next_offset = Some(last.checked_add(1).context("Telegram update id overflow")?);
        }
        Ok(updates)
    }

    /// Run the long-poll loop with an application-owned update handler. The
    /// handler is deliberately the only hook here; command/session policy is
    /// outside this B1 module.
    pub async fn run<F, Fut>(&mut self, mut handle: F) -> Result<()>
    where
        F: FnMut(Update) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        loop {
            let updates = self
                .api
                .get_updates(self.next_offset, self.timeout_seconds)
                .await?;
            for update in updates {
                handle(update.clone()).await?;
                self.next_offset = Some(
                    update
                        .update_id
                        .checked_add(1)
                        .context("Telegram update id overflow")?,
                );
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PollResult {
    pub updates: Vec<Update>,
    pub next_offset: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
}

impl Update {
    pub fn user_id(&self) -> Option<i64> {
        self.message.as_ref()?.from.as_ref().map(|user| user.id)
    }

    pub fn chat_id(&self) -> Option<i64> {
        self.message.as_ref().map(|message| message.chat.id)
    }

    pub fn text(&self) -> Option<&str> {
        self.message.as_ref()?.text.as_deref()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Message {
    pub message_id: i64,
    #[serde(default)]
    pub from: Option<User>,
    pub chat: Chat,
    #[serde(default)]
    pub date: Option<i64>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct User {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Serialize)]
struct SendMessageRequest {
    chat_id: i64,
    text: String,
}

#[derive(Debug, Serialize)]
struct EditMessageTextRequest {
    chat_id: i64,
    message_id: i64,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(bound = "T: serde::de::DeserializeOwned")]
struct ApiResponse<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    error_code: Option<i64>,
}

fn decode_result<T>(body: &[u8]) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let response: ApiResponse<T> = serde_json::from_slice(body)
        .map_err(|_| anyhow::anyhow!("Telegram Bot API returned invalid JSON"))?;
    ensure!(
        response.ok,
        "Telegram Bot API returned an application error{}",
        response
            .error_code
            .map(|code| format!(" (code {code})"))
            .unwrap_or_default()
    );
    response
        .result
        .context("Telegram Bot API response did not contain a result")
}

async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("Telegram Bot API response body failed"))?
    {
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
            "Telegram Bot API response exceeded the size limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds = raw.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds.min(60)))
}

fn retry_after_body(body: &[u8]) -> Option<Duration> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let seconds = value["parameters"]["retry_after"].as_u64()?;
    Some(Duration::from_secs(seconds.min(60)))
}

fn retryable_body(body: &[u8]) -> bool {
    let value: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return false,
    };
    if value["ok"].as_bool() != Some(false) {
        return false;
    }
    value["error_code"]
        .as_i64()
        .is_some_and(|code| code == 429 || (500..=599).contains(&code))
}

fn is_not_modified(body: &[u8]) -> bool {
    let value: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return false,
    };
    value["ok"].as_bool() == Some(false)
        && value["description"].as_str().is_some_and(|description| {
            description.eq_ignore_ascii_case("bad request: message is not modified")
        })
}

/// Match the provider wire schedule: bounded exponential delays with small
/// jitter, while honoring a bounded server retry hint when present.
fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(delay) = retry_after {
        return delay;
    }
    const SCHEDULE_MS: [u64; 3] = [1_000, 4_000, 16_000];
    let base = SCHEDULE_MS[(attempt.max(1) as usize - 1).min(SCHEDULE_MS.len() - 1)];
    let spread = (base / 4).max(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64)
        .unwrap_or(0);
    Duration::from_millis(base - spread / 2 + nanos % spread)
}
