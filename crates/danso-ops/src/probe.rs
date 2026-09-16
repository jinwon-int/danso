//! Read-only process probes.
//!
//! `/proc` is preferred over `ps` so the results do not depend on platform `ps`
//! flag differences (the same reason ccc-node's `_stop_process_state` reads
//! `/proc/<pid>/status` first).

use std::path::Path;

/// Process states we care about. Anything we cannot read is reported as
/// `Unknown` and treated conservatively as live by [`pid_alive`], matching
/// ccc-node's "unknown state remains conservatively live" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcState {
    Zombie,
    Other,
    Unknown,
}

/// Read `State:` from `/proc/<pid>/status`.
pub fn process_state(pid: u32) -> ProcState {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return ProcState::Unknown;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("State:") {
            return match rest.split_whitespace().next() {
                Some(state) if state.starts_with('Z') => ProcState::Zombie,
                Some(_) => ProcState::Other,
                None => ProcState::Unknown,
            };
        }
    }
    ProcState::Unknown
}

/// Whether `pid` is a live process.
///
/// A zombie satisfies `kill -0` until its parent reaps it, but it has already
/// exited: it cannot poll and it cannot retain an open lock descriptor. Counting
/// it as live is what makes a stop wait out its whole budget for nothing, so it
/// is excluded here exactly as ccc-node's `stop_pid_alive` excludes it.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs error checking only and delivers no signal.
    if unsafe { libc::kill(pid, 0) } != 0 {
        let error = std::io::Error::last_os_error();
        // EPERM means the process exists but belongs to another user.
        if error.raw_os_error() != Some(libc::EPERM) {
            return false;
        }
    }
    process_state(pid as u32) != ProcState::Zombie
}

/// The argv of `pid`, NUL-separated in `/proc/<pid>/cmdline`.
pub fn cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

/// The current boot's identifier.
///
/// A pid recorded before a reboot can collide with an unrelated process after
/// one, so every pid record is scoped to the boot that produced it. When the
/// kernel does not expose one, callers get `None` and must not treat a pid
/// record as reusable evidence.
pub fn boot_id() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The live process holding an open descriptor on `path`, if we can identify one.
///
/// Callers establish *that* a lock is held with a non-blocking lock attempt;
/// this answers *who*, by looking for the open descriptor in `/proc/<pid>/fd`.
/// `predicate` filters candidates by argv so an unrelated process that merely
/// opened the file is not mistaken for the service.
///
/// Returns `Err` when `/proc` itself could not be listed: the caller must
/// surface that as indeterminate rather than as "nobody holds it".
pub fn descriptor_holder(
    path: &Path,
    predicate: impl Fn(&[String]) -> bool,
) -> Result<Option<u32>, std::io::Error> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let self_pid = std::process::id();
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid || !pid_alive(pid) {
            continue;
        }
        // An unreadable fd directory is another user's process, not evidence.
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let mut holds = false;
        for fd in fds {
            let Ok(fd) = fd else { continue };
            if std::fs::read_link(fd.path()).is_ok_and(|link| link == target) {
                holds = true;
                break;
            }
        }
        if !holds {
            continue;
        }
        if cmdline(pid).is_some_and(|argv| predicate(&argv)) {
            return Ok(Some(pid));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_live_and_has_argv() {
        let pid = std::process::id();
        assert!(pid_alive(pid));
        assert_ne!(process_state(pid), ProcState::Zombie);
        assert!(cmdline(pid).is_some_and(|argv| !argv.is_empty()));
    }

    #[test]
    fn pid_zero_is_never_live() {
        // kill(0, 0) addresses the caller's process group, which would report
        // success and make every status read "available".
        assert!(!pid_alive(0));
    }

    #[test]
    fn a_zombie_is_not_live() {
        // `kill -0` succeeds on a zombie until its parent reaps it, so a
        // liveness check built on the signal alone reports an exited process as
        // running: `status` would say available and `stop` would wait out its
        // whole budget for a process that is already gone.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn a short-lived child");
        let pid = child.id();
        // Deliberately not reaped yet, so the entry stays in state Z.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while process_state(pid) != ProcState::Zombie {
            assert!(
                std::time::Instant::now() < deadline,
                "child never reached the zombie state; the fixture cannot test this"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // SAFETY: signal 0 only performs error checking.
        assert_eq!(
            unsafe { libc::kill(pid as i32, 0) },
            0,
            "the fixture requires that kill -0 still succeeds on the zombie"
        );
        assert!(
            !pid_alive(pid),
            "a zombie has exited and must not count as live"
        );
        child.wait().expect("reap the child");
    }

    #[test]
    fn unused_pid_is_not_live() {
        // A pid far above the configured maximum cannot be allocated.
        assert!(!pid_alive(u32::MAX - 1));
        assert_eq!(process_state(u32::MAX - 1), ProcState::Unknown);
        assert!(cmdline(u32::MAX - 1).is_none());
    }

    #[test]
    fn descriptor_holder_finds_an_open_file_in_this_process_only_via_predicate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.lock");
        let _file = std::fs::File::create(&path).unwrap();
        // The scan skips the calling process, so its own descriptor is invisible.
        assert_eq!(descriptor_holder(&path, |_| true).unwrap(), None);
    }
}
