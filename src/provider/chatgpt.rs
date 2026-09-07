//! Opt-in read-only Codex file credentials and bounded terminal SSE responses.
//! No credential discovery, refresh, retry, or delegated agent execution.
use super::http::Http;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use std::{
    ffi::CString,
    fs::File,
    io::Read,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
};

pub struct ChatGpt {
    path: PathBuf,
    base: String,
    timeout: u64,
    account: String,
}
impl ChatGpt {
    pub fn new(path: &Path, base: &str, timeout: u64) -> Result<Self> {
        let url =
            reqwest::Url::parse(base).map_err(|_| anyhow::anyhow!("invalid ChatGPT endpoint"))?;
        ensure!(
            base == "https://chatgpt.com/backend-api/codex"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()),
            "ChatGPT endpoint must be the Codex service or literal loopback HTTP fixture"
        );
        let (token, account) = credentials(path)?;
        Http::new(base, "responses", &token, timeout)?;
        Ok(Self {
            path: path.into(),
            base: base.into(),
            timeout,
            account,
        })
    }
    pub async fn post(&self, body: &Value, usage: &mut crate::usage::Usage) -> Result<Value> {
        let (token, account) = credentials(&self.path)?;
        ensure!(
            account == self.account,
            "ChatGPT account changed during run; start a new run"
        );
        let mut headers = reqwest::header::HeaderMap::new();
        let mut id = reqwest::header::HeaderValue::from_str(&account)
            .map_err(|_| anyhow::anyhow!("invalid ChatGPT account header"))?;
        id.set_sensitive(true);
        headers.insert("chatgpt-account-id", id);
        headers.insert(
            "originator",
            reqwest::header::HeaderValue::from_static("danso"),
        );
        headers.insert(
            "openai-beta",
            reqwest::header::HeaderValue::from_static("responses=experimental"),
        );
        headers.insert(
            "accept",
            reqwest::header::HeaderValue::from_static("text/event-stream"),
        );
        let data = Http::new(&self.base, "responses", &token, self.timeout)?
            .post_until(body, usage, headers, completed_prefix)
            .await?;
        terminal_response(&data)
    }
}

fn private_file(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "ChatGPT auth path must be absolute");
    let mut dir = File::open("/")?;
    let parts: Vec<_> = path.components().collect();
    ensure!(parts.len() > 2, "invalid ChatGPT auth path");
    for (index, part) in parts.iter().enumerate().skip(1) {
        let Component::Normal(name) = part else {
            bail!("ChatGPT auth path must be normalized")
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid ChatGPT auth path"))?;
        let last = index == parts.len() - 1;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        // Walk relative to pinned directory descriptors; no path component follows symlinks.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        ensure!(fd >= 0, "cannot open ChatGPT auth path safely");
        let next = unsafe { File::from_raw_fd(fd) };
        let meta = next.metadata()?;
        if last || index == parts.len() - 2 {
            ensure!(
                meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
                "ChatGPT auth file and parent must be owner-only and owned by current user"
            );
        }
        if last {
            ensure!(
                meta.is_file() && meta.nlink() == 1 && meta.len() <= 64 * 1024,
                "ChatGPT auth must be a private regular file up to 64 KiB"
            );
            return Ok(next);
        }
        dir = next;
    }
    bail!("invalid ChatGPT auth path")
}

fn credentials(path: &Path) -> Result<(String, String)> {
    let mut file = private_file(path)?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .context("cannot read ChatGPT auth")?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() <= 64 * 1024
            && before.len() == after.len()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.mtime() == after.mtime(),
        "ChatGPT auth changed while reading"
    );
    let v: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid ChatGPT auth JSON"))?;
    ensure!(
        (v["auth_mode"].is_null() || v["auth_mode"] == "chatgpt") && v["OPENAI_API_KEY"].is_null(),
        "ChatGPT file authentication required; API keys are not subscription credentials"
    );
    let token = v["tokens"]["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing ChatGPT access token")?;
    let account = v["tokens"]["account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing ChatGPT account ID")?;
    let parts: Vec<_> = token.split('.').collect();
    ensure!(parts.len() == 3, "invalid ChatGPT access token format");
    let payload = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT token metadata"))?;
    let claims: Value = serde_json::from_slice(&payload)
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT token metadata"))?;
    // Metadata is not signature verification: the service authenticates the bearer.
    ensure!(
        claims["https://api.openai.com/auth"]["chatgpt_account_id"] == account,
        "ChatGPT account metadata mismatch"
    );
    let exp = claims["exp"]
        .as_i64()
        .context("missing ChatGPT token expiry")?;
    ensure!(
        exp > chrono::Utc::now().timestamp().saturating_add(60),
        "ChatGPT authentication expired or expires within 60 seconds; renew using Codex login then retry explicitly"
    );
    Ok((token.into(), account.into()))
}

