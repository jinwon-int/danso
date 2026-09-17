//! systemd unit rendering for `danso service install|reconcile|uninstall`
//! (`docs/unified-design.md` §6.4).
//!
//! Rendering is a pure function of the spec so the whole contract is testable
//! without systemd — which matters because the hosts that most need this
//! checked (Termux) are exactly the ones that cannot run it.
//!
//! The fields come from ccc-node's `bridge/service-systemd.sh`, which is the
//! behaviour being preserved rather than reinvented:
//!
//! * `KillMode=mixed` + `SendSIGKILL=yes` — SIGTERM reaches only the main
//!   process while it closes admission and drains; at the timeout systemd
//!   SIGKILLs the whole cgroup so descendants cannot survive as orphans.
//! * `Restart=always` with `RestartSec=3` — recover when the service treats a
//!   direct SIGTERM as a clean exit. An explicit `systemctl stop` still
//!   suppresses restart, so operator stop semantics are unchanged.
//! * `TimeoutStopSec` — the **outer** stop budget, the same quantity as
//!   `danso service stop --grace-secs`. See `stop.rs` for why it is not the
//!   turn drain.

use crate::stop::DEFAULT_GRACE_SECS;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `Restart=always` recovery delay, as in ccc-node.
pub const RESTART_SEC: u64 = 3;

/// `UMask` for the service (`docs/unified-design.md` §6.4). State files are
/// created 0600 by their writers; this makes the default restrictive too, so a
/// file added later cannot be world-readable by omission.
pub const UMASK: &str = "0077";

pub const UNIT_NAME: &str = "danso.service";

/// Where a unit is installed and what it is wanted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `/etc/systemd/system`, started at `multi-user.target`.
    System,
    /// `~/.config/systemd/user`, started at `default.target`.
    User,
}

impl Scope {
    pub fn wanted_by(self) -> &'static str {
        match self {
            Self::System => "multi-user.target",
            Self::User => "default.target",
        }
    }

    /// The directory the unit belongs in. `home` is only consulted for
    /// [`Scope::User`], so a system install does not depend on a home at all.
    pub fn unit_dir(self, home: &Path) -> PathBuf {
        match self {
            Self::System => PathBuf::from("/etc/systemd/system"),
            Self::User => home.join(".config/systemd/user"),
        }
    }

    pub fn unit_path(self, home: &Path) -> PathBuf {
        self.unit_dir(home).join(UNIT_NAME)
    }

    /// The `systemctl` argument selecting this scope.
    pub fn systemctl_flag(self) -> Option<&'static str> {
        match self {
            Self::System => None,
            Self::User => Some("--user"),
        }
    }
}

/// Everything the rendered unit depends on.
///
/// It is a value rather than something read from the environment inside
/// `render` so a test can render any host's unit, including one this machine
/// could not otherwise produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitSpec {
    pub scope: Scope,
    /// Absolute path to the `danso` binary.
    pub exe: PathBuf,
    /// State root passed to `service run`.
    pub data_dir: PathBuf,
    pub working_directory: PathBuf,
    pub home: PathBuf,
    pub path_env: String,
}

