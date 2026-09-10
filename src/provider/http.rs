//! Bounded, credential-safe transport shared by the Bearer-authenticated adapters.
use anyhow::{Result, ensure};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct Http {
    client: reqwest::Client,
    url: reqwest::Url,
    header: reqwest::header::HeaderName,
    key: reqwest::header::HeaderValue,
    retries: u32,
    zai_diagnostics: bool,
}
impl Http {
    /// Bearer-authenticated transport (OpenAI, GLM).
    pub fn new(base: &str, suffix: &str, key: &str, timeout_seconds: u64) -> Result<Self> {
        Self::with_auth(
            base,
            suffix,
            reqwest::header::AUTHORIZATION,
            &format!("Bearer {key}"),
            key,
            timeout_seconds,
        )
    }
    /// Transport for providers that authenticate with a non-Bearer header
    /// (Anthropic uses `x-api-key`). The credential is still carried in a
    /// sensitive `HeaderValue` and the same URL and redirect rules apply.
    pub fn with_auth(
        base: &str,
        suffix: &str,
        header: reqwest::header::HeaderName,
        header_value: &str,
        key: &str,
        timeout_seconds: u64,
    ) -> Result<Self> {
        ensure!(
            (1..=300).contains(&timeout_seconds),
            "invalid provider timeout"
        );
        Self::with_timeouts(
            base,
            suffix,
            header,
            header_value,
            key,
            Duration::from_secs(timeout_seconds),
            Duration::from_secs(10),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn with_timeouts(
        base: &str,
        suffix: &str,
        header: reqwest::header::HeaderName,
        header_value: &str,
        key: &str,
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self> {
        ensure!(!key.trim().is_empty(), "provider API key is empty");
        let mut url =
            reqwest::Url::parse(base).map_err(|_| anyhow::anyhow!("invalid provider base URL"))?;
        ensure!(
            url.host_str().is_some()
                && (url.scheme() == "https"
                    || (url.scheme() == "http"
                        && matches!(url.host_str(), Some("127.0.0.1" | "[::1]")))),
            "provider endpoint requires HTTPS (literal loopback HTTP allowed for tests)"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "provider base URL cannot contain credentials, query, or fragment"
        );
        url.set_path(&format!("{}/{}", url.path().trim_end_matches('/'), suffix));
        let mut key = reqwest::header::HeaderValue::from_str(header_value)
            .map_err(|_| anyhow::anyhow!("invalid provider API key header"))?;
        key.set_sensitive(true);
        let client = reqwest::Client::builder()
            .timeout(request_timeout)
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            url,
            header,
            key,
            retries: 0,
            zai_diagnostics: false,
        })
    }

    /// Bounded wire-level retry budget (issue #67 B): 0 disables; the
    /// default construction leaves retries off until configured.
    pub fn enable_zai_diagnostics(&mut self) {
        self.zai_diagnostics = true;
    }

    pub fn set_retries(&mut self, retries: u32) {
        self.retries = retries;
    }
    pub async fn post(&self, body: &Value, usage: &mut crate::usage::Usage) -> Result<Value> {
        let bytes = self
            .post_bytes(body, usage, reqwest::header::HeaderMap::new())
            .await?;
        serde_json::from_slice(&bytes).map_err(|_| {
            crate::failure::provider_error(crate::failure::ProviderReason::InvalidJson)
        })
    }
    pub async fn post_bytes(
        &self,
        body: &Value,
        usage: &mut crate::usage::Usage,
        headers: reqwest::header::HeaderMap,
    ) -> Result<Vec<u8>> {
        self.post_until(body, usage, headers, |_| Ok(None)).await
    }
    /// Stop at a validated application-level terminal event without waiting for EOF.
    pub async fn post_until(
        &self,
        body: &Value,
        usage: &mut crate::usage::Usage,
        headers: reqwest::header::HeaderMap,
        terminal: impl Fn(&[u8]) -> Result<Option<usize>>,
    ) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(body)?;
        ensure!(
            bytes.len() <= 512 * 1024,
            "request context exceeds 512 KiB; start a new session"
        );
        let request_bytes = bytes.len();
        // Bounded retries (issue #67 B): only before any effect — connect /
        // pre-header transport failures and retryable HTTP statuses (429,
        // 500, 502, 503, 504). Response-body failures never retry, because a
        // partial response may already have been consumed.
        let attempts = self.retries.saturating_add(1);
        let mut attempt: u32 = 0;
        let started = Instant::now();
        let mut response = loop {
            attempt += 1;
            usage.attempted = true;
            let request = self
                .client
                .post(self.url.clone())
                .headers(headers.clone())
                .header(self.header.clone(), self.key.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes.clone())
                .build()
                .map_err(|_| anyhow::anyhow!("could not construct provider request"))?;
            let attempt_started = Instant::now();
            let response = match self.client.execute(request).await {
                Ok(response) => response,
                Err(error) => {
                    let phase = if error.is_connect() {
                        "connect"
                    } else {
                        "before_response_headers"
                    };
                    let diagnostic =
                        transport_error(&error, phase, attempt_started, request_bytes, attempt);
                    let retryable = crate::failure::transport(&diagnostic)
                        .is_some_and(retryable_transport_phase);
                    if attempt < attempts && retryable {
                        tokio::time::sleep(retry_delay(attempt, None)).await;
                        continue;
                    }
                    return Err(diagnostic);
                }
            };
            if !response.status().is_success() {
                let status = response.status();
                let retry_after = retry_after_seconds(response.headers());
                if attempt < attempts && retryable_status(status.as_u16()) {
                    drop(response);
                    tokio::time::sleep(retry_delay(attempt, retry_after)).await;
                    continue;
                }
                let error = crate::failure::http_status_error(status);
                return Err(if self.zai_diagnostics {
                    error.context(super::http_diagnostic::capture(response).await)
                } else {
                    error
                });
            }
            break response;
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            transport_error(&error, "response_body", started, request_bytes, attempt)
        })? {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(crate::failure::provider_error(
                    crate::failure::ProviderReason::ResponseTooLarge,
                ));
            }
            bytes.extend_from_slice(&chunk);
            if let Some(end) = terminal(&bytes)? {
                ensure!(end <= bytes.len(), "invalid terminal response boundary");
                bytes.truncate(end);
                return Ok(bytes);
            }
        }
        Ok(bytes)
    }
}

