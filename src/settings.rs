//! One resolution layer for what the long-running service reads:
//! environment > `config.toml` > default (docs/unified-design.md §6.1, #136).
//!
//! Before this module the file was parsed and validated and then, for 35 of
//! its 38 keys, ignored: `doctor`, `backup` and `update` read four keys
//! between them and the Telegram service read none. Every value the service
//! needs now goes through [`Layered`], so a key that the file declares is a
//! key the service honours, and an error names the place the value actually
//! came from rather than the first name in a list of aliases.
//!
//! The CLI (`danso run`) stays flag-driven; it is not routed through here.
use crate::config::{self, Config};
use crate::telegram::{
    ALLOWED_USER_IDS_ENV, BOT_TOKEN_ENV, EFFORT_ENV, MODEL_ENV, PROVIDER_ENV, WORKSPACE_ENV,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};
use std::fmt;
use std::path::{Path, PathBuf};

/// The provider the service assumes when neither source names one.
pub const DEFAULT_PROVIDER: &str = "anthropic";

/// A `config.toml` key some command reads, with the environment aliases
/// that outrank it; the first one set wins. Readers take their alias lists
/// from here (`aliases`) and `config check` reports from the same rows, so a
/// name the service accepts and a name the report claims cannot drift apart.
#[derive(Debug)]
pub struct KeySource {
    pub key: &'static str,
    pub env: &'static [&'static str],
}

/// Every declared key that is read, in the order the file declares them.
/// A key with no alias (`update.*`) is file-or-default only. The three
/// keys the resolution layer does not route through `resolve_*` still
/// belong here: the token and allowlist (`telegram::TelegramConfig`) and the
/// memory root (`memory::MemoryConfig::resolve_root`) read the same names.
pub const KEY_SOURCES: &[KeySource] = &[
    KeySource {
        key: "core.workspace",
        env: &[WORKSPACE_ENV, "DANSO_WORKSPACE"],
    },
    KeySource {
        key: "core.timeout_seconds",
        env: &["DANSO_TELEGRAM_TIMEOUT_SECONDS", "DANSO_TIMEOUT_SECONDS"],
    },
    KeySource {
        key: "core.max_turns",
        env: &["DANSO_TELEGRAM_MAX_TURNS", "DANSO_MAX_TURNS"],
    },
    KeySource {
        key: "core.tool_timeout_seconds",
        env: &[
            "DANSO_TELEGRAM_TOOL_TIMEOUT_SECONDS",
            "DANSO_TOOL_TIMEOUT_SECONDS",
        ],
    },
    KeySource {
        key: "provider.name",
        env: &[PROVIDER_ENV, "DANSO_PROVIDER"],
    },
    // Plus the provider-specific alias `model_env_names` appends once the
    // provider is known.
    KeySource {
        key: "provider.model",
        env: &[MODEL_ENV, "DANSO_MODEL"],
    },
    KeySource {
        key: "provider.reasoning_effort",
        env: &[EFFORT_ENV, "DANSO_REASONING_EFFORT"],
    },
    KeySource {
        key: "provider.retries",
        env: &["DANSO_TELEGRAM_PROVIDER_RETRIES", "DANSO_PROVIDER_RETRIES"],
    },
    KeySource {
        key: "provider.timeout_seconds",
        env: &[
            "DANSO_TELEGRAM_PROVIDER_TIMEOUT_SECONDS",
            "DANSO_PROVIDER_TIMEOUT_SECONDS",
        ],
    },
    KeySource {
        key: "provider.max_output_tokens",
        env: &[
            "DANSO_TELEGRAM_MAX_OUTPUT_TOKENS",
            "DANSO_MAX_OUTPUT_TOKENS",
        ],
    },
    KeySource {
        key: "memory.dir",
        env: &[crate::memory::DIR_ENV],
    },
    KeySource {
        key: "memory.scope",
        env: &["DANSO_TELEGRAM_MEMORY_SCOPE"],
    },
    // The variable carries the token itself, the key names a file holding
    // it; either way it is where the token came from.
    KeySource {
        key: "telegram.token_file",
        env: &[BOT_TOKEN_ENV],
    },
    KeySource {
        key: "telegram.allowed_user_ids",
        env: &[ALLOWED_USER_IDS_ENV],
    },
    KeySource {
        key: "telegram.heartbeat_seconds",
        env: &["DANSO_TELEGRAM_HEARTBEAT_SECONDS"],
    },
    KeySource {
        key: "update.public_key",
        env: &[],
    },
    KeySource {
        key: "update.source",
        env: &[],
    },
    KeySource {
        key: "update.enforce_signature",
        env: &[],
    },
];