/// Environment a unit declares, including anything its drop-ins add.
///
/// systemd merges `<unit>.d/*.conf` over the unit itself, so what the service
/// will actually see is both. `install` has to look at both too: a unit that
/// declares nothing is perfectly startable when a drop-in supplies the rest,
/// and refusing that would be wrong.
pub fn declared_environment(unit_text: &str, drop_in_dir: &Path) -> BTreeMap<String, String> {
    let mut declared = BTreeMap::new();
    let mut absorb = |text: &str| {
        for line in text.lines() {
            let line = line.trim();
            let Some(assignment) = line.strip_prefix("Environment=") else {
                continue;
            };
            // `Environment=NAME=VALUE`, optionally quoted. Only the simple form
            // the renderer and a hand-written drop-in use is understood; a
            // shell-quoted multi-assignment line is left alone rather than
            // half-parsed into a wrong answer.
            let assignment = assignment.trim().trim_matches('"');
            if let Some((name, value)) = assignment.split_once('=') {
                declared.insert(name.trim().to_string(), value.to_string());
            }
        }
    };
    absorb(unit_text);
    let Ok(entries) = std::fs::read_dir(drop_in_dir) else {
        return declared;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
        .collect();
    // systemd applies drop-ins in lexical order; later ones win.
    files.sort();
    for file in files {
        if let Ok(text) = std::fs::read_to_string(&file) {
            absorb(&text);
        }
    }
    declared
}

/// `<unit>.d`, where systemd looks for drop-ins.
pub fn drop_in_dir(unit_path: &Path) -> PathBuf {
    let mut name = unit_path.file_name().unwrap_or_default().to_os_string();
    name.push(".d");
    unit_path.with_file_name(name)
}

/// The variable a service publishes its state root through.
///
/// The renderer writes it into the unit and the root package reads it back, so
/// it is defined once here rather than in each. Two copies of a name that has
/// to match is the same shape of bug as the one this export fixes.
pub const DATA_DIR_ENV: &str = "DANSO_TELEGRAM_DATA_DIR";

impl UnitSpec {
    /// Render the unit file.
    ///
    /// Deterministic: the same spec always produces byte-identical output. That
    /// is what makes `reconcile` able to detect drift by comparison rather than
    /// by parsing.
    ///
    /// The state root is published **twice**: as `--data-dir` for the process
    /// and as `DANSO_TELEGRAM_DATA_DIR` for everything that has to ask the
    /// installation about itself afterwards. Measured on yukson 2026-09-17
    /// (#118): with only the argument, `fleet-bridge-watch.sh` ran
    /// `danso service status --json` with no way to learn the directory, got
    /// `unavailable`, and reported a serving node as `AVAIL=no` — the
    /// false-DOWN class the watch exists to prevent.
    pub fn render(&self) -> String {
        format!(
            "[Unit]\n\
             Description=Danso resident service\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             [Service]\n\
             Type=simple\n\
             WorkingDirectory={working_directory}\n\
             Environment=HOME={home}\n\
             Environment=PATH={path_env}\n\
             Environment={data_dir_env}={data_dir}\n\
             ExecStart={exe} service run --data-dir {data_dir}\n\
             UMask={umask}\n\
             Restart=always\n\
             RestartSec={restart_sec}\n\
             KillMode=mixed\n\
             SendSIGKILL=yes\n\
             TimeoutStopSec={timeout_stop_sec}\n\
             [Install]\n\
             WantedBy={wanted_by}\n",
            working_directory = self.working_directory.display(),
            home = self.home.display(),
            path_env = self.path_env,
            exe = self.exe.display(),
            data_dir_env = DATA_DIR_ENV,
            data_dir = self.data_dir.display(),
            umask = UMASK,
            restart_sec = RESTART_SEC,
            // The unit's stop allowance and the CLI's default are the same
            // quantity. Writing the constant rather than a literal is what keeps
            // them from drifting apart in opposite directions.
            timeout_stop_sec = DEFAULT_GRACE_SECS,
            wanted_by = self.scope.wanted_by(),
        )
    }
}

/// What `reconcile` found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// No unit is installed.
    Absent,
    /// The installed unit matches what this binary would render.
    InSync,
    /// The installed unit differs. Carries the on-disk text so the caller can
    /// show a diff; `reconcile` reports, it does not overwrite.
    Differs { installed: String },
}

impl Drift {
    /// `0` in sync, `1` drifted, `2` absent.
    ///
    /// Absent is worse than drifted: a drifted unit is still supervising
    /// something, while an absent one means nothing is.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InSync => 0,
            Self::Differs { .. } => 1,
            Self::Absent => 2,
        }
    }

    pub fn summary(&self) -> &'static str {
        match self {
            Self::InSync => "Unit: in sync",
            Self::Differs { .. } => "Unit: drifted",
            Self::Absent => "Unit: not installed",
        }
    }
}

/// Compare the installed unit against what `spec` renders.
///
/// An unreadable-but-present unit is reported as drifted rather than absent:
/// claiming nothing is installed would invite an install on top of it.
pub fn drift(spec: &UnitSpec, unit_path: &Path) -> Drift {
    match std::fs::read_to_string(unit_path) {
        Ok(installed) if installed == spec.render() => Drift::InSync,
        Ok(installed) => Drift::Differs { installed },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Drift::Absent,
        Err(error) => Drift::Differs {
            installed: format!("<unreadable: {error}>"),
        },
    }
}

/// The Termux:Boot script that stands in for a unit where systemd is absent.
///
/// `install` prints this path and does **not** create it: writing into
/// `~/.termux/boot` changes what happens at device boot, which is a host
/// change an operator should make deliberately.
pub fn termux_boot_path(home: &Path) -> PathBuf {
    home.join(".termux/boot/danso-service")
}