// Never format reqwest's error: it may include the URL or private
// transport details. Stage names, measured duration and byte count are safe.
fn transport_error(
    error: &reqwest::Error,
    phase: &'static str,
    started: Instant,
    request_bytes: usize,
    attempts: u32,
) -> anyhow::Error {
    let phase = match phase {
        "connect" => crate::failure::TransportPhase::Connect,
        "before_response_headers" => crate::failure::TransportPhase::BeforeResponseHeaders,
        "response_body" => crate::failure::TransportPhase::ResponseBody,
        _ => unreachable!("native HTTP transport supplied an unknown phase"),
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let request_bytes = u64::try_from(request_bytes).unwrap_or(u64::MAX);
    let diagnostic = crate::failure::TransportDiagnostic::new(
        phase,
        elapsed_ms,
        request_bytes,
        error.is_timeout(),
    )
    .with_attempts(attempts);
    crate::failure::at(if error.is_timeout() {
        crate::failure::Kind::ProviderTimeout
    } else {
        crate::failure::Kind::Provider
    })(anyhow::Error::new(diagnostic))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        thread,
    };

    async fn delayed_failure(tls: bool, body_delay: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            if !tls {
                let mut reader = BufReader::new(&mut socket);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if body_delay {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                        .unwrap();
                    socket.flush().unwrap();
                }
            }
            thread::sleep(Duration::from_millis(350));
        });
        let scheme = if tls { "https" } else { "http" };
        let base = format!("{scheme}://127.0.0.1:{port}");
        let client = Http::with_timeouts(
            &base,
            "test",
            reqwest::header::AUTHORIZATION,
            "Bearer PRIVATE_KEY_MARKER",
            "PRIVATE_KEY_MARKER",
            Duration::from_millis(if tls { 1000 } else { 150 }),
            Duration::from_millis(if tls { 100 } else { 1000 }),
        )
        .unwrap();
        let request = serde_json::json!({"content":"PRIVATE_BODY_MARKER"});
        let error = client
            .post(&request, &mut crate::usage::Usage::default())
            .await
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(error.starts_with("provider request timed out:"), "{error}");
        assert!(error.contains("elapsed_ms="));
        assert!(error.contains(&format!(
            "request_bytes={}",
            serde_json::to_vec(&request).unwrap().len()
        )));
        assert!(!error.contains("PRIVATE") && !error.contains(&base));
        error
    }

    #[tokio::test]
    async fn tls_stall_is_connection_timeout() {
        assert!(
            delayed_failure(true, false)
                .await
                .contains("phase=connect ")
        );
    }
    #[tokio::test]
    async fn header_stall_is_pre_header_timeout() {
        assert!(
            delayed_failure(false, false)
                .await
                .contains("phase=before_response_headers ")
        );
    }
    #[tokio::test]
    async fn body_stall_is_body_timeout() {
        assert!(
            delayed_failure(false, true)
                .await
                .contains("phase=response_body ")
        );
    }

    #[tokio::test]
    async fn transport_timeout_exposes_only_typed_safe_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            thread::sleep(Duration::from_millis(250));
        });
        let base = format!("http://127.0.0.1:{port}");
        let client = Http::with_timeouts(
            &base,
            "test",
            reqwest::header::AUTHORIZATION,
            "Bearer PRIVATE_KEY_MARKER",
            "PRIVATE_KEY_MARKER",
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
        .unwrap();
        let request = serde_json::json!({"private":"PRIVATE_BODY_MARKER"});
        let error = client
            .post(&request, &mut crate::usage::Usage::default())
            .await
            .unwrap_err();
        server.join().unwrap();
        assert_eq!(
            crate::failure::category(&error),
            Some(crate::failure::Kind::ProviderTimeout)
        );
        let diagnostic = crate::failure::transport(&error).expect("typed transport metadata");
        assert_eq!(
            diagnostic.phase(),
            crate::failure::TransportPhase::BeforeResponseHeaders
        );
        assert!(diagnostic.elapsed_ms() > 0);
        assert_eq!(
            diagnostic.request_bytes(),
            serde_json::to_vec(&request).unwrap().len() as u64
        );
        let rendered = error.to_string();
        assert!(!rendered.contains("PRIVATE") && !rendered.contains(&base));
    }
}