/// The aliases a reader consults for `key`. A key outside the table is a
/// programming error, and the first test that resolves it says so.
pub fn aliases(key: &str) -> &'static [&'static str] {
    KEY_SOURCES
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.env)
        .unwrap_or_else(|| panic!("{key} is not in settings::KEY_SOURCES"))
}

/// Every environment name the service accepts a model under, most specific
/// first. `install` preflight uses the same list so it never refuses a unit
/// the service would start.
pub fn model_env_names(provider: &str) -> Vec<&'static str> {
    let mut names = aliases("provider.model").to_vec();
    match provider {
        "anthropic" => names.push("DANSO_ANTHROPIC_MODEL"),
        "openai" => names.push("DANSO_OPENAI_MODEL"),
        "openai-codex" => {
            names.push("DANSO_OPENAI_CODEX_MODEL");
            names.push("DANSO_OPENAI_MODEL");
        }
        "glm" => names.push("DANSO_GLM_MODEL"),
        _ => {}
    }
    names
}

/// `config check`'s `sources` (#136): for every key in [`KEY_SOURCES`],
/// `env:<NAME>` when an alias is set (its name, never what it holds), `file`
/// when only the file names the key, else `default`. Presence is what
/// `env_first` tests, so a set-but-empty variable reports `env` exactly as
/// it resolves; a required key with neither reports `default` and the
/// service refuses to start, which is the same fact seen from two sides.
pub fn sources(config: &Config) -> Map<String, Value> {
    let declared = config.declared();
    // The provider-specific model alias depends on which provider the
    // service will resolve; deciding it here the same way is what keeps
    // `env:DANSO_GLM_MODEL` from being claimed on an Anthropic node.
    let provider = aliases("provider.name")
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .or_else(|| config.provider.name.clone())
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
    KEY_SOURCES
        .iter()
        .map(|entry| {
            let names = match entry.key {
                "provider.model" => model_env_names(&provider),
                _ => entry.env.to_vec(),
            };
            let in_file = declared.iter().any(|(key, set)| *key == entry.key && *set);
            let source = match names.iter().find(|name| std::env::var_os(name).is_some()) {
                Some(name) => format!("env:{name}"),
                None if in_file => "file".to_string(),
                None => "default".to_string(),
            };
            (entry.key.to_string(), Value::String(source))
        })
        .collect()
}

/// `config.toml`, if present, beneath the process environment.
#[derive(Debug)]
pub struct Layered {
    config: Option<Config>,
    path: PathBuf,
}

impl Layered {
    /// Load `$DANSO_HOME/config.toml`. A missing file is fine; a file that is
    /// present but does not parse or validate is an error for every command,
    /// the same way `update` and `backup` already treat it. Running without a
    /// file that exists would silently drop whatever it declared.
    pub fn load() -> Result<Self> {
        let path = config::default_path()?;
        let config = match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", path.display()));
            }
            Ok(_) => Some(
                Config::load(&path)
                    .with_context(|| format!("{} is present but invalid", path.display()))?,
            ),
        };
        Ok(Self { config, path })
    }

    /// No file at all: environment and defaults only.
    pub fn without_file() -> Self {
        Self {
            config: None,
            path: PathBuf::new(),
        }
    }

    pub fn with_config(config: Config) -> Self {
        Self {
            config: Some(config),
            path: PathBuf::new(),
        }
    }

    pub fn config(&self) -> Option<&Config> {
        self.config.as_ref()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Where a resolved value came from, for the error that rejects it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Env(String),
    File(&'static str),
    Default,
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Env(name) => formatter.write_str(name),
            Source::File(key) => write!(formatter, "config.toml {key}"),
            Source::Default => formatter.write_str("default"),
        }
    }
}

/// The first of `names` that is set, with which one it was. A set-but-empty
/// variable counts as set: callers that need "empty means default" say so.
pub fn env_first(names: &[&str]) -> Result<Option<(String, Source)>> {
    for name in names {
        match std::env::var(name) {
            Ok(value) => return Ok(Some((value, Source::Env((*name).to_string())))),
            Err(std::env::VarError::NotPresent) => {}
            Err(std::env::VarError::NotUnicode(_)) => bail!("{name} must be valid UTF-8"),
        }
    }
    Ok(None)
}

/// A string: environment first, then the file key, else `None`.
pub fn resolve_string(
    layered: &Layered,
    names: &[&str],
    key: &'static str,
    file: impl FnOnce(&Config) -> Option<String>,
) -> Result<Option<(String, Source)>> {
    if let Some(found) = env_first(names)? {
        return Ok(Some(found));
    }
    Ok(layered
        .config()
        .and_then(file)
        .map(|value| (value, Source::File(key))))
}

