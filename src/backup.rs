//! Body-free state-home backup and explicit-target restore (roadmap #33 C2).
//!
//! The backup command is intentionally a synchronous, provider-free file
//! operation.  It reads the same home and Telegram data-directory resolvers
//! as the other inspection commands, but it never opens a lock or constructs
//! a service.  A backup is assembled in a private temporary sibling and is
//! exposed only by one final rename.

use chrono::{SecondsFormat, Utc};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use crate::{config, memory, telegram};

pub const BACKUP_DIR_ENV: &str = "DANSO_BACKUP_DIR";
pub const MANIFEST_FILE_NAME: &str = "manifest.json";
pub const BACKUP_PREFIX: &str = "backup-";
pub const TEMP_PREFIX: &str = ".tmp-";

const MEMORY_DIR_NAME: &str = "memory";
const CONVERSATIONS_DIR_NAME: &str = "conversations";
const HEALTH_FILE_NAME: &str = "health.json";
const TOKEN_LOCK_FILE_NAME: &str = ".telegram-token.lock";
const CONFIG_COMPONENT: &str = "config";
const MEMORY_COMPONENT: &str = "memory";
const CONVERSATIONS_COMPONENT: &str = "conversations";
const EXCLUSIONS: [&str; 3] = [
    "telegram.token_file",
    "telegram.token_lock",
    "telegram.health",
];

// Match the doctor's per-directory bound.  The depth bound additionally
// prevents a malicious state tree from consuming an unbounded call stack.
const MAX_SCAN_ENTRIES: usize = 4096;
const MAX_SCAN_DEPTH: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

const WARNING_MISSING: &str = "missing";
const WARNING_UNREADABLE: &str = "unreadable";
const WARNING_LAYOUT: &str = "layout";
const WARNING_SCAN_BOUND: &str = "scan_bound";
/// The component's pre-#136 root (`$HOME/.danso/<name>`) still holds
/// entries that this snapshot does not contain (#177).
const WARNING_LEGACY_STATE: &str = "legacy_state";
const VALID_WARNINGS: [&str; 5] = [
    WARNING_MISSING,
    WARNING_UNREADABLE,
    WARNING_LAYOUT,
    WARNING_SCAN_BOUND,
    WARNING_LEGACY_STATE,
];

const CATEGORY_HOME_UNAVAILABLE: &str = "home_unavailable";
const CATEGORY_HOME_UNREADABLE: &str = "home_unreadable";
const CATEGORY_BACKUP_DIR_UNAVAILABLE: &str = "backup_dir_unavailable";
const CATEGORY_BACKUP_EXISTS: &str = "backup_exists";
const CATEGORY_CONFIG_UNREADABLE: &str = "config_unreadable";
const CATEGORY_CONFIG_INVALID: &str = "config_invalid";
const CATEGORY_WRITE_FAILED: &str = "write_failed";
const CATEGORY_BACKUP_MISSING: &str = "backup_missing";
const CATEGORY_INVALID_BACKUP: &str = "invalid_backup";
const CATEGORY_BACKUP_UNREADABLE: &str = "backup_unreadable";
const CATEGORY_CREDENTIAL_MATERIAL: &str = "credential_material";
const CATEGORY_TARGET_REQUIRED: &str = "target_required";
const CATEGORY_TARGET_UNSAFE: &str = "target_unsafe";
const CATEGORY_TARGET_NOT_EMPTY: &str = "target_not_empty";
const CATEGORY_TARGET_CONFIG: &str = "target_contains_config";

/// The fixed failure class used by the CLI.  The contained category is never
/// an operating-system error, path, or file content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    CannotRun,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandError {
    kind: ErrorKind,
    category: &'static str,
}

impl CommandError {
    const fn cannot_run(category: &'static str) -> Self {
        Self {
            kind: ErrorKind::CannotRun,
            category,
        }
    }

    const fn failed(category: &'static str) -> Self {
        Self {
            kind: ErrorKind::Failed,
            category,
        }
    }

    pub fn kind(self) -> ErrorKind {
        self.kind
    }

    pub fn category(self) -> &'static str {
        self.category
    }

    pub fn exit_code(self) -> i32 {
        match self.kind {
            ErrorKind::CannotRun => 2,
            ErrorKind::Failed => 1,
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.category)
    }
}

impl std::error::Error for CommandError {}

/// Arguments for the provider-free `danso backup` command.
#[derive(Debug, Parser)]
#[command(
    name = "danso backup",
    about = "Capture an atomic, body-free snapshot of the durable state home"
)]
pub struct BackupArgs {}

