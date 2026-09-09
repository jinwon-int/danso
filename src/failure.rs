//! Stable, body-free failure categories. Never infer categories from error text.
use anyhow::Error;
use serde::Serialize;
use std::fmt;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Configuration,
    Session,
    Sandbox,
    Provider,
    ProviderTimeout,
    Compaction,
    RequestBudget,
    Output,
    Runtime,
    RunTimeout,
    Interrupted,
    Memory,
}

/// Safe metadata for a failure raised by the native HTTP transport.
///
/// This is deliberately a separate typed source from `DANSO_ERROR`: the
/// latter is the stable three-field failure category contract, while this
/// optional record carries only bounded, non-sensitive transport facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportPhase {
    Connect,
    BeforeResponseHeaders,
    ResponseBody,
}

impl TransportPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::BeforeResponseHeaders => "before_response_headers",
            Self::ResponseBody => "response_body",
        }
    }
}

#[derive(Debug)]
pub struct TransportDiagnostic {
    phase: TransportPhase,
    elapsed_ms: u64,
    request_bytes: u64,
    timed_out: bool,
    attempts: u32,
}

impl TransportDiagnostic {
    pub(crate) fn new(
        phase: TransportPhase,
        elapsed_ms: u64,
        request_bytes: u64,
        timed_out: bool,
    ) -> Self {
        Self {
            phase,
            elapsed_ms,
            request_bytes,
            timed_out,
            attempts: 1,
        }
    }

    /// Wire attempts made before this diagnostic fired (issue #67 B): 1 on
    /// the first try, more when bounded retries ran.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub(crate) fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }

    pub fn phase(&self) -> TransportPhase {
        self.phase
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.elapsed_ms
    }

    pub fn request_bytes(&self) -> u64 {
        self.request_bytes
    }
}

impl fmt::Display for TransportDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = if self.timed_out {
            "provider request timed out"
        } else if self.phase == TransportPhase::Connect {
            "provider connection failed"
        } else {
            "provider transport failed"
        };
        write!(
            f,
            "{message}: phase={} elapsed_ms={} request_bytes={}",
            self.phase.as_str(),
            self.elapsed_ms,
            self.request_bytes
        )
    }
}

impl std::error::Error for TransportDiagnostic {}

#[derive(Debug)]
struct Tagged {
    kind: Kind,
    source: Error,
}
impl fmt::Display for Tagged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}
impl std::error::Error for Tagged {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub fn category(error: &Error) -> Option<Kind> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Tagged>().map(|e| e.kind))
}

pub fn transport(error: &Error) -> Option<&TransportDiagnostic> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TransportDiagnostic>())
}

/// Closed vocabulary: never populate diagnostics from provider-controlled text.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderReason {
    HttpStatus,
    InvalidJson,
    ResponseTooLarge,
    StreamEnded,
    InvalidStream,
    UnsupportedStreamEvent,
    ResponseFailed,
    ResponseIncomplete,
    ResponseError,
    /// The provider stopped at its output token cap (issue #69 B):
    /// `stop_reason=max_tokens` / `stopReason=length` with no tool calls.
    MaxTokens,
}

#[derive(Debug, Serialize)]
pub struct ProviderDiagnostic {
    reason: ProviderReason,
    http_status: Option<u16>,
    output_tokens_max: Option<u32>,
}

impl ProviderDiagnostic {
    /// HTTP status when the failure came from an HTTP-level provider error.
    /// Diagnostic-only elsewhere; the distill drain classifier reads this to
    /// map provider failures onto retry classes (§4.7) without parsing text.
    pub fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    /// Closed-vocabulary reason (tests and ccc-node validation read this;
    /// the record itself is already body-free JSON).
    pub fn reason(&self) -> ProviderReason {
        self.reason
    }
}

impl fmt::Display for ProviderDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "provider response failure: {:?}", self.reason)?;
        if let Some(status) = self.http_status {
            write!(f, " HTTP {status}")?;
        }
        if let Some(cap) = self.output_tokens_max {
            write!(f, " output token cap {cap}; raise --max-output-tokens")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProviderDiagnostic {}

pub fn provider(error: &Error) -> Option<&ProviderDiagnostic> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ProviderDiagnostic>())
}

pub fn provider_error(reason: ProviderReason) -> Error {
    Error::new(ProviderDiagnostic {
        reason,
        http_status: None,
        output_tokens_max: None,
    })
}