/// Retryable provider statuses (issue #67 B): quota/rate limiting and the
/// classic transient server errors. Other 4xx never retry.
fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Only failures before any response byte was consumed retry; a
/// response-body failure may already have delivered partial content.
fn retryable_transport_phase(diagnostic: &crate::failure::TransportDiagnostic) -> bool {
    diagnostic.phase() != crate::failure::TransportPhase::ResponseBody
}

/// `Retry-After` in seconds, capped at 60s; HTTP-date forms are ignored.
fn retry_after_seconds(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(seconds.min(60)))
}

/// Exponential backoff 1s -> 4s -> 16s with roughly +-25% jitter; a
/// Retry-After hint (already capped) replaces the schedule outright.
fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(delay) = retry_after {
        return delay;
    }
    const SCHEDULE_MS: [u64; 3] = [1_000, 4_000, 16_000];
    let base = SCHEDULE_MS[(attempt.max(1) as usize - 1).min(SCHEDULE_MS.len() - 1)];
    let spread = (base / 4).max(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    Duration::from_millis(base - spread / 2 + nanos % spread)
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn retryable_status_set_is_closed_and_transient_only() {
        for status in [429, 500, 502, 503, 504] {
            assert!(retryable_status(status), "{status} must retry");
        }
        for status in [400, 401, 403, 404, 418, 302, 200] {
            assert!(!retryable_status(status), "{status} must not retry");
        }
    }

    #[test]
    fn backoff_schedule_stays_within_jitter_bounds() {
        for (attempt, base) in [(1u32, 1_000u64), (2, 4_000), (3, 16_000), (9, 16_000)] {
            let delay = retry_delay(attempt, None);
            let ms = delay.as_millis() as u64;
            assert!(
                ms >= base - base / 4 && ms <= base + base / 4,
                "attempt {attempt}: {ms}ms outside ±25% of {base}ms"
            );
        }
    }

    #[test]
    fn retry_after_is_respected_and_capped_at_sixty_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_seconds(&headers), None);
        headers.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_after_seconds(&headers), Some(Duration::from_secs(7)));
        headers.insert(reqwest::header::RETRY_AFTER, "999".parse().unwrap());
        assert_eq!(retry_after_seconds(&headers), Some(Duration::from_secs(60)));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "http-date-form".parse().unwrap(),
        );
        assert_eq!(retry_after_seconds(&headers), None);
    }
}