/// Arguments for `danso restore`.  The target is deliberately a required
/// option rather than an implicit home path.
#[derive(Debug, Parser)]
#[command(
    name = "danso restore",
    about = "Restore a backup into an explicit target directory"
)]
pub struct RestoreArgs {
    /// Explicit destination directory; the live home is never inferred.
    #[arg(long)]
    pub target: PathBuf,
    /// Permit restoring into a non-empty target, subject to the config guard.
    #[arg(long)]
    pub force: bool,
    /// Backup directory produced by `danso backup`.
    pub backup_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct RestoreReport {
    pub restored: bool,
    pub target: String,
    pub components: Vec<RestoredComponent>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RestoredComponent {
    pub name: String,
    pub file_count: u64,
    pub byte_count: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u8,
    created_at: String,
    source_home: String,
    components: Vec<ManifestComponent>,
    exclusions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestComponent {
    name: String,
    file_count: u64,
    byte_count: u64,
    warnings: Vec<String>,
    /// Only the config component records a source mode.  A string retains
    /// the leading zero of an octal mode in JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
}

#[derive(Default)]
struct ComponentBuilder {
    file_count: u64,
    byte_count: u64,
    warnings: BTreeSet<String>,
}

impl ComponentBuilder {
    fn warning(&mut self, warning: &'static str) {
        self.warnings.insert(warning.to_string());
    }

    fn add_file(&mut self, bytes: u64) -> Result<(), CommandError> {
        self.file_count = self
            .file_count
            .checked_add(1)
            .ok_or(CommandError::failed(CATEGORY_WRITE_FAILED))?;
        self.byte_count = self
            .byte_count
            .checked_add(bytes)
            .ok_or(CommandError::failed(CATEGORY_WRITE_FAILED))?;
        Ok(())
    }

    fn finish(self, name: &str, mode: Option<String>) -> ManifestComponent {
        ManifestComponent {
            name: name.to_string(),
            file_count: self.file_count,
            byte_count: self.byte_count,
            warnings: self.warnings.into_iter().collect(),
            mode,
        }
    }
}

struct TempGuard {
    path: Option<PathBuf>,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            // The path was created by this invocation and is inside the
            // backup root.  Best-effort cleanup keeps the failure path body
            // free; the operation itself never removes source state.
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyFailure {
    Source,
    Destination,
}

#[derive(Clone, Debug)]
struct RestoreFile {
    source: PathBuf,
    relative: PathBuf,
}

#[derive(Default)]
struct RestoreComponent {
    name: String,
    dirs: Vec<PathBuf>,
    files: Vec<RestoreFile>,
    file_count: u64,
    byte_count: u64,
}

struct RestorePreflight {
    components: Vec<RestoreComponent>,
}

/// Run `danso backup` using the process's configured home and Telegram data
/// directory.  No provider/runtime initialization belongs on this path.
pub fn run_backup() -> Result<PathBuf, CommandError> {
    let home = config::home().map_err(|_| CommandError::cannot_run(CATEGORY_HOME_UNAVAILABLE))?;
    let backup_root = backup_root(&home)?;
    let telegram_data_dir = telegram::data_dir_from_env().ok();
    let legacy_home = config::legacy_home();
    create_at(
        &home,
        &backup_root,
        telegram_data_dir.as_deref(),
        legacy_home.as_deref(),
    )
}

/// Run `danso restore` after resolving relative CLI paths against the current
/// directory.  A missing backup path is a usage/cannot-run condition, while a
/// malformed existing backup is a normal restore failure.
pub fn run_restore(args: &RestoreArgs) -> Result<RestoreReport, CommandError> {
    if args.target.as_os_str().is_empty() {
        return Err(CommandError::cannot_run(CATEGORY_TARGET_REQUIRED));
    }
    let target = absolute_path(&args.target)
        .map_err(|_| CommandError::cannot_run(CATEGORY_TARGET_REQUIRED))?;
    let backup_dir = absolute_path(&args.backup_dir)
        .map_err(|_| CommandError::cannot_run(CATEGORY_BACKUP_MISSING))?;
    match fs::symlink_metadata(&backup_dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(CommandError::cannot_run(CATEGORY_BACKUP_MISSING))
        }
        Err(_) => Err(CommandError::failed(CATEGORY_BACKUP_UNREADABLE)),
        Ok(_) => restore_at(&target, &backup_dir, args.force),
    }
}

fn backup_root(home: &Path) -> Result<PathBuf, CommandError> {
    if let Some(raw) = std::env::var_os(BACKUP_DIR_ENV) {
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(CommandError::failed(CATEGORY_BACKUP_DIR_UNAVAILABLE));
        }
        return Ok(path);
    }
    Ok(home.join("backups"))
}

/// Explicit-path entry point used by the implementation and tests.  The
/// optional Telegram path is already obtained through the existing resolver
/// by `run_backup`; it is explicit here so tests never need process-global
/// environment mutation.  `legacy_home` is `config::legacy_home()`: when a
/// pre-#136 root under it still holds entries the snapshot omits, the
/// matching component carries the `legacy_state` warning (#177) — the
/// archive itself stays what the service under `home` reads and writes.
pub fn create_at(
    home: &Path,
    backup_root: &Path,
    telegram_data_dir: Option<&Path>,
    legacy_home: Option<&Path>,
) -> Result<PathBuf, CommandError> {
    if !home.is_absolute() || !backup_root.is_absolute() {
        return Err(CommandError::failed(CATEGORY_BACKUP_DIR_UNAVAILABLE));
    }
    if telegram_data_dir.is_some_and(|path| !path.is_absolute()) {
        return Err(CommandError::failed(CATEGORY_BACKUP_DIR_UNAVAILABLE));
    }
    ensure_home(home)?;
    ensure_backup_root(backup_root)?;

    let config_path = home.join(config::FILE_NAME);
    // An unreadable or invalid config is not the same as no config. Falling
    // back silently dropped the `telegram.token_file` exclusion and the
    // configured `[memory] dir`, so one typo in an unrelated key could put
    // the bot token into an archive that travels between nodes (#153).
    // A missing file is left to the config component copy below, which
    // already refuses it as `config_unreadable`.
    let parsed_config = match fs::symlink_metadata(&config_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        _ => Some(
            config::Config::load(&config_path)
                .map_err(|_| CommandError::failed(CATEGORY_CONFIG_INVALID))?,
        ),
    };
    // Beneath the `home` this entry point was handed, not a second
    // resolution of the environment: the archive's `memory/` must be the
    // tree the same home's service writes (#136).
    let memory_root = memory::MemoryConfig::root_under(
        home,
        parsed_config
            .as_ref()
            .and_then(|value| value.memory.dir.clone()),
    );
    let token_path = parsed_config
        .as_ref()
        .and_then(|value| value.telegram.token_file.clone());

    let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let backup_name = format!("{BACKUP_PREFIX}{created_at}");
    let backup_path = backup_root.join(&backup_name);
    let temp_path = backup_root.join(format!("{TEMP_PREFIX}{created_at}"));

    if fs::symlink_metadata(&backup_path).is_ok() {
        return Err(CommandError::failed(CATEGORY_BACKUP_EXISTS));
    }
    if fs::symlink_metadata(&temp_path).is_ok() {
        return Err(CommandError::failed(CATEGORY_BACKUP_EXISTS));
    }

    fs::create_dir(&temp_path).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    let mut guard = TempGuard::new(temp_path.clone());
    set_mode(&temp_path, 0o700).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;

    let config_component = build_config_component(
        &config_path,
        &temp_path.join(config::FILE_NAME),
        token_path.as_deref(),
    )?;
    let memory_component = build_memory_component(
        &memory_root,
        &temp_path.join(MEMORY_DIR_NAME),
        token_path.as_deref(),
    )?;
    let mut components = vec![config_component, memory_component];
    if let Some(data_dir) = telegram_data_dir {
        components.push(build_conversations_component(
            data_dir,
            &temp_path.join(CONVERSATIONS_DIR_NAME),
            token_path.as_deref(),
        )?);
    }
    // State the operator has not migrated is not archived and must not look
    // archived: name it in the component it belongs to, as a fixed category
    // like every other omission.  `doctor` spells out the paths.
    for root in config::legacy_roots(home, legacy_home, telegram_data_dir, &memory_root) {
        let name = if root.name == memory::DEFAULT_DIR_NAME {
            MEMORY_COMPONENT
        } else {
            CONVERSATIONS_COMPONENT
        };
        if let Some(component) = components.iter_mut().find(|c| c.name == name)
            && !component.warnings.iter().any(|w| w == WARNING_LEGACY_STATE)
        {
            component.warnings.push(WARNING_LEGACY_STATE.to_string());
            component.warnings.sort();
        }
    }

    let manifest = Manifest {
        version: 1,
        created_at,
        source_home: home.to_string_lossy().into_owned(),
        components,
        exclusions: EXCLUSIONS
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
    };
    write_private_bytes(
        &temp_path.join(MANIFEST_FILE_NAME),
        &serde_json::to_vec(&manifest).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?,
    )?;

    // `rename` is the first point at which the completed backup becomes
    // visible.  Both paths are siblings, so this is one filesystem-atomic
    // directory rename.
    fs::rename(&temp_path, &backup_path)
        .map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    guard.disarm();
    Ok(backup_path)
}

fn ensure_home(home: &Path) -> Result<(), CommandError> {
    match fs::symlink_metadata(home) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(CommandError::failed(CATEGORY_HOME_UNREADABLE)),
        Err(_) => Err(CommandError::failed(CATEGORY_HOME_UNREADABLE)),
    }
}

fn ensure_backup_root(path: &Path) -> Result<(), CommandError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(CommandError::failed(CATEGORY_BACKUP_DIR_UNAVAILABLE)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|_| CommandError::failed(CATEGORY_BACKUP_DIR_UNAVAILABLE))?;
            set_mode(path, 0o700)
                .map_err(|_| CommandError::failed(CATEGORY_BACKUP_DIR_UNAVAILABLE))?;
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
                _ => Err(CommandError::failed(CATEGORY_BACKUP_DIR_UNAVAILABLE)),
            }
        }
        Err(_) => Err(CommandError::failed(CATEGORY_BACKUP_DIR_UNAVAILABLE)),
    }
}

