use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    ffi::CString,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};
pub(super) fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))
}

/// Open a workspace file through a directory descriptor chain pinned at `cwd`.
///
/// Path-based validation followed by a path-based write is a check-then-use
/// race: the final component can be swapped for a symlink in between. Every
/// component here is opened relative to the previously opened descriptor with
/// `O_NOFOLLOW`, so the bytes land in the inode that was validated, and no
/// component can redirect the walk. `context.rs` already resolves the system
/// context file this way; this is the same walk on the write path.
///
/// A symlink at any component is refused rather than resolved. Reads are
/// unaffected.
fn open_in_workspace(cwd: &Path, path: &Path, create: bool) -> Result<fs::File> {
    let relative = path
        .strip_prefix(cwd)
        .map_err(|_| anyhow::anyhow!("writes must stay inside the workspace"))?;
    let parts: Vec<_> = relative.components().collect();
    ensure!(
        !parts.is_empty() && parts.iter().all(|p| matches!(p, Component::Normal(_))),
        "writes must stay inside the workspace"
    );
    let mut dir = fs::File::open(cwd).context("workspace is unavailable")?;
    for part in &parts[..parts.len() - 1] {
        let name = CString::new(part.as_os_str().as_bytes())?;
        // mkdirat is idempotent here: EEXIST is fine, and the following
        // openat with O_NOFOLLOW|O_DIRECTORY rejects a symlink squatting
        // on the name whether we created it or found it.
        unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o755) };
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        ensure!(fd >= 0, "writes must stay inside the workspace");
        dir = unsafe { fs::File::from_raw_fd(fd) };
    }
    let name = CString::new(parts[parts.len() - 1].as_os_str().as_bytes())?;
    let flags =
        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | if create { libc::O_CREAT } else { 0 };
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o644) };
    ensure!(fd >= 0, "writes must stay inside the workspace");
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

pub(super) fn replace_or_write(args: &Value, edit: bool) -> Result<()> {
    let path = PathBuf::from(string(args, "path")?);
    let cwd = std::env::current_dir()?;
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    // A dangling symlink may point into the sandbox's private /tmp.
    // Reject it as well: success there would falsely claim a workspace
    // edit even though the host target remained unchanged.
    if fs::symlink_metadata(&path).is_ok() {
        ensure!(
            path.canonicalize()?.starts_with(&cwd),
            "writes must stay inside the workspace"
        );
    }
    // Resolve the closest existing ancestor before creating anything.
    let existing = path
        .ancestors()
        .find(|p| p.exists())
        .context("no parent")?
        .canonicalize()?;
    ensure!(
        existing.starts_with(&cwd),
        "writes must stay inside the workspace"
    );
    ensure!(
        !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "parent traversal is not allowed"
    );
    // One descriptor for both the read and the write: an edit must not read
    // one inode and then write another.
    let mut file = open_in_workspace(&cwd, &path, !edit)?;
    let content = if edit {
        let old = string(args, "oldText")?;
        ensure!(!old.is_empty(), "oldText must not be empty");
        ensure!(
            file.metadata()?.is_file(),
            "not a regular file: {}",
            path.display()
        );
        let mut current = String::new();
        (&mut file)
            .take(crate::context::FILE_LIMIT + 1)
            .read_to_string(&mut current)?;
        ensure!(
            current.len() as u64 <= crate::context::FILE_LIMIT,
            "file exceeds byte budget: {}",
            path.display()
        );
        ensure!(
            current.matches(old).count() == 1,
            "oldText must match exactly once"
        );
        current.replacen(old, string(args, "newText")?, 1)
    } else {
        string(args, "content")?.to_string()
    };
    ensure!(
        content.len() <= crate::context::FILE_LIMIT as usize,
        "write exceeds byte budget"
    );
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(content.as_bytes())?;
    println!("ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        dir
    }

    // These run in one test because they share the process-wide cwd.
    #[test]
    fn writes_refuse_to_follow_symlinks_at_any_component() {
        let dir = workspace();
        let cwd = std::env::current_dir().unwrap();
        let outside = dir.path().parent().unwrap().join("danso-outside-target");
        fs::write(&outside, "ORIGINAL").unwrap();

        // Plain writes and edits still work.
        replace_or_write(&json!({"path": "a.txt", "content": "hello"}), false).unwrap();
        assert_eq!(fs::read_to_string(cwd.join("a.txt")).unwrap(), "hello");
        replace_or_write(
            &json!({"path": "a.txt", "oldText": "hello", "newText": "world"}),
            true,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(cwd.join("a.txt")).unwrap(), "world");
        // Nested directories are still created.
        replace_or_write(&json!({"path": "x/y/z.txt", "content": "deep"}), false).unwrap();
        assert_eq!(fs::read_to_string(cwd.join("x/y/z.txt")).unwrap(), "deep");

        // A symlink at the final component is refused, and the outside file
        // is untouched. Path-based validation could be raced here; O_NOFOLLOW
        // cannot.
        std::os::unix::fs::symlink(&outside, cwd.join("escape")).unwrap();
        assert!(
            replace_or_write(&json!({"path": "escape", "content": "tampered"}), false).is_err()
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "ORIGINAL");

        // A symlinked intermediate directory is refused too.
        std::os::unix::fs::symlink(dir.path().parent().unwrap(), cwd.join("dirlink")).unwrap();
        assert!(
            replace_or_write(&json!({"path": "dirlink/new.txt", "content": "x"}), false).is_err()
        );

        // Editing through a symlink is refused rather than resolved.
        fs::write(cwd.join("real.txt"), "content").unwrap();
        std::os::unix::fs::symlink(cwd.join("real.txt"), cwd.join("link.txt")).unwrap();
        assert!(
            replace_or_write(
                &json!({"path": "link.txt", "oldText": "content", "newText": "changed"}),
                true
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(cwd.join("real.txt")).unwrap(), "content");

        // An edit against a missing file does not create it.
        assert!(
            replace_or_write(
                &json!({"path": "nope.txt", "oldText": "a", "newText": "b"}),
                true
            )
            .is_err()
        );
        assert!(!cwd.join("nope.txt").exists());

        fs::remove_file(&outside).ok();
    }
}
