//! Optional, bounded Z.AI diagnostics. Never expose provider-controlled prose.
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, serde::Serialize)]
pub struct HttpDiagnostic {
    pub version: u8,
    pub provider: &'static str,
    pub http_status: u16,
    pub provider_code: Option<u16>,
    pub retry_after_seconds: Option<u32>,
}
impl std::fmt::Display for HttpDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Z.AI HTTP diagnostic")
    }
}
impl std::error::Error for HttpDiagnostic {}

#[derive(Deserialize)]
struct Envelope {
    error: Code,
}
#[derive(Deserialize)]
struct Code {
    code: serde_json::Value,
}
fn code(bytes: &[u8]) -> Option<u16> {
    let value = serde_json::from_slice::<Envelope>(bytes).ok()?.error.code;
    let n = match value {
        serde_json::Value::Number(n) => u16::try_from(n.as_u64()?).ok()?,
        serde_json::Value::String(s) if s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse().ok()?
        }
        _ => return None,
    };
    matches!(n, 1113 | 1302 | 1305 | 1308..=1311 | 1313..=1321).then_some(n)
}
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<u32> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    // Preserve the advertised seconds, not the retry scheduler's 60-second cap.
    // Dates and noncanonical values are omitted; no untrusted header is printed.
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = raw.parse().ok()?;
    (n <= 86400).then_some(n)
}

pub async fn capture(mut response: reqwest::Response) -> HttpDiagnostic {
    let status = response.status().as_u16();
    let retry_after_seconds = retry_after(response.headers());
    // A diagnostic body cannot replace or indefinitely delay the known HTTP error.
    let provider_code = tokio::time::timeout(Duration::from_secs(1), async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > 16384 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        code(&bytes)
    })
    .await
    .ok()
    .flatten();
    HttpDiagnostic {
        version: 1,
        provider: "zai",
        http_status: status,
        provider_code,
        retry_after_seconds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codes_are_allowlisted_and_duplicates_rejected() {
        for n in [1113, 1302, 1305, 1308, 1313, 1321] {
            assert_eq!(
                code(format!(r#"{{"error":{{"code":"{n}","message":"SECRET"}}}}"#).as_bytes()),
                Some(n)
            );
        }
        for s in [
            r#"{"error":{"code":true}}"#,
            r#"{"error":{"code":1305.0}}"#,
            r#"{"error":{"code":9999}}"#,
            r#"{"error":{"code":"SECRET"}}"#,
            r#"{"error":{"code":1302,"code":1305}}"#,
            r#"{"error":{"code":1302},"error":{"code":1305}}"#,
        ] {
            assert_eq!(code(s.as_bytes()), None);
        }
    }
    #[test]
    fn retry_header_is_bounded_and_not_scheduler_clamped() {
        let mut h = reqwest::header::HeaderMap::new();
        for (s, want) in [
            ("120", Some(120)),
            ("0", Some(0)),
            ("86400", Some(86400)),
            ("86401", None),
            ("-1", None),
            ("SECRET", None),
        ] {
            h.insert(reqwest::header::RETRY_AFTER, s.parse().unwrap());
            assert_eq!(retry_after(&h), want);
        }
    }
}