fn build_config_component(
    source: &Path,
    destination: &Path,
    token_path: Option<&Path>,
) -> Result<ManifestComponent, CommandError> {
    let mut builder = ComponentBuilder::default();
    let mode;
    match regular_metadata(source) {
        Ok(metadata) => {
            if is_credential_path(source, token_path) || is_credential_name(source.file_name()) {
                return Err(CommandError::failed(CATEGORY_CONFIG_UNREADABLE));
            }
            match copy_regular_file(source, destination) {
                Ok(bytes) => {
                    builder.add_file(bytes)?;
                    mode = Some(octal_mode(&metadata));
                }
                Err(CopyFailure::Source) => {
                    return Err(CommandError::failed(CATEGORY_CONFIG_UNREADABLE));
                }
                Err(CopyFailure::Destination) => {
                    return Err(CommandError::failed(CATEGORY_WRITE_FAILED));
                }
            }
        }
        Err(_) => {
            return Err(CommandError::failed(CATEGORY_CONFIG_UNREADABLE));
        }
    }
    Ok(builder.finish(CONFIG_COMPONENT, mode))
}

fn build_memory_component(
    source: &Path,
    destination: &Path,
    token_path: Option<&Path>,
) -> Result<ManifestComponent, CommandError> {
    create_private_dir(destination)?;
    let mut builder = ComponentBuilder::default();
    let _metadata = match regular_directory_metadata(source) {
        Ok(metadata) => metadata,
        Err(MetadataFailure::Missing) => {
            builder.warning(WARNING_MISSING);
            return Ok(builder.finish(MEMORY_COMPONENT, None));
        }
        Err(_) => {
            builder.warning(WARNING_UNREADABLE);
            return Ok(builder.finish(MEMORY_COMPONENT, None));
        }
    };
    let mut scope_count = 0usize;
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(_) => {
            builder.warning(WARNING_UNREADABLE);
            return Ok(builder.finish(MEMORY_COMPONENT, None));
        }
    };
    for (index, entry) in entries.enumerate() {
        if index >= MAX_SCAN_ENTRIES {
            builder.warning(WARNING_SCAN_BOUND);
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                builder.warning(WARNING_UNREADABLE);
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            continue;
        };
        if !memory::paths::valid_scope(name_text) {
            continue;
        }
        scope_count += 1;
        let scope_source = entry.path();
        let scope_destination = destination.join(name_text);
        match regular_directory_metadata(&scope_source) {
            Ok(_) => create_private_dir(&scope_destination)?,
            Err(_) => {
                builder.warning(WARNING_UNREADABLE);
                continue;
            }
        }
        for tree_name in ["state", "memories"] {
            let tree_source = scope_source.join(tree_name);
            let tree_destination = scope_destination.join(tree_name);
            create_private_dir(&tree_destination)?;
            match regular_directory_metadata(&tree_source) {
                Ok(_) => copy_tree(
                    &tree_source,
                    &tree_destination,
                    &mut builder,
                    token_path,
                    None,
                    0,
                )?,
                Err(MetadataFailure::Missing) => builder.warning(WARNING_LAYOUT),
                Err(_) => builder.warning(WARNING_UNREADABLE),
            }
        }
    }
    if scope_count == 0 {
        builder.warning(WARNING_LAYOUT);
    }
    Ok(builder.finish(MEMORY_COMPONENT, None))
}