fn parse_env_integer<T: std::str::FromStr>(raw: &str, source: &Source) -> Result<T> {
    ensure!(!raw.trim().is_empty(), "{source} must not be empty");
    raw.trim()
        .parse::<T>()
        .map_err(|_| anyhow::anyhow!("{source} must be an integer"))
}

/// A bounded integer: environment, then file, then `default`. The range is
/// enforced on whichever source supplied the value, and the error says which.
pub fn resolve_u64(
    layered: &Layered,
    names: &[&str],
    key: &'static str,
    file: impl FnOnce(&Config) -> Option<u64>,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64> {
    let (value, source) = match env_first(names)? {
        Some((raw, source)) => (parse_env_integer::<u64>(&raw, &source)?, source),
        None => match layered.config().and_then(file) {
            Some(value) => (value, Source::File(key)),
            None => return Ok(default),
        },
    };
    ensure!(
        (min..=max).contains(&value),
        "{source} must be {min}..={max}"
    );
    Ok(value)
}

pub fn resolve_u32(
    layered: &Layered,
    names: &[&str],
    key: &'static str,
    file: impl FnOnce(&Config) -> Option<u32>,
    default: u32,
    min: u32,
    max: u32,
) -> Result<u32> {
    let (value, source) = match env_first(names)? {
        Some((raw, source)) => (parse_env_integer::<u32>(&raw, &source)?, source),
        None => match layered.config().and_then(file) {
            Some(value) => (value, Source::File(key)),
            None => return Ok(default),
        },
    };
    ensure!(
        (min..=max).contains(&value),
        "{source} must be {min}..={max}"
    );
    Ok(value)
}

/// An optional integer with no default: environment, then file.
pub fn resolve_optional_u32(
    layered: &Layered,
    names: &[&str],
    key: &'static str,
    file: impl FnOnce(&Config) -> Option<u32>,
) -> Result<Option<u32>> {
    let _ = key;
    if let Some((raw, source)) = env_first(names)? {
        return Ok(Some(parse_env_integer::<u32>(&raw, &source)?));
    }
    Ok(layered.config().and_then(file))
}

/// An optional integer with no file key.
pub fn env_optional_usize(names: &[&str]) -> Result<Option<usize>> {
    match env_first(names)? {
        Some((raw, source)) => Ok(Some(parse_env_integer::<usize>(&raw, &source)?)),
        None => Ok(None),
    }
}

/// A boolean with no file key.
pub fn env_bool(names: &[&str], default: bool) -> Result<bool> {
    let Some((raw, source)) = env_first(names)? else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("{source} must be a boolean"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Process environment is global; tests that set it take this lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn with_env<T>(pairs: &[(&str, Option<&str>)], body: impl FnOnce() -> T) -> T {
        let _lock = env_lock();
        let saved: Vec<_> = pairs
            .iter()
            .map(|(name, _)| (name.to_string(), std::env::var_os(name)))
            .collect();
        for (name, value) in pairs {
            match value {
                // SAFETY: serialised by ENV_LOCK; no other thread reads the
                // environment while a test holds it.
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
        let result = body();
        for (name, value) in saved {
            match value {
                Some(value) => unsafe { std::env::set_var(&name, value) },
                None => unsafe { std::env::remove_var(&name) },
            }
        }
        result
    }

    fn file(text: &str) -> Layered {
        Layered::with_config(Config::parse(text).expect("fixture config"))
    }

    #[test]
    fn environment_beats_file_beats_default() {
        let layered = file("[core]\ntimeout_seconds = 120\n");
        with_env(&[("DANSO_TIMEOUT_SECONDS", Some("30"))], || {
            assert_eq!(
                resolve_u64(
                    &layered,
                    &["DANSO_TELEGRAM_TIMEOUT_SECONDS", "DANSO_TIMEOUT_SECONDS"],
                    "core.timeout_seconds",
                    |c| c.core.timeout_seconds,
                    900,
                    1,
                    3600
                )
                .unwrap(),
                30
            );
        });
        with_env(
            &[
                ("DANSO_TELEGRAM_TIMEOUT_SECONDS", None),
                ("DANSO_TIMEOUT_SECONDS", None),
            ],
            || {
                assert_eq!(
                    resolve_u64(
                        &layered,
                        &["DANSO_TELEGRAM_TIMEOUT_SECONDS", "DANSO_TIMEOUT_SECONDS"],
                        "core.timeout_seconds",
                        |c| c.core.timeout_seconds,
                        900,
                        1,
                        3600
                    )
                    .unwrap(),
                    120
                );
                assert_eq!(
                    resolve_u64(
                        &Layered::without_file(),
                        &["DANSO_TELEGRAM_TIMEOUT_SECONDS", "DANSO_TIMEOUT_SECONDS"],
                        "core.timeout_seconds",
                        |c| c.core.timeout_seconds,
                        900,
                        1,
                        3600
                    )
                    .unwrap(),
                    900
                );
            },
        );
    }

    #[test]
    fn errors_name_the_source_that_supplied_the_value() {
        // The second alias is the one set; the error must say so, not name
        // the first alias in the list.
        with_env(
            &[
                ("DANSO_TELEGRAM_TIMEOUT_SECONDS", None),
                ("DANSO_TIMEOUT_SECONDS", Some("7200")),
            ],
            || {
                let error = resolve_u64(
                    &Layered::without_file(),
                    &["DANSO_TELEGRAM_TIMEOUT_SECONDS", "DANSO_TIMEOUT_SECONDS"],
                    "core.timeout_seconds",
                    |c| c.core.timeout_seconds,
                    900,
                    1,
                    3600,
                )
                .unwrap_err()
                .to_string();
                assert_eq!(error, "DANSO_TIMEOUT_SECONDS must be 1..=3600");
            },
        );
        let layered = file("[core]\ntimeout_seconds = 7200\n");
        with_env(
            &[
                ("DANSO_TELEGRAM_TIMEOUT_SECONDS", None),
                ("DANSO_TIMEOUT_SECONDS", None),
            ],
            || {
                let error = resolve_u64(
                    &layered,
                    &["DANSO_TELEGRAM_TIMEOUT_SECONDS", "DANSO_TIMEOUT_SECONDS"],
                    "core.timeout_seconds",
                    |c| c.core.timeout_seconds,
                    900,
                    1,
                    3600,
                )
                .unwrap_err()
                .to_string();
                assert_eq!(error, "config.toml core.timeout_seconds must be 1..=3600");
            },
        );
    }

    #[test]
    fn strings_fall_back_to_the_file_and_report_where_from() {
        let layered = file("[provider]\nmodel = \"file-model\"\n");
        with_env(
            &[("DANSO_TELEGRAM_MODEL", None), ("DANSO_MODEL", None)],
            || {
                let (value, source) = resolve_string(
                    &layered,
                    &["DANSO_TELEGRAM_MODEL", "DANSO_MODEL"],
                    "provider.model",
                    |c| c.provider.model.clone(),
                )
                .unwrap()
                .unwrap();
                assert_eq!(value, "file-model");
                assert_eq!(source, Source::File("provider.model"));
            },
        );
        with_env(&[("DANSO_MODEL", Some("env-model"))], || {
            let (value, source) = resolve_string(
                &layered,
                &["DANSO_TELEGRAM_MODEL", "DANSO_MODEL"],
                "provider.model",
                |c| c.provider.model.clone(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(value, "env-model");
            assert_eq!(source, Source::Env("DANSO_MODEL".into()));
        });
    }

    /// The table is the read keys, no more and no fewer: a key read through
    /// an alias the table does not know reports the wrong source, and a key
    /// in the table that `UNREAD_KEYS` also lists claims a reader it lacks.
    #[test]
    fn key_sources_are_exactly_the_read_keys() {
        let declared = Config::default().declared();
        let read: Vec<&str> = declared
            .iter()
            .map(|(key, _)| *key)
            .filter(|key| !config::UNREAD_KEYS.contains(key))
            .collect();
        let table: Vec<&str> = KEY_SOURCES.iter().map(|entry| entry.key).collect();
        for key in &read {
            assert!(
                table.contains(key),
                "{key} is read but has no KEY_SOURCES row"
            );
        }
        for key in &table {
            assert!(
                read.contains(key),
                "{key} is in KEY_SOURCES but not a read key"
            );
        }
        let unique: std::collections::BTreeSet<&str> = table.iter().copied().collect();
        assert_eq!(unique.len(), table.len(), "a key twice: {table:?}");
        for alias in KEY_SOURCES.iter().flat_map(|entry| entry.env) {
            assert!(alias.starts_with("DANSO_"), "{alias}");
        }
        assert!(model_env_names("glm").starts_with(aliases("provider.model")));
    }

    #[test]
    fn a_present_but_invalid_file_refuses_to_load() {
        let home = tempfile::tempdir().unwrap();
        let danso_home = home.path().join(".danso");
        std::fs::create_dir_all(&danso_home).unwrap();
        std::fs::write(danso_home.join("config.toml"), "[cron]\nenbled = true\n").unwrap();
        with_env(
            &[("DANSO_HOME", Some(danso_home.to_str().unwrap()))],
            || {
                let error = Layered::load().unwrap_err().to_string();
                assert!(error.contains("present but invalid"), "{error}");
                std::fs::remove_file(danso_home.join("config.toml")).unwrap();
                assert!(Layered::load().unwrap().config().is_none());
            },
        );
    }
}
