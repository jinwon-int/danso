//! Bounded, credential-safe transport shared by the Bearer-authenticated adapters.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::time::Duration;

pub struct Http {
    client: reqwest::Client,
    url: reqwest::Url,
    key: reqwest::header::HeaderValue,
}
impl Http {
    pub fn new(base: &str, suffix: &str, key: &str) -> Result<Self> {
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
            .timeout(Duration::from_secs(60))
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
        let request = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::AUTHORIZATION, self.key.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .build()
            .map_err(|_| anyhow::anyhow!("could not construct provider request"))?;
        usage.attempted = true;
        let mut response = self.client.execute(request).await.map_err(|e| {
            anyhow::anyhow!(if e.is_timeout() {
                "provider request timed out"
            } else if e.is_connect() {
                "provider connection failed"
            } else {
                "provider request failed"
            })
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
            .map_err(|_| anyhow::anyhow!("provider response read failed"))?
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