fn build_conversations_component(
    data_dir: &Path,
    destination: &Path,
    token_path: Option<&Path>,
) -> Result<ManifestComponent, CommandError> {
    create_private_dir(destination)?;
    let mut builder = ComponentBuilder::default();
    match regular_directory_metadata(data_dir) {
        Ok(_) => {}
        Err(MetadataFailure::Missing) => {
            builder.warning(WARNING_MISSING);
            return Ok(builder.finish(CONVERSATIONS_COMPONENT, None));
        }
        Err(_) => {
            builder.warning(WARNING_UNREADABLE);
            return Ok(builder.finish(CONVERSATIONS_COMPONENT, None));
        }
    }
    let source = data_dir.join(CONVERSATIONS_DIR_NAME);
    match regular_directory_metadata(&source) {
        Ok(_) => copy_tree(
            &source,
            destination,
            &mut builder,
            token_path,
            Some(data_dir),
            0,
        )?,
        Err(MetadataFailure::Missing) => builder.warning(WARNING_MISSING),
        Err(_) => builder.warning(WARNING_UNREADABLE),
    }
    Ok(builder.finish(CONVERSATIONS_COMPONENT, None))
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    builder: &mut ComponentBuilder,
    token_path: Option<&Path>,
    telegram_data_dir: Option<&Path>,
    depth: usize,
) -> Result<(), CommandError> {
    if depth > MAX_SCAN_DEPTH {
        builder.warning(WARNING_SCAN_BOUND);
        return Ok(());
    }
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(_) => {
            builder.warning(WARNING_UNREADABLE);
            return Ok(());
        }
    };
    for (index, entry) in entries.enumerate() {
        if index >= MAX_SCAN_ENTRIES {
            builder.warning(WARNING_SCAN_BOUND);
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                builder.warning(WARNING_UNREADABLE);
                continue;
            }
        };
        let name = entry.file_name();
        let source_path = entry.path();
        if is_excluded_source(&source_path, &name, token_path, telegram_data_dir) {
            continue;
        }
        let destination_path = destination.join(Path::new(&name));
        let metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(_) => {
                builder.warning(WARNING_UNREADABLE);
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            builder.warning(WARNING_UNREADABLE);
        } else if metadata.is_dir() {
            create_private_dir(&destination_path)?;
            copy_tree(
                &source_path,
                &destination_path,
                builder,
                token_path,
                telegram_data_dir,
                depth + 1,
            )?;
        } else if metadata.is_file() {
            match copy_regular_file(&source_path, &destination_path) {
                Ok(bytes) => builder.add_file(bytes)?,
                Err(CopyFailure::Source) => builder.warning(WARNING_UNREADABLE),
                Err(CopyFailure::Destination) => {
                    return Err(CommandError::failed(CATEGORY_WRITE_FAILED));
                }
            }
        } else {
            builder.warning(WARNING_UNREADABLE);
        }
    }
    Ok(())
}

fn is_excluded_source(
    path: &Path,
    name: &OsStr,
    token_path: Option<&Path>,
    telegram_data_dir: Option<&Path>,
) -> bool {
    if name == OsStr::new(TOKEN_LOCK_FILE_NAME) || is_credential_name(Some(name)) {
        return true;
    }
    if is_credential_path(path, token_path) {
        return true;
    }
    telegram_data_dir.is_some_and(|data_dir| path == data_dir.join(HEALTH_FILE_NAME))
}

fn is_credential_path(path: &Path, token_path: Option<&Path>) -> bool {
    token_path.is_some_and(|token_path| normalized_path(path) == normalized_path(token_path))
}

fn is_credential_name(name: Option<&OsStr>) -> bool {
    let Some(name) = name.and_then(OsStr::to_str) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    if lower == TOKEN_LOCK_FILE_NAME
        || lower == "token"
        || lower == "token.txt"
        || lower == "telegram.token"
        || lower == "telegram-token"
        || lower == "telegram_token"
    {
        return true;
    }
    let tokenish = lower.contains("token");
    let secretish = lower.ends_with(".secret")
        || (lower.contains("secret") && (lower.contains("telegram") || lower.starts_with("bot-")));
    tokenish || secretish
}

fn normalized_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetadataFailure {
    Missing,
    Unreadable,
    NotRegular,
}

fn regular_metadata(path: &Path) -> Result<Metadata, MetadataFailure> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(MetadataFailure::Missing);
        }
        Err(_) => return Err(MetadataFailure::Unreadable),
    };
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        Ok(metadata)
    } else {
        Err(MetadataFailure::NotRegular)
    }
}

fn regular_directory_metadata(path: &Path) -> Result<Metadata, MetadataFailure> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(MetadataFailure::Missing);
        }
        Err(_) => return Err(MetadataFailure::Unreadable),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(metadata)
    } else {
        Err(MetadataFailure::NotRegular)
    }
}

fn create_private_dir(path: &Path) -> Result<(), CommandError> {
    fs::create_dir(path).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    set_mode(path, 0o700).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))
}

fn copy_regular_file(source: &Path, destination: &Path) -> Result<u64, CopyFailure> {
    let mut input_options = OpenOptions::new();
    input_options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        input_options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut input = input_options
        .open(source)
        .map_err(|_| CopyFailure::Source)?;

    let mut output_options = OpenOptions::new();
    output_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        output_options.mode(0o600);
        output_options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut output = output_options
        .open(destination)
        .map_err(|_| CopyFailure::Destination)?;

    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = match input.read(&mut buffer) {
            Ok(read) => read,
            Err(_) => {
                let _ = fs::remove_file(destination);
                return Err(CopyFailure::Source);
            }
        };
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|_| CopyFailure::Destination)?;
        total = match total.checked_add(read as u64) {
            Some(total) => total,
            None => {
                let _ = fs::remove_file(destination);
                return Err(CopyFailure::Destination);
            }
        };
    }
    output.sync_all().map_err(|_| CopyFailure::Destination)?;
    set_mode(destination, 0o600).map_err(|_| CopyFailure::Destination)?;
    Ok(total)
}

fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<(), CommandError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    file.write_all(bytes)
        .map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    file.sync_all()
        .map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    set_mode(path, 0o600).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))
}

