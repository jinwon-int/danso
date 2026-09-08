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
        }
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
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;
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
