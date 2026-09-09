//! Bounded, credential-safe transport shared by the Bearer-authenticated adapters.
use anyhow::{Result, ensure};
use serde_json::Value;
use std::time::{Duration, Instant};

/// Retry-After is honored up to this cap; beyond it a hostile or misconfigured
/// header cannot extend a run past its own budget (#67 B).
const RETRY_AFTER_CAP_SECONDS: u64 = 60;
/// Exponential base delays (seconds) for the first three retries, 1s -> 4s -> 16s.
const BACKOFF_BASE_SECONDS: [u64; 3] = [1, 4, 16];

/// Retry policy for one provider request. Transport timeouts in the connect or
/// pre-header phase and server-side status codes (429, 500, 502, 503, 504) are
/// retryable; everything else — including timeouts after the response headers
/// arrived, where a partial body may already have been consumed — fails on the
/// first attempt. A provider request runs strictly before any tool effect, so
/// retrying never re-executes an effect (#67 B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub retries: u32,
    pub attempts: u32,
}
impl RetryPolicy {
    pub const MAX_RETRIES: u32 = 5;
    /// `retries` is the number of retries after the first attempt (0..=5).
    pub fn new(retries: u32) -> Result<Self> {
        ensure!(retries <= Self::MAX_RETRIES, "invalid provider retry limit");
        Ok(Self {
            retries,
            attempts: retries + 1,
        })
    }
    fn retryable_status(status: reqwest::StatusCode) -> bool {
        matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
    }
    /// Retry-After in seconds. Only the seconds form is honored; an HTTP-date
    /// or unparseable value means "no hint" and the exponential backoff applies.
    fn retry_after(response: &reqwest::Response) -> Option<Duration> {
        let value = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)?
            .to_str()
            .ok()?;
        let seconds: u64 = value.trim().parse().ok()?;
        Some(Duration::from_secs(seconds.min(RETRY_AFTER_CAP_SECONDS)))
    }
    /// Bounded delay before the next attempt: the capped Retry-After when
    /// present, otherwise the exponential base with up to ±25% jitter derived
    /// from the wall clock (no external rng dependency).
    fn delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(wait) = retry_after {
            return wait.min(Duration::from_secs(RETRY_AFTER_CAP_SECONDS));
        }
        let base = BACKOFF_BASE_SECONDS[attempt.saturating_sub(1).min(2) as usize];
        let quarter = base / 4;
        let nanos = u64::try_from(Instant::now().elapsed().as_nanos()).unwrap_or(0);
        let jitter = nanos % (2 * quarter + 1);
        Duration::from_secs(base - quarter + jitter)
    }
}

pub struct Http {
    client: reqwest::Client,
    url: reqwest::Url,
    header: reqwest::header::HeaderName,
    key: reqwest::header::HeaderValue,
    retries: u32,
}
impl Http {
    /// Bearer-authenticated transport (OpenAI, GLM).
    pub fn new(
        base: &str,
        suffix: &str,
        key: &str,
        timeout_seconds: u64,
        retries: u32,
    ) -> Result<Self> {
        Self::with_auth(
            base,
            suffix,
            reqwest::header::AUTHORIZATION,
            &format!("Bearer {key}"),
            key,
            timeout_seconds,
            retries,
        )
    }
    /// Transport for providers that authenticate with a non-Bearer header
    /// (Anthropic uses `x-api-key`). The credential is still carried in a
    /// sensitive `HeaderValue` and the same URL and redirect rules apply.
    #[allow(clippy::too_many_arguments)]
    pub fn with_auth(
        base: &str,
        suffix: &str,
        header: reqwest::header::HeaderName,
        header_value: &str,
        key: &str,
        timeout_seconds: u64,
        retries: u32,
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
            retries,
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
        retries: u32,
    ) -> Result<Self> {
        ensure!(!key.trim().is_empty(), "provider API key is empty");
        let retries = RetryPolicy::new(retries)?.attempts - 1;
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
            retries,
        })
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
        usage.attempted = true;
        // Provider requests run strictly before any tool effect, so retrying
        // never re-executes an effect. The whole retry sequence still lives
        // inside the run timeout; --provider-timeout-seconds bounds each
        // attempt, not the sequence (#67 B).
        let attempts_allowed = RetryPolicy::new(self.retries)?.attempts;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let started = Instant::now();
            let request = self
                .client
                .post(self.url.clone())
                .headers(headers.clone())
                .header(self.header.clone(), self.key.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes.clone())
                .build()
                .map_err(|_| anyhow::anyhow!("could not construct provider request"))?;
            let response = match self.client.execute(request).await {
                Ok(response) => response,
                Err(error) => {
                    let phase = if error.is_connect() {
                        "connect"
                    } else {
                        "before_response_headers"
                    };
                    let timed_out = error.is_timeout();
                    let retryable = timed_out;
                    if retryable && attempt < attempts_allowed {
                        let wait = RetryPolicy::delay(attempt, None);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(transport_error(
                        &error,
                        phase,
                        started,
                        request_bytes,
                        u64::from(attempt),
                    ));
                }
            };
            if !response.status().is_success() {
                let retry_after = RetryPolicy::retry_after(&response);
                let status = response.status();
                if RetryPolicy::retryable_status(status) && attempt < attempts_allowed {
                    tokio::time::sleep(RetryPolicy::delay(attempt, retry_after)).await;
                    continue;
                }
                return Err(crate::failure::http_status_error(status));
            }
            let mut response = response;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| {
                transport_error(
                    &error,
                    "response_body",
                    started,
                    request_bytes,
                    u64::from(attempt),
                )
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
            return Ok(bytes);
        }
    }
}