fn octal_mode(metadata: &Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        format!("{:04o}", metadata.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        "unknown".to_string()
    }
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub fn restore_at(
    target: &Path,
    backup_dir: &Path,
    force: bool,
) -> Result<RestoreReport, CommandError> {
    let preflight = preflight_backup(backup_dir)?;
    inspect_target(target, force, &preflight)?;
    write_restore(target, &preflight, force)?;

    let components = preflight
        .components
        .iter()
        .map(|component| RestoredComponent {
            name: component.name.clone(),
            file_count: component.file_count,
            byte_count: component.byte_count,
        })
        .collect();
    Ok(RestoreReport {
        restored: true,
        target: target.to_string_lossy().into_owned(),
        components,
    })
}

fn preflight_backup(backup_dir: &Path) -> Result<RestorePreflight, CommandError> {
    let _metadata = regular_directory_metadata(backup_dir)
        .map_err(|_| CommandError::failed(CATEGORY_INVALID_BACKUP))?;
    let manifest_path = backup_dir.join(MANIFEST_FILE_NAME);
    let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)
        .map_err(|_| CommandError::failed(CATEGORY_INVALID_BACKUP))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| CommandError::failed(CATEGORY_INVALID_BACKUP))?;
    validate_manifest(&manifest)?;

    let configured_token_name = configured_token_name(&backup_dir.join(config::FILE_NAME));
    reject_credential_material(backup_dir, configured_token_name.as_deref(), 0)?;
    validate_backup_root_entries(backup_dir, &manifest, configured_token_name.as_deref())?;

    let mut components = Vec::with_capacity(manifest.components.len());
    for manifest_component in &manifest.components {
        let mut component = RestoreComponent {
            name: manifest_component.name.clone(),
            ..RestoreComponent::default()
        };
        match manifest_component.name.as_str() {
            CONFIG_COMPONENT => {
                let source = backup_dir.join(config::FILE_NAME);
                let size = scan_restore_file(
                    &source,
                    &mut component,
                    PathBuf::from(config::FILE_NAME),
                    configured_token_name.as_deref(),
                )?;
                if size != manifest_component.byte_count || component.file_count != 1 {
                    return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
                }
            }
            MEMORY_COMPONENT => {
                let source = backup_dir.join(MEMORY_DIR_NAME);
                scan_restore_directory(
                    &source,
                    &mut component,
                    PathBuf::from(MEMORY_DIR_NAME),
                    configured_token_name.as_deref(),
                    0,
                )?;
            }
            CONVERSATIONS_COMPONENT => {
                let source = backup_dir.join(CONVERSATIONS_DIR_NAME);
                scan_restore_directory(
                    &source,
                    &mut component,
                    PathBuf::from(CONVERSATIONS_DIR_NAME),
                    configured_token_name.as_deref(),
                    0,
                )?;
            }
            _ => return Err(CommandError::failed(CATEGORY_INVALID_BACKUP)),
        }
        if component.file_count != manifest_component.file_count
            || component.byte_count != manifest_component.byte_count
        {
            return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
        }
        component
            .files
            .sort_by(|left, right| left.relative.cmp(&right.relative));
        component.dirs.sort();
        components.push(component);
    }
    Ok(RestorePreflight { components })
}

fn reject_credential_material(
    directory: &Path,
    configured_token_name: Option<&OsStr>,
    depth: usize,
) -> Result<(), CommandError> {
    if depth > MAX_SCAN_DEPTH {
        return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
    }
    let entries =
        fs::read_dir(directory).map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_SCAN_ENTRIES {
            return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
        }
        let entry = entry.map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
        let name = entry.file_name();
        if is_credential_name(Some(&name))
            || configured_token_name.is_some_and(|token| token == name.as_os_str())
        {
            return Err(CommandError::failed(CATEGORY_CREDENTIAL_MATERIAL));
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            reject_credential_material(&path, configured_token_name, depth + 1)?;
        }
    }
    Ok(())
}

fn validate_manifest(manifest: &Manifest) -> Result<(), CommandError> {
    let expected_exclusions = EXCLUSIONS
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    if manifest.version != 1
        || !manifest.created_at.ends_with('Z')
        || chrono::DateTime::parse_from_rfc3339(&manifest.created_at).is_err()
        || !Path::new(&manifest.source_home).is_absolute()
        || manifest.exclusions != expected_exclusions
    {
        return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
    }
    let mut names = BTreeSet::new();
    for component in &manifest.components {
        if ![CONFIG_COMPONENT, MEMORY_COMPONENT, CONVERSATIONS_COMPONENT]
            .contains(&component.name.as_str())
            || !names.insert(component.name.clone())
            || component
                .warnings
                .iter()
                .any(|warning| !VALID_WARNINGS.contains(&warning.as_str()))
        {
            return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
        }
        if let Some(mode) = &component.mode
            && (mode.len() != 4 || !mode.bytes().all(|byte| (b'0'..=b'7').contains(&byte)))
        {
            return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
        }
    }
    if !names.contains(CONFIG_COMPONENT) || !names.contains(MEMORY_COMPONENT) {
        return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
    }
    Ok(())
}

fn validate_backup_root_entries(
    backup_dir: &Path,
    manifest: &Manifest,
    configured_token_name: Option<&OsStr>,
) -> Result<(), CommandError> {
    let allowed = manifest
        .components
        .iter()
        .map(|component| component.name.clone())
        .collect::<BTreeSet<_>>();
    let entries =
        fs::read_dir(backup_dir).map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_SCAN_ENTRIES {
            return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
        }
        let entry = entry.map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
        let name = entry.file_name();
        if is_credential_name(Some(&name))
            || configured_token_name.is_some_and(|token| token == name.as_os_str())
        {
            return Err(CommandError::failed(CATEGORY_CREDENTIAL_MATERIAL));
        }
        let allowed_name = name.as_os_str() == OsStr::new(MANIFEST_FILE_NAME)
            || name.as_os_str() == OsStr::new(config::FILE_NAME)
            || (name.as_os_str() == OsStr::new(MEMORY_DIR_NAME)
                && allowed.contains(MEMORY_COMPONENT))
            || (name.as_os_str() == OsStr::new(CONVERSATIONS_DIR_NAME)
                && allowed.contains(CONVERSATIONS_COMPONENT));
        if !allowed_name {
            return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
        }
    }
    Ok(())
}

fn configured_token_name(path: &Path) -> Option<OsString> {
    config::Config::load(path)
        .ok()
        .and_then(|config| config.telegram.token_file)
        .and_then(|path| path.file_name().map(OsStr::to_os_string))
}

