//! Optional, bounded Z.AI diagnostics. Never expose provider-controlled prose.
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, serde::Serialize)]
pub struct HttpDiagnostic {
    #[serde(skip)]
    source: Option<anyhow::Error>,
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
impl std::error::Error for HttpDiagnostic {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e.as_ref())
    }
}
impl HttpDiagnostic {
    pub fn with_source(mut self, error: anyhow::Error) -> anyhow::Error {
        self.source = Some(error);
        anyhow::Error::new(self)
    }

    /// Z.AI quota/subscription exhaustion, per the official error table
    /// (docs.z.ai/api-reference/api-code): 1113 no resource package,
    /// 1308 usage-limit window, 1309 expired plan, 1310 weekly/monthly
    /// limit, 1311 plan lacks the model, 1313 fair-usage throttle,
    /// 1314/1315 enterprise package/key mismatch, 1316..=1321 5-hour/7-day
    /// windows. Each resets in hours or days — or needs account action —
    /// so the bounded retry schedule (waits capped at 60 s) cannot
    /// recover: the turn fails closed on the first response instead of
    /// burning retries against the wall clock. Transient 429s (1302
    /// request rate, 1305 overload) and 429s without an allowlisted code
    /// keep the normal retry schedule.
    pub fn quota_exhausted(&self) -> bool {
        matches!(self.provider_code, Some(1113 | 1308..=1311 | 1313..=1321))
    }
}

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
        source: None,
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

    #[test]
    fn quota_exhaustion_covers_the_official_non_transient_429_family() {
        let diagnostic = |provider_code: Option<u16>| HttpDiagnostic {
            source: None,
            version: 1,
            provider: "zai",
            http_status: 429,
            provider_code,
            retry_after_seconds: None,
        };
        // Official table: every 429 that resets in hours/days or needs
        // account action.
        for code in [
            1113, 1308, 1309, 1310, 1311, 1313, 1314, 1315, 1316, 1319, 1321,
        ] {
            assert!(diagnostic(Some(code)).quota_exhausted(), "{code}");
        }
        // Transient 429s stay on the retry schedule; an unclassifiable
        // body (code omitted) must not fail fast either.
        for code in [None, Some(1302), Some(1305), Some(1214)] {
            assert!(!diagnostic(code).quota_exhausted(), "{code:?}");
        }
    }
}