// Never format reqwest's error: it may include the URL or private
// transport details. Stage names, measured duration and byte count are safe.
fn transport_error(
    error: &reqwest::Error,
    phase: &'static str,
    started: Instant,
    request_bytes: usize,
    attempts: u64,
) -> anyhow::Error {
    let phase = match phase {
        "connect" => crate::failure::TransportPhase::Connect,
        "before_response_headers" => crate::failure::TransportPhase::BeforeResponseHeaders,
        "response_body" => crate::failure::TransportPhase::ResponseBody,
        _ => unreachable!("native HTTP transport supplied an unknown phase"),
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let request_bytes = u64::try_from(request_bytes).unwrap_or(u64::MAX);
    let diagnostic = crate::failure::TransportDiagnostic::with_attempts(
        phase,
        elapsed_ms,
        request_bytes,
        error.is_timeout(),
        attempts,
    );
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
            0,
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
            0,
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

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn retry_policy_bounds_fail_closed() {
        assert_eq!(RetryPolicy::new(0).unwrap().attempts, 1);
        assert_eq!(RetryPolicy::new(5).unwrap().attempts, 6);
        assert!(RetryPolicy::new(6).is_err());
        assert!(RetryPolicy::new(u32::MAX).is_err());
    }

    #[test]
    fn retryable_statuses_are_the_documented_closed_set() {
        for code in [429, 500, 502, 503, 504] {
            assert!(RetryPolicy::retryable_status(
                reqwest::StatusCode::from_u16(code).unwrap()
            ));
        }
        for code in [400, 401, 403, 404, 409, 418, 499, 501, 505] {
            assert!(!RetryPolicy::retryable_status(
                reqwest::StatusCode::from_u16(code).unwrap()
            ));
        }
    }

    #[test]
    fn delay_follows_exponential_base_with_capped_retry_after() {
        // Jitter is bounded to ±25% of the base, so the delay lands in a
        // deterministic range even though its source is the wall clock.
        let range = |attempt: u32| {
            let base = BACKOFF_BASE_SECONDS[attempt.saturating_sub(1).min(2) as usize];
            let quarter = base / 4;
            (
                Duration::from_secs(base - quarter),
                Duration::from_secs(base + quarter),
            )
        };
        for attempt in [1u32, 2, 3, 9] {
            let (low, high) = range(attempt);
            let delay = RetryPolicy::delay(attempt, None);
            assert!(delay >= low && delay <= high, "{attempt}: {delay:?}");
        }
        assert_eq!(
            RetryPolicy::delay(1, Some(Duration::from_secs(2))),
            Duration::from_secs(2)
        );
        // A hostile Retry-After cannot extend the run beyond the cap.
        assert_eq!(
            RetryPolicy::delay(1, Some(Duration::from_secs(120))),
            Duration::from_secs(RETRY_AFTER_CAP_SECONDS)
        );
    }
}