fn scan_restore_file(
    source: &Path,
    component: &mut RestoreComponent,
    relative: PathBuf,
    configured_token_name: Option<&OsStr>,
) -> Result<u64, CommandError> {
    if !safe_relative(&relative) {
        return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
    }
    if is_credential_name(source.file_name())
        || configured_token_name.is_some_and(|token| source.file_name() == Some(token))
    {
        return Err(CommandError::failed(CATEGORY_CREDENTIAL_MATERIAL));
    }
    let metadata =
        regular_metadata(source).map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
    let bytes = consume_regular_file(source)
        .map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
    if bytes != metadata.len() {
        return Err(CommandError::failed(CATEGORY_BACKUP_UNREADABLE));
    }
    component.file_count = component
        .file_count
        .checked_add(1)
        .ok_or(CommandError::failed(CATEGORY_INVALID_BACKUP))?;
    component.byte_count = component
        .byte_count
        .checked_add(bytes)
        .ok_or(CommandError::failed(CATEGORY_INVALID_BACKUP))?;
    component.files.push(RestoreFile {
        source: source.to_path_buf(),
        relative,
    });
    Ok(bytes)
}

fn scan_restore_directory(
    source: &Path,
    component: &mut RestoreComponent,
    relative: PathBuf,
    configured_token_name: Option<&OsStr>,
    depth: usize,
) -> Result<(), CommandError> {
    if !safe_relative(&relative) {
        return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
    }
    regular_directory_metadata(source)
        .map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
    component.dirs.push(relative.clone());
    if depth > MAX_SCAN_DEPTH {
        return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
    }
    let entries =
        fs::read_dir(source).map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_SCAN_ENTRIES {
            return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
        }
        let entry = entry.map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
        let name = entry.file_name();
        if is_credential_name(Some(&name))
            || configured_token_name.is_some_and(|token| token == name.as_os_str())
        {
            return Err(CommandError::failed(CATEGORY_CREDENTIAL_MATERIAL));
        }
        let child_source = entry.path();
        let child_relative = relative.join(Path::new(&name));
        let metadata = fs::symlink_metadata(&child_source)
            .map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;
        if metadata.file_type().is_symlink() {
            return Err(CommandError::failed(CATEGORY_BACKUP_UNREADABLE));
        }
        if metadata.is_dir() {
            scan_restore_directory(
                &child_source,
                component,
                child_relative,
                configured_token_name,
                depth + 1,
            )?;
        } else if metadata.is_file() {
            scan_restore_file(
                &child_source,
                component,
                child_relative,
                configured_token_name,
            )?;
        } else {
            return Err(CommandError::failed(CATEGORY_BACKUP_UNREADABLE));
        }
    }
    Ok(())
}

fn safe_relative(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn consume_regular_file(path: &Path) -> io::Result<u64> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(total);
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("size overflow"))?;
    }
}

fn inspect_target(
    target: &Path,
    force: bool,
    preflight: &RestorePreflight,
) -> Result<(), CommandError> {
    validate_target_ancestors(target)?;
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(CommandError::failed(CATEGORY_TARGET_UNSAFE));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(CommandError::failed(CATEGORY_TARGET_UNSAFE)),
    }

    let config_path = target.join(config::FILE_NAME);
    let config_exists = fs::symlink_metadata(&config_path).is_ok();
    let nonempty = fs::read_dir(target)
        .map_err(|_| CommandError::failed(CATEGORY_TARGET_UNSAFE))?
        .next()
        .transpose()
        .map_err(|_| CommandError::failed(CATEGORY_TARGET_UNSAFE))?
        .is_some();
    if force && config_exists {
        return Err(CommandError::failed(CATEGORY_TARGET_CONFIG));
    }
    if nonempty && !force {
        return Err(CommandError::failed(CATEGORY_TARGET_NOT_EMPTY));
    }

    for component in &preflight.components {
        for relative in &component.dirs {
            validate_destination_directory(target, relative)?;
        }
        for file in &component.files {
            if let Some(relative) = file.relative.parent() {
                validate_destination_directory(target, relative)?;
            }
        }
        for file in &component.files {
            validate_destination_file(target, &file.relative)?;
        }
    }
    Ok(())
}

fn validate_target_ancestors(target: &Path) -> Result<(), CommandError> {
    let mut current = target.parent();
    while let Some(path) = current {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(CommandError::failed(CATEGORY_TARGET_UNSAFE));
            }
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => current = path.parent(),
            Err(_) => return Err(CommandError::failed(CATEGORY_TARGET_UNSAFE)),
        }
    }
    Ok(())
}

fn validate_destination_directory(target: &Path, relative: &Path) -> Result<(), CommandError> {
    if !safe_relative(relative) {
        return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
    }
    let path = target.join(relative);
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(CommandError::failed(CATEGORY_TARGET_UNSAFE)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(CommandError::failed(CATEGORY_TARGET_UNSAFE)),
    }
}

fn validate_destination_file(target: &Path, relative: &Path) -> Result<(), CommandError> {
    if !safe_relative(relative) {
        return Err(CommandError::failed(CATEGORY_INVALID_BACKUP));
    }
    let path = target.join(relative);
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(CommandError::failed(CATEGORY_TARGET_UNSAFE)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(CommandError::failed(CATEGORY_TARGET_UNSAFE)),
    }
}

fn write_restore(
    target: &Path,
    preflight: &RestorePreflight,
    force: bool,
) -> Result<(), CommandError> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(CommandError::failed(CATEGORY_TARGET_UNSAFE)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(target).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
        }
        Err(_) => return Err(CommandError::failed(CATEGORY_WRITE_FAILED)),
    }
    set_mode(target, 0o700).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;

    let mut directories = preflight
        .components
        .iter()
        .flat_map(|component| component.dirs.iter())
        .cloned()
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| (path.components().count(), path.clone()));
    directories.dedup();
    for relative in directories {
        let path = target.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(CommandError::failed(CATEGORY_WRITE_FAILED)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&path).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
            }
            Err(_) => return Err(CommandError::failed(CATEGORY_WRITE_FAILED)),
        }
        set_mode(&path, 0o700).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    }

    for component in &preflight.components {
        for file in &component.files {
            write_restore_file(target, file, force)?;
        }
    }
    Ok(())
}

