use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{fs, path::PathBuf};
pub(super) fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))
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
    let content = if edit {
        let old = string(args, "oldText")?;
        ensure!(!old.is_empty(), "oldText must not be empty");
        let current = crate::context::read_bounded(&path, crate::context::FILE_LIMIT)?;
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
    fs::create_dir_all(path.parent().context("missing parent")?)?;
    fs::write(path, content)?;
    println!("ok");
    Ok(())
}
