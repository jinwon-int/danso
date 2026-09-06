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
}