fn write_restore_file(target: &Path, file: &RestoreFile, force: bool) -> Result<(), CommandError> {
    let destination = target.join(&file.relative);
    let parent = destination
        .parent()
        .ok_or(CommandError::failed(CATEGORY_WRITE_FAILED))?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => return Err(CommandError::failed(CATEGORY_WRITE_FAILED)),
    }
    let exists = match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => true,
        Ok(_) => return Err(CommandError::failed(CATEGORY_WRITE_FAILED)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => return Err(CommandError::failed(CATEGORY_WRITE_FAILED)),
    };
    if exists && !force {
        return Err(CommandError::failed(CATEGORY_TARGET_NOT_EMPTY));
    }

    let mut input_options = OpenOptions::new();
    input_options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        input_options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut input = input_options
        .open(&file.source)
        .map_err(|_| CommandError::failed(CATEGORY_BACKUP_UNREADABLE))?;

    let mut output_options = OpenOptions::new();
    output_options.write(true);
    if exists {
        output_options.truncate(true);
    } else {
        output_options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        output_options.mode(0o600);
        output_options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut output = output_options
        .open(&destination)
        .map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    io::copy(&mut input, &mut output).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    output
        .sync_all()
        .map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))?;
    set_mode(&destination, 0o600).map_err(|_| CommandError::failed(CATEGORY_WRITE_FAILED))
}

fn read_bounded(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let metadata = regular_metadata_io(path)?;
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input exceeds bound",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input exceeds bound",
        ));
    }
    Ok(bytes)
}