/// The body an operator would put at [`termux_boot_path`].
pub fn termux_boot_script(exe: &Path, data_dir: &Path) -> String {
    format!(
        "#!/data/data/com.termux/files/usr/bin/sh\n\
         # Danso resident service (no systemd on Termux).\n\
         # `--supervise` provides the crash policy that `Restart=always` would.\n\
         termux-wake-lock\n\
         exec {exe} service run --supervise --data-dir {data_dir}\n",
        exe = exe.display(),
        data_dir = data_dir.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(scope: Scope) -> UnitSpec {
        UnitSpec {
            scope,
            exe: PathBuf::from("/usr/local/bin/danso"),
            data_dir: PathBuf::from("/root/.danso/telegram"),
            working_directory: PathBuf::from("/root/.danso"),
            home: PathBuf::from("/root"),
            path_env: "/usr/local/bin:/usr/bin:/bin".to_string(),
        }
    }

    #[test]
    fn the_unit_stop_allowance_is_the_outer_budget_constant() {
        // The single most important line in this file: the unit's allowance and
        // `service stop --grace-secs` must be the same quantity. A literal here
        // would let them drift in opposite directions and silently reintroduce
        // the collapsed-budget bug this row exists to avoid.
        assert!(
            spec(Scope::System)
                .render()
                .contains(&format!("TimeoutStopSec={DEFAULT_GRACE_SECS}")),
            "the unit must render the outer budget constant"
        );
        assert!(spec(Scope::System).render().contains("TimeoutStopSec=70"));
    }

    #[test]
    fn the_rendered_unit_carries_the_ccc_node_lifecycle_fields() {
        let unit = spec(Scope::System).render();
        for required in [
            "[Unit]",
            "After=network-online.target",
            "Wants=network-online.target",
            "[Service]",
            "Type=simple",
            "Restart=always",
            "RestartSec=3",
            // SIGTERM reaches the main process only; the cgroup is killed at
            // the timeout so descendants cannot survive as orphans.
            "KillMode=mixed",
            "SendSIGKILL=yes",
            "UMask=0077",
            "[Install]",
        ] {
            assert!(unit.contains(required), "missing {required} in:\n{unit}");
        }
    }

    #[test]
    fn exec_start_runs_the_service_not_a_second_runtime() {
        let unit = spec(Scope::System).render();
        assert!(unit.contains("ExecStart=/usr/local/bin/danso service run --data-dir "));
        assert!(
            !unit.contains("--supervise"),
            "under systemd, Restart=always is the supervisor; a second one would fight it"
        );
    }

    #[test]
    fn scope_selects_the_target_and_the_install_directory() {
        assert_eq!(Scope::System.wanted_by(), "multi-user.target");
        assert_eq!(Scope::User.wanted_by(), "default.target");
        assert!(
            spec(Scope::System)
                .render()
                .contains("WantedBy=multi-user.target")
        );
        assert!(
            spec(Scope::User)
                .render()
                .contains("WantedBy=default.target")
        );

        let home = Path::new("/home/agent");
        assert_eq!(
            Scope::System.unit_path(home),
            PathBuf::from("/etc/systemd/system/danso.service")
        );
        assert_eq!(
            Scope::User.unit_path(home),
            PathBuf::from("/home/agent/.config/systemd/user/danso.service")
        );
        assert_eq!(Scope::System.systemctl_flag(), None);
        assert_eq!(Scope::User.systemctl_flag(), Some("--user"));
    }

    #[test]
    fn rendering_is_deterministic() {
        // Drift detection compares text, so an unstable renderer would report
        // drift on every reconcile of an untouched unit.
        assert_eq!(spec(Scope::System).render(), spec(Scope::System).render());
        assert_ne!(spec(Scope::System).render(), spec(Scope::User).render());
    }

    #[test]
    fn drift_distinguishes_absent_from_in_sync_from_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(UNIT_NAME);
        let spec = spec(Scope::System);

        assert_eq!(drift(&spec, &path), Drift::Absent);
        assert_eq!(drift(&spec, &path).exit_code(), 2);

        std::fs::write(&path, spec.render()).unwrap();
        assert_eq!(drift(&spec, &path), Drift::InSync);
        assert_eq!(drift(&spec, &path).exit_code(), 0);

        std::fs::write(
            &path,
            spec.render()
                .replace("TimeoutStopSec=70", "TimeoutStopSec=10"),
        )
        .unwrap();
        match drift(&spec, &path) {
            Drift::Differs { installed } => assert!(installed.contains("TimeoutStopSec=10")),
            other => panic!("expected drift, got {other:?}"),
        }
        assert_eq!(drift(&spec, &path).exit_code(), 1);
    }

    #[test]
    fn a_present_but_unreadable_unit_is_drift_not_absence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(UNIT_NAME);
        // A directory where the unit belongs: present, unreadable as text.
        std::fs::create_dir(&path).unwrap();
        assert!(
            matches!(drift(&spec(Scope::System), &path), Drift::Differs { .. }),
            "reporting this as absent would invite installing on top of it"
        );
    }

    #[test]
    fn the_termux_fallback_supervises_because_nothing_else_will() {
        let script = termux_boot_script(
            Path::new("/data/data/com.termux/files/usr/bin/danso"),
            Path::new("/data/data/com.termux/files/home/.danso/telegram"),
        );
        assert!(
            script.contains("--supervise"),
            "without systemd, --supervise is the only restart policy there is"
        );
        assert!(script.contains("termux-wake-lock"));
        assert_eq!(
            termux_boot_path(Path::new("/data/data/com.termux/files/home")),
            PathBuf::from("/data/data/com.termux/files/home/.termux/boot/danso-service")
        );
    }

    #[test]
    fn declared_environment_merges_the_unit_and_its_drop_ins() {
        let dir = tempfile::tempdir().unwrap();
        let drop_ins = dir.path().join("danso.service.d");
        std::fs::create_dir_all(&drop_ins).unwrap();
        // Lexical order, later wins — the order systemd applies them in.
        std::fs::write(
            drop_ins.join("10-first.conf"),
            "[Service]\nEnvironment=A=one\nEnvironment=B=keep\n",
        )
        .unwrap();
        std::fs::write(drop_ins.join("20-second.conf"), "Environment=A=two\n").unwrap();
        // Not a drop-in: systemd only reads `*.conf`.
        std::fs::write(drop_ins.join("notes.txt"), "Environment=C=ignored\n").unwrap();

        let unit = "[Service]\nEnvironment=HOME=/root\nEnvironment=A=unit\n";
        let declared = declared_environment(unit, &drop_ins);
        assert_eq!(declared.get("HOME").map(String::as_str), Some("/root"));
        assert_eq!(
            declared.get("A").map(String::as_str),
            Some("two"),
            "the last drop-in wins, as systemd applies them"
        );
        assert_eq!(declared.get("B").map(String::as_str), Some("keep"));
        assert!(!declared.contains_key("C"), "only *.conf is a drop-in");
    }

    #[test]
    fn a_missing_drop_in_directory_is_not_an_error() {
        let declared = declared_environment(
            "Environment=A=1\n",
            Path::new("/nonexistent/danso.service.d"),
        );
        assert_eq!(declared.get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn the_drop_in_directory_is_the_unit_path_plus_d() {
        assert_eq!(
            drop_in_dir(Path::new("/etc/systemd/system/danso.service")),
            PathBuf::from("/etc/systemd/system/danso.service.d")
        );
    }

    #[test]
    fn the_unit_publishes_the_state_root_to_anything_that_asks_later() {
        let rendered = spec(Scope::System).render();
        assert!(
            rendered.contains("Environment=DANSO_TELEGRAM_DATA_DIR=/root/.danso/telegram\n"),
            "the state root has to be readable from the unit, not only passed \
             as an argument: `fleet-bridge-watch.sh` runs `danso service \
             status --json` with no arguments and no other way to learn the \
             directory. Measured on yukson 2026-09-17 (#118), a serving node \
             reported AVAIL=no without this line.\n{rendered}"
        );
        // Both, and they have to agree. The argument is what the process uses;
        // the variable is what everything else reads.
        assert!(
            rendered.contains("--data-dir /root/.danso/telegram\n"),
            "{rendered}"
        );
    }

    #[test]
    fn the_exported_name_is_the_one_the_binary_reads() {
        assert_eq!(DATA_DIR_ENV, "DANSO_TELEGRAM_DATA_DIR");
        assert!(
            spec(Scope::System)
                .render()
                .contains(&format!("Environment={DATA_DIR_ENV}=")),
            "the unit must export the same name the binary resolves from"
        );
    }

    #[test]
    fn the_unit_snapshot_is_stable() {
        // A full snapshot so any field change has to be an intentional edit
        // here, not an unnoticed side effect.
        assert_eq!(
            spec(Scope::System).render(),
            "[Unit]\n\
             Description=Danso resident service\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             [Service]\n\
             Type=simple\n\
             WorkingDirectory=/root/.danso\n\
             Environment=HOME=/root\n\
             Environment=PATH=/usr/local/bin:/usr/bin:/bin\n\
             Environment=DANSO_TELEGRAM_DATA_DIR=/root/.danso/telegram\n\
             ExecStart=/usr/local/bin/danso service run --data-dir /root/.danso/telegram\n\
             UMask=0077\n\
             Restart=always\n\
             RestartSec=3\n\
             KillMode=mixed\n\
             SendSIGKILL=yes\n\
             TimeoutStopSec=70\n\
             [Install]\n\
             WantedBy=multi-user.target\n"
        );
    }
}
