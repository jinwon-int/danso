//! Owner-only filesystem primitives and scope routing for the memory state
//! tree (issue #52 §3/§6.1). Rust port of the audited ccc-node `secure_fs`
//! invariants plus the canonical scope validator from the #52 design:
//! symlink/hardlink rejection, forced 0700 directories and 0600 regular
//! files owned by the current user, bounded reads, fsync + atomic replace
//! writes, and exclusive locking with a deadline.

use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;

/// Facts file name, shared with the ccc-node contract for fixture compatibility.
pub const FACTS_FILE: &str = "memory-facts.jsonl";
/// Resume pointer name (§4.1); written by the harness in M3, read everywhere.
pub const RESUME_FILE: &str = "resume.md";
/// Working-state name (§5.3); written by the harness in M3, read everywhere.
pub const WORKING_STATE_FILE: &str = "working-state.md";
/// Per-scope single-writer lock (§6.1).
pub const SCOPE_LOCK_FILE: &str = ".memory.lock";

/// Default exclusive-lock deadline; failures are reported, never waited out
/// silently.
pub const LOCK_TIMEOUT_DEFAULT_MS: u64 = 3000;
pub const LOCK_TIMEOUT_MAX_MS: u64 = 10_000;

/// The canonical scope validator (§7): exactly one function; `global` is the
/// CLI default, `shared` is the shared tree, `private-` scopes carry a
/// 32-lowercase-hex derived label. Raw caller ids (Telegram ids, session
/// ids) never appear in scope names.
pub fn valid_scope(name: &str) -> bool {
    name == "global"
        || name == "shared"
        || (name.len() == 8 + 32
            && name.starts_with("private-")
            && name[8..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
}

/// Validate and return the canonical scope name (configuration error otherwise).
pub fn require_scope(name: &str) -> Result<&str> {
    ensure!(
        valid_scope(name),
        "memory scope must be global, shared, or private-<32 hex>"
    );
    Ok(name)
}

/// An absolute memory root plus one validated scope below it. Every file
/// operation goes through this route so scope names are validated exactly
/// once per process.
#[derive(Clone, Debug)]
pub struct Route {
    root: PathBuf,
    scope: String,
}

impl Route {
    pub fn new(root: &Path, scope: &str) -> Result<Self> {
        ensure!(root.is_absolute(), "memory root must be an absolute path");
        Ok(Self {
            root: root.to_path_buf(),
            scope: require_scope(scope)?.to_string(),
        })
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<scope>` — the audience boundary directory.
    pub fn scope_dir(&self) -> PathBuf {
        self.root.join(&self.scope)
    }

    pub fn memories_dir(&self) -> PathBuf {
        self.scope_dir().join("memories")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.scope_dir().join("state")
    }

    pub fn facts_file(&self) -> PathBuf {
        self.state_dir().join(FACTS_FILE)
    }

    pub fn resume_file(&self) -> PathBuf {
        self.state_dir().join(RESUME_FILE)
    }

    pub fn working_state_file(&self) -> PathBuf {
        self.state_dir().join(WORKING_STATE_FILE)
    }

    pub fn scope_lock(&self) -> PathBuf {
        self.state_dir().join(SCOPE_LOCK_FILE)
    }

    /// The shared-scope route below the same root (§7 read rule: private
    /// routes additionally read the shared tree; `shared` and `global` never
    /// open any other tree).
    pub fn shared_route(&self) -> Option<Route> {
        if self.scope.starts_with("private-") {
            Some(Route {
                root: self.root.clone(),
                scope: "shared".to_string(),
            })
        } else {
            None
        }
    }
}

fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

/// Create a directory (recursively) for the memory tree without following
/// symlinks (ccc `_validate_existing_directory_components` contract):
/// ancestor components must be root- or self-owned and must not be
/// group/other-writable unless they carry the sticky bit (so a 1777 /tmp is
/// a trusted ancestor and is never chmod'd or created); the final component
/// must be process-owned and is tightened to 0700.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::fs::{create_dir, set_permissions};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    ensure!(path.is_absolute(), "memory path must be absolute");
    let components: Vec<_> = path.components().collect();
    ensure!(
        components.len() > 1,
        "memory path must have a component below the root"
    );
    let mut current = PathBuf::new();
    let last = components.len() - 1;
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let is_final = index == last;
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink() && meta.is_dir(),
                    "memory path must be a real directory: {}",
                    current.display()
                );
                if is_final {
                    ensure!(
                        meta.uid() == euid(),
                        "memory directory must be owned by the current user: {}",
                        current.display()
                    );
                    if meta.mode() & 0o777 != 0o700 {
                        set_permissions(&current, std::fs::Permissions::from_mode(0o700))?;
                        let after = std::fs::symlink_metadata(&current)?;
                        ensure!(
                            after.mode() & 0o777 == 0o700,
                            "memory directory mode could not be tightened: {}",
                            current.display()
                        );
                    }
                } else {
                    ensure!(
                        meta.uid() == 0 || meta.uid() == euid(),
                        "memory path has an unsafe owner ancestor: {} (uid={})",
                        current.display(),
                        meta.uid()
                    );
                    if meta.mode() & 0o022 != 0 {
                        ensure!(
                            meta.mode() & 0o1000 != 0,
                            "memory path has an unsafe writable ancestor: {} ({:04o})",
                            current.display(),
                            meta.mode() & 0o777
                        );
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                create_dir(&current)?;
                set_permissions(&current, std::fs::Permissions::from_mode(0o700))?;
                let meta = std::fs::symlink_metadata(&current)?;
                ensure!(
                    meta.is_dir() && meta.uid() == euid() && meta.mode() & 0o777 == 0o700,
                    "memory directory creation failed: {}",
                    current.display()
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Validate one already-open file descriptor: regular file, exactly one link,
/// owned by the current user, mode exactly 0600.
fn validate_open(file: &std::fs::File, what: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "{what} must be a regular file");
    ensure!(meta.nlink() == 1, "{what} must not have hard links");
    ensure!(
        meta.uid() == euid(),
        "{what} must be owned by the current user"
    );
    ensure!(meta.mode() & 0o777 == 0o600, "{what} must have mode 0600");
    Ok(())
}

/// Open a file whose every path component is pinned with O_NOFOLLOW, then
/// validate the final descriptor. This closes the check-then-open symlink
/// window that pathname-only validation leaves open.
pub fn open_secure(path: &Path, what: &str) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;
    ensure!(path.is_absolute(), "{what} path must be absolute");
    let parts: Vec<_> = path.components().collect();
    ensure!(
        parts.len() > 1
            && parts[0] == Component::RootDir
            && parts[1..].iter().all(|p| matches!(p, Component::Normal(_))),
        "{what} path must be canonical"
    );
    let mut dir = std::fs::File::open("/")?;
    let last = parts.len() - 2;
    for (i, part) in parts[1..].iter().enumerate() {
        let name = CString::new(part.as_os_str().as_bytes())?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if i == last { 0 } else { libc::O_DIRECTORY };
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        ensure!(fd >= 0, "{what} is unavailable: {}", path.display());
        dir = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    validate_open(&dir, what)?;
    Ok(dir)
}

/// Validate a path's on-disk state without opening it: no symlink, regular
/// file, one link, current-user owner, mode 0600. Absent paths pass.
pub fn validate_regular(path: &Path, what: &str) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
        Ok(meta) => {
            ensure!(
                !meta.file_type().is_symlink() && meta.is_file(),
                "{what} must be a regular file: {}",
                path.display()
            );
            ensure!(
                meta.nlink() == 1,
                "{what} must not have hard links: {}",
                path.display()
            );
            ensure!(
                meta.uid() == euid(),
                "{what} has the wrong owner: {}",
                path.display()
            );
            ensure!(
                meta.mode() & 0o777 == 0o600,
                "{what} must have mode 0600: {}",
                path.display()
            );
            Ok(true)
        }
    }
}

/// Read an owner-only regular file through a pinned descriptor with a hard
/// byte bound. `Ok(None)` means the file does not exist.
pub fn read_bounded(path: &Path, max_bytes: u64, what: &str) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    if !validate_regular(path, what)? {
        return Ok(None);
    }
    let file = open_secure(path, what)?;
    let mut data = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= max_bytes,
        "{what} exceeds its safe read bound: {}",
        path.display()
    );
    Ok(Some(data))
}

pub fn fsync_dir(path: &Path) -> Result<()> {
    let dir = std::fs::File::open(path)?;
    dir.sync_all()?;
    Ok(())
}

/// Atomically replace a file with `payload`: owner-only temp file in the same
/// directory, write + fsync, rename, fsync the directory. The target must
/// already satisfy the regular-file invariants (or not exist).
pub fn atomic_write(path: &Path, payload: &[u8], what: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    validate_regular(path, what)?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{what} has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{what} has no file name"))?
        .to_string_lossy();
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let temp = dir.join(format!(
        ".{name}.tmp-{}-{unique}-{nanos}",
        std::process::id()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        validate_open(&file, what)?;
        file.write_all(payload)?;
        file.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(e.into());
    }
    fsync_dir(dir)?;
    validate_regular(path, what)?;
    Ok(())
}

/// An exclusive advisory lock file below a private directory. Locks are
/// retried until the deadline and fail closed with a typed error, so a stuck
/// holder can never cause a silent partial write.
pub struct ExclusiveLock {
    file: std::fs::File,
}

impl ExclusiveLock {
    pub fn acquire(path: &Path, timeout_ms: u64) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        let timeout = Duration::from_millis(timeout_ms.clamp(1, LOCK_TIMEOUT_MAX_MS));
        let started = Instant::now();
        validate_regular(path, "memory lock")?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        validate_open(&file, "memory lock")?;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { file }),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() >= timeout {
                        anyhow::bail!(
                            "memory lock is held elsewhere; refusing to wait past {} ms: {}",
                            timeout.as_millis(),
                            path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// Clamp a caller-supplied lock deadline into the supported window.
pub fn lock_timeout(timeout_ms: u64) -> u64 {
    if timeout_ms == 0 {
        LOCK_TIMEOUT_DEFAULT_MS
    } else {
        timeout_ms.min(LOCK_TIMEOUT_MAX_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_names_are_canonical() {
        assert!(valid_scope("global"));
        assert!(valid_scope("shared"));
        assert!(valid_scope(&format!("private-{}", "a".repeat(32))));
        assert!(!valid_scope("private-ABCDEF0123456789ABCDEF0123456789"));
        assert!(!valid_scope("private-short"));
        assert!(!valid_scope("global "));
        assert!(!valid_scope(""));
        assert!(!valid_scope("../escape"));
        assert!(require_scope("global").is_ok());
        assert!(require_scope("nope").is_err());
    }

    #[test]
    fn route_paths_are_below_the_scope_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let route = Route::new(tmp.path(), "global").unwrap();
        assert!(route.facts_file().starts_with(route.scope_dir()));
        assert_eq!(
            route.scope_dir(),
            tmp.path().join("global/state").parent().unwrap()
        );
        assert!(Route::new(tmp.path(), "bogus").is_err());
        assert!(Route::new(Path::new("relative"), "global").is_err());
    }

    #[test]
    fn ensure_private_dir_does_not_tighten_root_or_tmp() {
        use std::os::unix::fs::MetadataExt;
        let root_before = std::fs::metadata("/").unwrap().mode() & 0o777;
        let tmp_before = std::fs::metadata("/tmp").unwrap().mode() & 0o777;
        assert_ne!(
            root_before, 0o700,
            "precondition: / must not already be 0700"
        );
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/b");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata("/").unwrap().mode() & 0o777, root_before);
        assert_eq!(
            std::fs::metadata("/tmp").unwrap().mode() & 0o777,
            tmp_before
        );
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn private_dirs_are_owner_only_and_symlinks_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/b");
        ensure_private_dir(&dir).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(meta.mode() & 0o777, 0o700);

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
        atomic_write(&dir.join("f"), b"data\n", "probe").unwrap();
        assert!(read_bounded(&link.join("f"), 10, "probe").is_err());
    }

    #[test]
    fn atomic_write_enforces_owner_only_and_detects_hardlinks() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        atomic_write(&file, b"one\n", "probe").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"one\n");

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(atomic_write(&file, b"two\n", "probe").is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

        let second = tmp.path().join("g");
        std::fs::hard_link(&file, &second).unwrap();
        assert!(atomic_write(&file, b"three\n", "probe").is_err());
        assert!(read_bounded(&file, 10, "probe").is_err());
    }

    #[test]
    fn read_bound_is_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        atomic_write(&file, &[b'x'; 100], "probe").unwrap();
        assert_eq!(
            read_bounded(&file, 100, "probe").unwrap().unwrap().len(),
            100
        );
        assert!(read_bounded(&file, 99, "probe").is_err());
        assert!(
            read_bounded(&tmp.path().join("missing"), 10, "probe")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn lock_conflicts_fail_closed_within_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join(".lock");
        let held = ExclusiveLock::acquire(&lock, 1000).unwrap();
        let started = Instant::now();
        assert!(ExclusiveLock::acquire(&lock, 100).is_err());
        assert!(started.elapsed() >= Duration::from_millis(100));
        drop(held);
        ExclusiveLock::acquire(&lock, 1000).unwrap();
    }
}