fn completed_prefix(data: &[u8]) -> Result<Option<usize>> {
    // A UTF-8 character or SSE frame may be split across arbitrary HTTP chunks.
    let lf = data.windows(2).rposition(|w| w == b"\n\n").map(|i| i + 2);
    let crlf = data
        .windows(4)
        .rposition(|w| w == b"\r\n\r\n")
        .map(|i| i + 4);
    let Some(end) = lf.max(crlf) else {
        return Ok(None);
    };
    Ok(parse_events(&data[..end])?.map(|_| end))
}

fn terminal_response(data: &[u8]) -> Result<Value> {
    parse_events(data)?.context("ChatGPT SSE ended without completed response")
}

fn parse_events(data: &[u8]) -> Result<Option<Value>> {
    let text =
        std::str::from_utf8(data).map_err(|_| anyhow::anyhow!("invalid ChatGPT SSE encoding"))?;
    let normalized = text.replace("\r\n", "\n");
    ensure!(normalized.ends_with("\n\n"), "truncated ChatGPT SSE stream");
    let mut terminal = None;
    for block in normalized.split("\n\n") {
        let mut event = None;
        let mut payload = Vec::new();
        for line in block.lines() {
            if let Some(s) = line.strip_prefix("event:") {
                ensure!(event.is_none(), "duplicate ChatGPT SSE event field");
                event = Some(s.trim());
            } else if let Some(s) = line.strip_prefix("data:") {
                payload.push(s.strip_prefix(' ').unwrap_or(s));
            } else if !line.is_empty() && !line.starts_with(':') {
                bail!("unsupported ChatGPT SSE field");
            }
        }
        if payload.is_empty() {
            continue;
        }
        let payload = payload.join("\n");
        if payload == "[DONE]" {
            ensure!(
                terminal.is_some(),
                "ChatGPT SSE ended without completed response"
            );
            continue;
        }
        ensure!(
            terminal.is_none(),
            "ChatGPT SSE data after completed response"
        );
        let v: Value = serde_json::from_str(&payload)
            .map_err(|_| anyhow::anyhow!("invalid ChatGPT SSE JSON"))?;
        let kind = v["type"]
            .as_str()
            .context("missing ChatGPT SSE event type")?;
        ensure!(
            event.is_none_or(|e| e == kind),
            "ChatGPT SSE event type mismatch"
        );
        match kind {
            "response.completed" | "response.done" => {
                ensure!(
                    v["response"].is_object()
                        && v["response"]["status"] == "completed"
                        && v["response"]["error"].is_null(),
                    "ChatGPT response did not complete"
                );
                terminal = Some(v["response"].clone());
            }
            "error" | "response.failed" | "response.incomplete" => {
                bail!("ChatGPT response failed or incomplete")
            }
            "response.created"
            | "response.in_progress"
            | "response.output_item.added"
            | "response.output_item.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done"
            | "response.refusal.delta"
            | "response.refusal.done" => {}
            _ => bail!("unsupported ChatGPT SSE event"),
        }
    }
    Ok(terminal)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_terminal_in_buffer_fails() {
        let frame =
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
        let mut data = frame.to_vec();
        data.extend_from_slice(frame);
        assert!(terminal_response(&data).is_err());
    }
    #[test]
    fn partial_utf8_and_frame_wait_for_delimiter() {
        assert!(
            completed_prefix(b"data: {\"text\":\"\xf0\x9f")
                .unwrap()
                .is_none()
        );
        assert!(
            completed_prefix(b"data: {\"type\":\"response.completed\"}")
                .unwrap()
                .is_none()
        );
    }
}
