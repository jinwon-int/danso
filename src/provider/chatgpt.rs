//! Explicit file credentials (read-only or adopted managed store) and bounded SSE.
//! No credential discovery, model-request retry, or delegated agent execution.
use super::http::Http;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
        let (token, account) = super::chatgpt_auth::inspect(path)?;
        Http::new(base, "responses", &token, timeout)?;
        Ok(Self {
            path: path.into(),
            base: base.into(),
            timeout,
            account,
        })
    }
    pub async fn post(&self, body: &Value, usage: &mut crate::usage::Usage) -> Result<Value> {
        let (token, account) =
            super::chatgpt_auth::access(&self.path, &self.base, self.timeout).await?;
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
    let mut items: BTreeMap<u64, Value> = BTreeMap::new();
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
                let mut response = v["response"].clone();
                let output = response["output"]
                    .as_array_mut()
                    .context("missing ChatGPT terminal output")?;
                if output.is_empty() {
                    for (expected, (index, item)) in items.iter().enumerate() {
                        ensure!(
                            *index == expected as u64,
                            "incomplete ChatGPT streamed output"
                        );
                        output.push(item.clone());
                    }
                } else {
                    for (index, item) in &items {
                        let index =
                            usize::try_from(*index).context("invalid ChatGPT output index")?;
                        ensure!(
                            output.get(index) == Some(item),
                            "conflicting ChatGPT terminal output"
                        );
                    }
                }
                terminal = Some(response);
            }
            "response.output_item.done" => {
                let index = v["output_index"]
                    .as_u64()
                    .context("missing ChatGPT output index")?;
                ensure!(
                    v["item"].is_object(),
                    "missing ChatGPT completed output item"
                );
                ensure!(
                    items.insert(index, v["item"].clone()).is_none(),
                    "duplicate ChatGPT output index"
                );
            }
            "error" | "response.failed" | "response.incomplete" => {
                bail!("ChatGPT response failed or incomplete")
            }
            "response.created"
            | "response.in_progress"
            | "response.output_item.added"
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
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";
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