pub fn http_status_error(status: reqwest::StatusCode) -> Error {
    Error::new(ProviderDiagnostic {
        reason: ProviderReason::HttpStatus,
        http_status: Some(status.as_u16()),
        output_tokens_max: None,
    })
}

/// The response stopped at the output token cap (issue #69 B): terminal
/// `length` stop with no tool calls. Body-free diagnostic carries the cap;
/// the display text names the flag that raises it.
pub fn max_tokens_error(output_tokens_max: u32) -> Error {
    Error::new(ProviderDiagnostic {
        reason: ProviderReason::MaxTokens,
        http_status: None,
        output_tokens_max: Some(output_tokens_max),
    })
}

pub fn provider_context(reason: ProviderReason) -> impl FnOnce(Error) -> Error {
    move |error| {
        if provider(&error).is_some() || transport(&error).is_some() {
            error
        } else {
            provider_error(reason)
        }
    }
}

pub fn report_provider(diagnostic: &ProviderDiagnostic) {
    eprintln!(
        "DANSO_PROVIDER={}",
        serde_json::json!({
            "version": 1, "reason": diagnostic.reason, "http_status": diagnostic.http_status,
            "output_tokens_max": diagnostic.output_tokens_max,
        })
    );
}

pub fn at(kind: Kind) -> impl FnOnce(Error) -> Error {
    move |error| {
        if category(&error).is_some() {
            error
        } else {
            Error::new(Tagged {
                kind,
                source: error,
            })
        }
    }
}

pub fn report(kind: Kind, exit_code: i32) {
    eprintln!(
        "DANSO_ERROR={}",
        serde_json::json!({
            "version": 1, "category": kind, "exit_code": exit_code
        })
    );
}

pub fn report_transport(diagnostic: &TransportDiagnostic) {
    eprintln!(
        "DANSO_TRANSPORT={}",
        serde_json::json!({
            "version": 1,
            "phase": diagnostic.phase.as_str(),
            "elapsed_ms": diagnostic.elapsed_ms,
            "request_bytes": diagnostic.request_bytes,
            "attempts": diagnostic.attempts,
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_diagnostic_survives_category_without_copying_error_text() {
        let error = provider_context(ProviderReason::InvalidStream)(anyhow::anyhow!("PRIVATE"));
        let error = at(Kind::Provider)(error).context("outer PRIVATE context");
        let error = provider_context(ProviderReason::ResponseError)(error);
        assert_eq!(category(&error), Some(Kind::Provider));
        assert_eq!(
            provider(&error).unwrap().reason,
            ProviderReason::InvalidStream
        );
        assert_eq!(
            serde_json::to_value(provider(&error).unwrap()).unwrap(),
            serde_json::json!({"reason":"invalid_stream", "http_status":null, "output_tokens_max":null})
        );
        let transport_error = Error::new(TransportDiagnostic::new(
            TransportPhase::ResponseBody,
            1,
            1,
            false,
        ));
        let error = provider_context(ProviderReason::InvalidStream)(transport_error);
        assert!(provider(&error).is_none());
        assert!(transport(&error).is_some());
    }

    #[test]
    fn typed_categories_survive_context_and_ignore_untrusted_text() {
        let error = anyhow::anyhow!("PRIVATE provider_timeout compaction request_budget");
        assert_eq!(category(&error), None);
        let error = at(Kind::ProviderTimeout)(error).context("PRIVATE extra context");
        let error = at(Kind::Compaction)(error);
        assert_eq!(category(&error), Some(Kind::ProviderTimeout));
        assert_eq!(
            serde_json::to_string(&category(&error)).unwrap(),
            "\"provider_timeout\""
        );
    }

    #[test]
    fn transport_record_is_bounded_and_body_free() {
        let diagnostic =
            TransportDiagnostic::new(TransportPhase::ResponseBody, u64::MAX, 512 * 1024, true);
        assert_eq!(diagnostic.phase(), TransportPhase::ResponseBody);
        assert_eq!(diagnostic.elapsed_ms(), u64::MAX);
        assert_eq!(diagnostic.request_bytes(), 512 * 1024);
        let error = at(Kind::ProviderTimeout)(Error::new(diagnostic));
        assert_eq!(category(&error), Some(Kind::ProviderTimeout));
        let diagnostic = transport(&error).unwrap();
        assert_eq!(diagnostic.phase(), TransportPhase::ResponseBody);
        assert!(!diagnostic.to_string().contains("http"));
        assert!(!diagnostic.to_string().contains("PRIVATE"));
    }
}
