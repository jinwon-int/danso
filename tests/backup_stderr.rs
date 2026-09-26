//! CLI-level backup contract for the stranded pre-#136 roots (#185): a
//! successful `danso backup` names each old tree it does not contain on
//! stderr — paths only — while stdout stays exactly the archive path and
//! the exit status stays zero.  A node without a `DANSO_HOME` split prints
//! nothing.

use std::fs;
use std::path::Path;
use std::process::Command;

fn write_private_file(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory");
    }
    fs::write(path, bytes).expect("write file");
}

fn backup_command(root: &Path) -> Command {
    let home = root.join("danso-home");
    fs::create_dir_all(&home).expect("home");
    fs::write(home.join("config.toml"), "").expect("empty config");
    let mut command = Command::new(env!("CARGO_BIN_EXE_danso"));
    command
        .arg("backup")
        .env("DANSO_HOME", &home)
        .env("HOME", root)
        .env_remove("DANSO_TELEGRAM_DATA_DIR")
        .env_remove("DANSO_MEMORY_DIR")
        .env_remove("DANSO_BACKUP_DIR");
    command
}

/// A `DANSO_HOME` node whose pre-#136 Telegram state and memory still sit
/// under `$HOME/.danso` (#176): the backup succeeds, stdout stays the one
/// archive path, and stderr names both old trees with the roots in use —
/// no contents, no counts, one line each.
#[test]
fn backup_names_each_stranded_legacy_root_on_stderr() {
    let root = tempfile::tempdir().expect("temporary backup root");
    let home = root.path().join("danso-home");
    // The roots in use, as a post-#175 node has them.
    write_private_file(&home.join("telegram/conversations/b.json"), b"current\n");
    write_private_file(&home.join("memory/global/note.md"), b"current\n");
    // The stranded pre-#136 trees.
    write_private_file(
        &root.path().join(".danso/telegram/conversations/a.json"),
        b"OLD_CONVERSATION_MARKER",
    );
    write_private_file(&root.path().join(".danso/memory/global/old.md"), b"old\n");

    let output = backup_command(root.path()).output().expect("run backup");

    assert_eq!(output.status.code(), Some(0), "backup succeeds");
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 path line");
    let stdout_lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(stdout_lines.len(), 1, "stdout is exactly the archive path");
    assert!(Path::new(stdout_lines[0]).is_absolute(), "path line");
    assert!(fs::metadata(stdout_lines[0]).is_ok(), "archive exists");

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 warnings");
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(
        lines,
        vec![
            format!(
                "backup warning: {} is not in the snapshot; the service uses {}",
                root.path().join(".danso/telegram").display(),
                home.join("telegram").display()
            ),
            format!(
                "backup warning: {} is not in the snapshot; the service uses {}",
                root.path().join(".danso/memory").display(),
                home.join("memory").display()
            ),
        ],
        "one path-only line per stranded root, telegram first"
    );
    assert!(
        Path::new(stdout_lines[0]).join("manifest.json").is_file(),
        "the path line is the backup directory"
    );
}

/// Without a `DANSO_HOME` split there is nothing to name: stderr stays
/// empty and the command is unchanged.
#[test]
fn backup_without_a_split_prints_no_warning() {
    let root = tempfile::tempdir().expect("temporary backup root");
    let home = root.path().join("danso-home");
    write_private_file(&home.join("telegram/conversations/b.json"), b"current\n");
    write_private_file(&home.join("memory/global/note.md"), b"current\n");

    let output = backup_command(root.path()).output().expect("run backup");

    assert_eq!(output.status.code(), Some(0), "backup succeeds");
    assert!(output.stderr.is_empty(), "no split, no warning");
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 path line");
    assert_eq!(stdout.lines().count(), 1, "stdout is the archive path");
}
