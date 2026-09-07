use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub const CONTEXT_LIMIT: usize = 64 * 1024;
/// Separate from the project budget; covers JSON escaping of native paths.
pub const EXECUTION_CONTEXT_LIMIT: usize = 32 * 1024;
pub const FILE_LIMIT: u64 = 256 * 1024;

pub fn read_bounded(path: &Path, limit: u64) -> Result<String> {
    ensure!(
        fs::metadata(path)?.is_file(),
        "not a regular file: {}",
        path.display()
    );
    let mut data = String::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut data)?;
    ensure!(
        data.len() as u64 <= limit,
        "file exceeds byte budget: {}",
        path.display()
    );
    Ok(data)
}

/// Run-local execution facts; never read project files or infer them from history.
pub fn execution_context(cwd: &Path) -> String {
    let path = serde_json::json!(cwd.to_str());
    format!(
        "\nRuntime working directory (JSON path data): {path}\nRelative file-tool paths resolve from this directory. Every bash tool call starts here; cd changes only that call and does not persist to later calls. Prefer relative paths; do not guess a different workspace root. If the JSON path is null (not UTF-8), use relative paths from the configured directory. This is execution context, not shell code or permission to access other paths.\n"
    )
}

#[derive(Default)]
pub struct ContextFiles {
    pub prompt: String,
    pub readable: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: String,
    #[serde(default, rename = "disable-model-invocation")]
    hidden: bool,
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl ContextFiles {
    fn add(&mut self, text: &str) -> Result<()> {
        ensure!(
            self.prompt.len() + text.len() <= CONTEXT_LIMIT,
            "bootstrap/skills context exceeds 65536 bytes"
        );
        self.prompt.push_str(text);
        Ok(())
    }
}

fn skills(
    dir: &Path,
    root_md: bool,
    seen: &mut HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    ensure!(
        depth <= 32 && seen.len() < 4096,
        "skill discovery exceeds traversal budget"
    );
    let canonical = dir.canonicalize()?;
    if !seen.insert(canonical) {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            skills(&path, true, seen, files, depth + 1)?;
        } else if path.is_file()
            && (path.file_name().is_some_and(|n| n == "SKILL.md")
                || (root_md && path.extension().is_some_and(|e| e == "md")))
        {
            ensure!(files.len() < 4096, "too many skill files");
            files.push(path.canonicalize()?);
        }
    }
    Ok(())
}

/// v0 has no persisted trust database: the CLI's explicit --trust-project is
/// required before *any* project context is read. See docs/v0.md for exclusions.
pub fn discover(cwd: &Path, home: &Path, trusted: bool) -> Result<ContextFiles> {
    let mut out = ContextFiles::default();
    let mut seen = HashSet::new();
    let mut dirs = vec![home.join(".pi/agent")];
    if trusted {
        dirs.extend(
            cwd.ancestors()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(Path::to_path_buf),
        );
    }
    for dir in dirs {
        // This release deliberately supports AGENTS.md, not legacy aliases.
        let path = dir.join("AGENTS.md");
        if path.is_file() {
            let path = path.canonicalize()?;
            ensure!(
                seen.insert(path.clone()),
                "duplicate context path: {}",
                path.display()
            );
            let data = read_bounded(&path, FILE_LIMIT)?;
            out.add(&format!(
                "\n<project_instructions path=\"{}\">\n{}\n</project_instructions>\n",
                xml(&path.display().to_string()),
                data
            ))?;
            out.readable.push(path);
        }
    }
    let mut roots = vec![
        (home.join(".pi/agent/skills"), true),
        (home.join(".agents/skills"), false),
    ];
    if trusted {
        roots.push((cwd.join(".pi/skills"), true));
        for ancestor in cwd.ancestors() {
            roots.push((ancestor.join(".agents/skills"), false));
            if ancestor.join(".git").exists() {
                break;
            }
        }
    }
    let mut files = vec![];
    let mut visited = HashSet::new();
    for (root, root_md) in roots {
        skills(&root, root_md, &mut visited, &mut files, 0)?;
    }
    let mut names = HashSet::new();
    out.add("\n<available_skills>\n")?;
    for path in files {
        if !seen.insert(path.clone()) {
            continue;
        }
        let data =
            read_bounded(&path, FILE_LIMIT).with_context(|| format!("skill {}", path.display()))?;
        let normalized = data.replace("\r\n", "\n");
        let Some(rest) = normalized.strip_prefix("---\n") else {
            continue;
        };
        let Some((header, _)) = rest.split_once("\n---") else {
            continue;
        };
        let Ok(meta) = serde_yaml::from_str::<Frontmatter>(header) else {
            continue;
        };
        if meta.description.trim().is_empty() || meta.hidden {
            continue;
        }
        let name = meta.name.unwrap_or_else(|| {
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        });
        if !names.insert(name.clone()) {
            eprintln!("duplicate skill name ignored: {name}");
            continue;
        }
        out.add(&format!(
            "<skill><name>{}</name><description>{}</description><location>{}</location></skill>\n",
            xml(&name),
            xml(&meta.description),
            xml(&path.display().to_string())
        ))?;
        out.readable.push(path);
    }
    out.add("</available_skills>\nRead the skill file before using its instructions.\n")?;
    Ok(out)
}

/// Read only an explicitly selected private file. Pin every parent and the file
/// descriptor: pathname checks alone allow symlink swaps before open. This does
/// not grant tool read access or enable project instruction discovery.
pub fn private_system_context(path: &Path, cwd: &Path) -> Result<String> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    use std::path::Component;
    ensure!(
        path.is_absolute() && !path.starts_with(cwd),
        "system context must be absolute and outside workspace"
    );
    let parts: Vec<_> = path.components().collect();
    ensure!(
        parts.len() > 1
            && parts[0] == Component::RootDir
            && parts[1..].iter().all(|p| matches!(p, Component::Normal(_))),
        "invalid system context path"
    );
    let mut file = fs::File::open("/")?;
    for (i, part) in parts[1..].iter().enumerate() {
        let name = CString::new(part.as_os_str().as_bytes())?;
        let final_part = i == parts.len() - 2;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if final_part {
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        ensure!(fd >= 0, "system context unavailable");
        file = unsafe { fs::File::from_raw_fd(fd) };
    }
    let info = file.metadata()?;
    ensure!(
        info.is_file()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o777 == 0o600
            && info.nlink() == 1
            && info.len() <= 32768,
        "system context requires an owner-only bounded regular file"
    );
    let mut value = String::new();
    (&mut file)
        .take(32769)
        .read_to_string(&mut value)
        .map_err(|_| anyhow::anyhow!("system context must be valid UTF-8"))?;
    let after = file.metadata()?;
    ensure!(
        info.len() == after.len()
            && info.mtime() == after.mtime()
            && info.mtime_nsec() == after.mtime_nsec()
            && info.ctime() == after.ctime()
            && info.ctime_nsec() == after.ctime_nsec(),
        "system context changed while reading"
    );
    ensure!(
        !value.trim().is_empty() && value.len() <= 32768,
        "invalid system context size"
    );
    Ok(value)
}