fn regular_metadata_io(path: &Path) -> io::Result<Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        Ok(metadata)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    use tempfile::tempdir;

    fn private_dir(path: &Path) {
        fs::create_dir_all(path).expect("directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("directory mode");
    }

    fn private_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            private_dir(parent);
        }
        fs::write(path, bytes).expect("file");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("file mode");
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempdir().expect("temp root");
        let home = root.path().join("home");
        let memory = root.path().join("memory");
        let telegram = root.path().join("telegram");
        let backups = root.path().join("backups");
        private_dir(&home);
        private_dir(&memory);
        private_dir(&telegram.join(CONVERSATIONS_DIR_NAME));
        private_file(&memory.join("global/state/facts.jsonl"), b"fact body\n");
        private_file(&memory.join("global/memories/MEMORY.md"), b"memory body\n");
        private_file(
            &telegram.join(CONVERSATIONS_DIR_NAME).join("1.json"),
            b"message body\n",
        );
        let token = root.path().join("telegram.token");
        private_file(&token, b"TEST_TOKEN_SECRET_MARKER");
        let config = format!(
            "[memory]\ndir = \"{}\"\n[telegram]\ntoken_file = \"{}\"\n",
            memory.display(),
            token.display()
        );
        private_file(&home.join(config::FILE_NAME), config.as_bytes());
        (root, home, telegram, backups)
    }

    fn all_bytes(path: &Path) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in fs::read_dir(path).expect("read tree") {
            let entry = entry.expect("entry");
            let metadata = fs::symlink_metadata(entry.path()).expect("metadata");
            if metadata.is_dir() {
                out.extend(all_bytes(&entry.path()));
            } else if metadata.is_file() {
                out.extend(fs::read(entry.path()).expect("bytes"));
            }
        }
        out
    }

    #[test]
    fn backup_is_atomic_and_excludes_token_bytes() {
        let (_root, home, telegram, backups) = fixture();
        let backup = create_at(&home, &backups, Some(&telegram), None).expect("backup");
        assert!(backup.is_dir());
        assert!(!fs::read_dir(&backups).expect("backups").any(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(TEMP_PREFIX)
        }));
        assert!(!String::from_utf8_lossy(&all_bytes(&backup)).contains("TEST_TOKEN_SECRET_MARKER"));
        assert_eq!(
            fs::read(home.join(config::FILE_NAME)).expect("source config"),
            fs::read(backup.join(config::FILE_NAME)).expect("backup config")
        );
        assert_eq!(
            fs::read(
                home.parent()
                    .expect("root")
                    .join("memory/global/state/facts.jsonl")
            )
            .expect("source facts"),
            fs::read(backup.join("memory/global/state/facts.jsonl")).expect("backup facts")
        );
        assert_eq!(
            fs::read(telegram.join(CONVERSATIONS_DIR_NAME).join("1.json"))
                .expect("source conversation"),
            fs::read(backup.join(CONVERSATIONS_DIR_NAME).join("1.json"))
                .expect("backup conversation")
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(backup.join(MANIFEST_FILE_NAME)).expect("manifest"))
                .expect("manifest json");
        assert_eq!(manifest["version"], 1);
        assert_eq!(manifest["components"][0]["mode"], "0600");
    }

    /// A config that fails to parse or validate must refuse the backup
    /// rather than run without it (#153): without the parsed config the
    /// `token_file` exclusion is gone and only the filename heuristic
    /// remains, which a token stored under a neutral name defeats.
    #[test]
    fn invalid_config_refuses_backup_instead_of_dropping_exclusions() {
        let (root, home, telegram, backups) = fixture();
        // A token path the name heuristic does not recognise, declared only
        // through config.toml, under a directory the backup copies.
        let neutral_token = telegram.join(CONVERSATIONS_DIR_NAME).join("bot.key");
        private_file(&neutral_token, b"TEST_TOKEN_SECRET_MARKER");
        let memory = root.path().join("memory");
        let mut config = format!(
            "[memory]\ndir = \"{}\"\n[telegram]\ntoken_file = \"{}\"\n",
            memory.display(),
            neutral_token.display()
        );
        // Sanity: with a valid config the neutral-named token is excluded.
        fs::write(home.join(config::FILE_NAME), config.as_bytes()).expect("config");
        let backup = create_at(&home, &backups, Some(&telegram), None).expect("backup");
        assert!(!String::from_utf8_lossy(&all_bytes(&backup)).contains("TEST_TOKEN_SECRET_MARKER"));

        // One unknown key elsewhere in the file invalidates it.
        config.push_str("[cron]\nenbled = true\n");
        fs::write(home.join(config::FILE_NAME), config.as_bytes()).expect("config");
        let error = create_at(&home, &backups, Some(&telegram), None).expect_err("must refuse");
        assert_eq!(error.category(), CATEGORY_CONFIG_INVALID);
        assert_eq!(error.kind(), ErrorKind::Failed);
        // Nothing new was written: the earlier backup is the only entry and
        // no temp directory was left behind.
        let entries: Vec<_> = fs::read_dir(&backups)
            .expect("backups")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0], backup.file_name().expect("name"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_memory_component_is_a_warning() {
        let (_root, home, telegram, backups) = fixture();
        let memory = home.parent().expect("root").join("memory");
        fs::remove_dir_all(memory.join("global/state")).expect("remove state");
        std::os::unix::fs::symlink(memory.join("global/memories"), memory.join("global/state"))
            .expect("symlink");
        let backup = create_at(&home, &backups, Some(&telegram), None).expect("backup");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(backup.join(MANIFEST_FILE_NAME)).expect("manifest"))
                .expect("manifest json");
        let memory_component = manifest["components"]
            .as_array()
            .expect("components")
            .iter()
            .find(|component| component["name"] == MEMORY_COMPONENT)
            .expect("memory component");
        assert!(
            memory_component["warnings"]
                .as_array()
                .expect("warnings")
                .iter()
                .any(|warning| warning == WARNING_UNREADABLE)
        );
    }

    #[test]
    fn restore_reproduces_owner_only_files_and_one_report() {
        let (_root, home, telegram, backups) = fixture();
        let backup = create_at(&home, &backups, Some(&telegram), None).expect("backup");
        let target = backups.join("restore");
        let report = restore_at(&target, &backup, false).expect("restore");
        assert!(report.restored);
        assert_eq!(report.target, target.to_string_lossy().into_owned());
        assert_eq!(
            fs::read(home.join(config::FILE_NAME)).expect("source config"),
            fs::read(target.join(config::FILE_NAME)).expect("restored config")
        );
        assert_eq!(
            fs::read(
                home.parent()
                    .expect("root")
                    .join("memory/global/state/facts.jsonl")
            )
            .expect("source facts"),
            fs::read(target.join("memory/global/state/facts.jsonl")).expect("restored facts")
        );
        let mode = fs::metadata(target.join(config::FILE_NAME))
            .expect("config")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(target.join(MEMORY_DIR_NAME))
            .expect("memory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn restore_rejects_nonempty_and_config_targets() {
        let (_root, home, telegram, backups) = fixture();
        let backup = create_at(&home, &backups, Some(&telegram), None).expect("backup");
        let target = backups.join("restore");
        private_file(&target.join("existing"), b"existing");
        let error = restore_at(&target, &backup, false).unwrap_err();
        assert_eq!(error.category(), CATEGORY_TARGET_NOT_EMPTY);
        private_file(&target.join(config::FILE_NAME), b"config");
        let error = restore_at(&target, &backup, true).unwrap_err();
        assert_eq!(error.category(), CATEGORY_TARGET_CONFIG);
    }

    #[test]
    fn restore_preflight_rejects_corrupt_manifest_without_creating_target() {
        let (_root, home, telegram, backups) = fixture();
        let backup = create_at(&home, &backups, Some(&telegram), None).expect("backup");
        fs::write(backup.join(MANIFEST_FILE_NAME), b"not json").expect("corrupt manifest");
        let target = backups.join("corrupt-target");
        let error = restore_at(&target, &backup, false).expect_err("corrupt backup refusal");
        assert_eq!(error.category(), CATEGORY_INVALID_BACKUP);
        assert!(!target.exists());
    }

    #[test]
    fn restore_preflight_rejects_token_shaped_file_without_creating_target() {
        let (_root, home, telegram, backups) = fixture();
        let backup = create_at(&home, &backups, Some(&telegram), None).expect("backup");
        private_file(
            &backup
                .join(MEMORY_DIR_NAME)
                .join("global/state/telegram.token"),
            b"TEST_TOKEN_SECRET_MARKER",
        );
        let target = backups.join("credential-target");
        let error = restore_at(&target, &backup, false).expect_err("credential refusal");
        assert_eq!(error.category(), CATEGORY_CREDENTIAL_MATERIAL);
        assert!(!target.exists());
    }

    #[test]
    fn restore_requires_target() {
        let error = RestoreArgs::try_parse_from(["danso restore", "backup"]).unwrap_err();
        assert_eq!(error.exit_code(), 2);
    }

    /// A `DANSO_HOME` node whose pre-#136 state still sits under the old
    /// root (#177): the snapshot holds only the root in use, says so with
    /// the fixed `legacy_state` category on that component, and copies
    /// nothing from the old tree.  A root chosen explicitly is not compared.
    #[test]
    fn legacy_state_left_behind_is_named_not_archived() {
        let (root, home, _chosen_telegram, backups) = fixture();
        let legacy_home = root.path().join("old-danso");
        private_file(
            &legacy_home.join("telegram/conversations/a.json"),
            b"OLD_CONVERSATION_MARKER",
        );
        private_dir(&legacy_home.join("memory/global"));
        let default_telegram = home.join(telegram::DEFAULT_DATA_DIR_NAME);
        private_file(
            &default_telegram.join("conversations/b.json"),
            b"new conversation\n",
        );

        let backup = create_at(&home, &backups, Some(&default_telegram), Some(&legacy_home))
            .expect("backup");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(backup.join(MANIFEST_FILE_NAME)).expect("manifest"))
                .expect("json");
        let warnings = |name: &str| {
            manifest["components"]
                .as_array()
                .expect("components")
                .iter()
                .find(|c| c["name"] == name)
                .expect("component")["warnings"]
                .clone()
        };
        assert_eq!(
            warnings("conversations"),
            serde_json::json!(["legacy_state"])
        );
        // The fixture's `memory.dir` is a deliberate choice: not compared.
        assert_eq!(warnings("memory"), serde_json::json!([]));
        assert_eq!(
            manifest["components"][2]["file_count"], 1,
            "only the root in use is archived"
        );
        let bytes = all_bytes(&backup);
        assert!(
            !bytes.windows(23).any(|w| w == b"OLD_CONVERSATION_MARKER"),
            "legacy bodies never enter the archive"
        );
        // The same snapshot restores: the category is a known one.
        let target = root.path().join("restore-target");
        let args = RestoreArgs {
            target: target.clone(),
            force: false,
            backup_dir: backup.clone(),
        };
        assert!(run_restore(&args).expect("restore").restored);

        // Without a split there is nothing to name.
        let backup = create_at(&home, &backups, Some(&default_telegram), None).expect("backup");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(backup.join(MANIFEST_FILE_NAME)).expect("manifest"))
                .expect("json");
        assert_eq!(manifest["components"][2]["warnings"], serde_json::json!([]));
    }
}
