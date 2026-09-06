//! Bounded, credential-safe transport shared by the Bearer-authenticated adapters.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct Http {
    client: reqwest::Client,
    url: reqwest::Url,
    key: reqwest::header::HeaderValue,
}
impl Http {
    pub fn new(base: &str, suffix: &str, key: &str, timeout_seconds: u64) -> Result<Self> {
        ensure!(
            (1..=300).contains(&timeout_seconds),
            "invalid provider timeout"
        );
        Self::with_timeouts(
            base,
            suffix,
            key,
            Duration::from_secs(timeout_seconds),
            Duration::from_secs(10),
        )
    }
    fn with_timeouts(
        base: &str,
        suffix: &str,
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
        let mut key = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| anyhow::anyhow!("invalid provider API key header"))?;
        key.set_sensitive(true);
        let client = reqwest::Client::builder()
            .timeout(request_timeout)
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { client, url, key })
    }
    pub async fn post(&self, body: &Value, usage: &mut crate::usage::Usage) -> Result<Value> {
        let bytes = serde_json::to_vec(body)?;
        ensure!(
            bytes.len() <= 512 * 1024,
            "request context exceeds 512 KiB; start a new session"
        );
        let request_bytes = bytes.len();
        let request = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::AUTHORIZATION, self.key.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .build()
            .map_err(|_| anyhow::anyhow!("could not construct provider request"))?;
        usage.attempted = true;
        let started = Instant::now();
        let mut response = self.client.execute(request).await.map_err(|error| {
            let phase = if error.is_connect() {
                "connect"
            } else {
                "before_response_headers"
            };
            transport_error(&error, phase, started, request_bytes)
        })?;
        ensure!(
            response.status().is_success(),
            "provider request failed: HTTP {}",
            response.status().as_u16()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| transport_error(&error, "response_body", started, request_bytes))?
        {
            ensure!(
                bytes.len() + chunk.len() <= 1024 * 1024,
                "provider response exceeds 1 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("invalid provider JSON")
    }
}

// Never format reqwest's error: it may include the URL or private
// transport details. Stage names, measured duration and byte count are safe.
fn transport_error(
    error: &reqwest::Error,
    phase: &'static str,
    started: Instant,
    request_bytes: usize,
) -> anyhow::Error {
    let message = if error.is_timeout() {
        "provider request timed out"
    } else if error.is_connect() {
        "provider connection failed"
    } else {
        "provider transport failed"
    };
    crate::failure::at(if error.is_timeout() {
        crate::failure::Kind::ProviderTimeout
    } else {
        crate::failure::Kind::Provider
    })(anyhow::anyhow!(
        "{message}: phase={phase} elapsed_ms={} request_bytes={request_bytes}",
        started.elapsed().as_millis()
    ))
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
}
