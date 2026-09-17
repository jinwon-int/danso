//! `danso service` — the resident-service surface (`docs/unified-design.md` §6.4).
//!
//! This is the composition root for the service: it owns the CLI, the process
//! bookkeeping and the environment, and it hands the actual work to the
//! existing Telegram loop. `run` does not implement a second polling loop —
//! duplicating the runtime is the failure this module exists to avoid, and
//! `danso telegram` stays as an alias for the same path.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use danso_ops::{
    health::{self, HEALTH_FILE_NAME, RunMode},
    pidfile::{PidFile, SERVICE_PID_FILE_NAME, ServicePid},
    status::{StatusInputs, StatusReport},
    stop::{StopOutcome, TURN_DRAIN_SECS},
    unit::{Drift, Scope, UnitSpec},
};
use std::path::Path;
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    name = "danso service",
    about = "Run and inspect the resident Danso service. No provider or network access from `status`."
)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Run the service in the foreground with pid and health bookkeeping.
    Run {
        /// State root. Defaults to the Telegram data directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Supervise the service instead of running it directly.
        ///
        /// For hosts without systemd (Termux). Restarts the service after a
        /// crash and gives up on a crash loop rather than spinning.
        #[arg(long)]
        supervise: bool,
    },
    /// Report whether the service is available, degraded or unavailable.
    Status {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Emit the machine-readable report instead of the text rendering.
        #[arg(long)]
        json: bool,
    },
    /// Render and install the systemd unit. Does not start the service.
    Install {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Install into the per-user scope instead of the system scope.
        #[arg(long)]
        user: bool,
        /// Print the rendered unit and the target path; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Compare the installed unit against what this binary renders.
    ///
    /// Reports drift and reloads the systemd daemon. It never rewrites the
    /// unit, restarts the service or changes whether it is enabled.
    Reconcile {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long)]
        user: bool,
    },
    /// Disable and remove the unit. Refuses while the service is still serving.
    Uninstall {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long)]
        user: bool,
    },
    /// Stop the running service within a bounded wall-clock budget.
    Stop {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Outer wall-clock budget for the whole stop, in seconds.
        ///
        /// This is not the turn drain: the service drains active turns for
        /// `TURN_DRAIN_SECS` and then tears down, and this budget has to cover
        /// both. Values below the default can cut teardown short.
        #[arg(long, default_value_t = danso_ops::stop::DEFAULT_GRACE_SECS,
              value_parser = grace_secs)]
        grace_secs: u64,
    },
}

/// Reject an out-of-range budget during argument parsing, which runs before
/// any signal is sent. ccc-node makes the same ordering explicit; a rejected
/// budget must leave the service exactly as it was, not half-stopped.
fn grace_secs(raw: &str) -> Result<u64, String> {
    danso_ops::stop::parse_grace_secs(raw)
}

/// Whether an argv belongs to a Danso service process.
///
/// Used to attribute a held token lock. It must not match an unrelated process
/// that merely has the lock file open, and it must not match this very
/// inspection — `status` never takes the lock, so it never appears as a holder.
pub fn is_service_argv(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    let is_danso = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "danso" || name.starts_with("danso-"));
    if !is_danso {
        return false;
    }
    argv.iter()
        .skip(1)
        .any(|arg| arg == "service" || arg == "telegram")
        && !argv.iter().any(|arg| arg == "status")
}

/// `update activate` needs the same health document `service status` reads;
/// resolving it twice in two places is how the two end up looking at
/// different files.
pub(crate) fn resolve_data_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(dir) => Ok(dir),
        None => crate::telegram::data_dir_from_env().context(crate::telegram::DATA_DIR_ENV),
    }
}

/// Run the service in the foreground. Returns only when the loop ends.
///
/// SIGTERM leaves the polling loop, which stops admitting new turns, then
/// drains the turns already running for [`TURN_DRAIN_SECS`]. Teardown — the
/// token lock and the pid record — happens after that, on the way out of this
/// function, which is what the outer budget's remaining seconds pay for.
pub fn run(data_dir: Option<PathBuf>) -> Result<()> {
    let data_dir = resolve_data_dir(data_dir)?;
    // The loop resolves its own state root from the environment, so an explicit
    // `--data-dir` has to be published there rather than threaded separately;
    // two sources for one path is how they drift apart.
    unsafe { std::env::set_var(crate::telegram::DATA_DIR_ENV, &data_dir) };
    crate::telegram::ensure_private_dir(&data_dir)?;
    health::set_run_mode(RunMode::Run);

    let argv: Vec<String> = std::env::args().collect();
    // Bookkeeping is acquired before the loop so a second instance is refused
    // here rather than after it has started touching state. The token lock is
    // still the ownership signal; this only makes ownership *trackable*.
    let _pid = PidFile::acquire(
        &data_dir,
        SERVICE_PID_FILE_NAME,
        ServicePid::for_current_process(&argv),
    )?;

    let service = crate::telegram::TelegramService::from_env()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the service runtime")?;
    let report = runtime.block_on(async move {
        let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
        let signalled = std::sync::Arc::clone(&shutdown);
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            signalled.notify_waiters();
        });
        service
            .run_until_drained(shutdown, Duration::from_secs(TURN_DRAIN_SECS))
            .await
    })?;
    if !report.is_complete() {
        // Not an error: the drain is bounded by contract. Say so plainly so the
        // operator knows work was cut, and keep it body-free.
        eprintln!(
            "service drained with {} turn(s) still running; journals retained, nothing replayed",
            report.still_running
        );
    }
    Ok(())
}

/// Stop the running service within `grace`, then report what happened.
///
/// A kill is a failed stop: bookkeeping is retained and the exit code is
/// non-zero, so a supervisor does not start a replacement on top of a process
/// whose teardown never ran.
pub fn stop(data_dir: Option<PathBuf>, grace: Duration) -> Result<StopOutcome> {
    let data_dir = resolve_data_dir(data_dir)?;
    let pid_path = data_dir.join(SERVICE_PID_FILE_NAME);
    let Some(record) = danso_ops::pidfile::read(&pid_path)? else {
        // No usable record. The lock may still be held by an unbookkept
        // process, but stopping something we cannot attribute is how an
        // unrelated pid gets signalled; `status` reports that case as degraded
        // and it is resolved by hand.
        return Ok(StopOutcome::NotRunning);
    };
    if !record.is_live() {
        return Ok(StopOutcome::NotRunning);
    }
    // Announce the stop before signalling. A supervisor that reaps the child
    // between the signal and the marker would read a kill as a crash and start
    // a replacement on top of a teardown that never ran.
    let _marker = danso_ops::stop::begin_stop(&data_dir, record.pid)?;
    Ok(danso_ops::stop::terminate_and_wait(record.pid, grace))
}

/// Read the service state. Never acquires a lock, creates state, or calls out.
pub fn status(data_dir: Option<PathBuf>) -> Result<StatusReport> {
    let data_dir = resolve_data_dir(data_dir)?;
    let pid_path = data_dir.join(SERVICE_PID_FILE_NAME);
    let health_path = data_dir.join(HEALTH_FILE_NAME);
    let lock_path = data_dir.join(crate::telegram::TOKEN_LOCK_FILE_NAME);
    Ok(danso_ops::status::determine(
        &StatusInputs {
            pid_path: &pid_path,
            health_path: &health_path,
            lock_path: &lock_path,
        },
        chrono::Utc::now(),
        is_service_argv,
    ))
}

/// Supervise the service on a host without systemd.
///
/// The supervisor runs the service as a child rather than in-process: a crash
/// that takes down the runtime must not take the supervisor with it, which is
/// the whole point of having one. The child does its own pid and health
/// bookkeeping, so this loop owns only the restart decision.
pub fn supervise(data_dir: Option<PathBuf>) -> Result<i32> {
    let data_dir = resolve_data_dir(data_dir)?;
    crate::telegram::ensure_private_dir(&data_dir)?;
    health::set_run_mode(RunMode::Supervise);
    let exe = std::env::current_exe().context("could not resolve the running binary")?;

    // A wake lock keeps Android from suspending the process between polls.
    // Absent elsewhere, and its absence is not an error.
    if let Ok(wake_lock) = which("termux-wake-lock") {
        let _ = std::process::Command::new(wake_lock).status();
    }

    let mut policy = danso_ops::CrashPolicy::new();
    loop {
        // `spawn`, not `status`: the child's pid is what ties a death to the
        // stop marker, and `status` throws it away.
        let mut child = std::process::Command::new(&exe)
            .arg("service")
            .arg("run")
            .arg("--data-dir")
            .arg(&data_dir)
            .spawn()
            .context("could not start the supervised service")?;
        let child_pid = child.id();
        let status = child.wait().context("could not wait for the service")?;
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        // A signal alone does not say who sent it. `service stop` escalates to
        // SIGKILL when the budget runs out, so the marker it leaves is what
        // separates that from the service being killed — by the OOM killer
        // above all, which is the likeliest death on the one platform where
        // this loop *is* the restart policy.
        let stopping = danso_ops::stop::stop_in_progress(&data_dir, child_pid, chrono::Utc::now());
        let clean = danso_ops::supervise::is_operator_stop(status.success(), signal, stopping);
        match policy.record(clean, std::time::Instant::now()) {
            danso_ops::Decision::Stop => return Ok(0),
            danso_ops::Decision::Restart => {}
            danso_ops::Decision::CrashLoop => {
                eprintln!(
                    "service supervision stopped: {} rapid crashes ({})",
                    policy.rapid_crashes(),
                    danso_ops::supervise::CRASH_LOOP_REASON
                );
                return Ok(danso_ops::supervise::CRASH_LOOP_EXIT);
            }
        }
    }
}

fn which(program: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is unset")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .context("program not found")
}

/// Build the unit spec for this host.
///
/// `exe` is the running binary rather than a name looked up on `PATH`: the unit
/// must keep pointing at the image that installed it, not at whatever a future
/// `PATH` happens to resolve.
pub fn unit_spec(data_dir: Option<PathBuf>, user: bool) -> Result<UnitSpec> {
    let data_dir = resolve_data_dir(data_dir)?;
    let home = home_dir()?;
    Ok(UnitSpec {
        scope: if user { Scope::User } else { Scope::System },
        exe: std::env::current_exe().context("could not resolve the running binary")?,
        working_directory: home.clone(),
        data_dir,
        home,
        path_env: std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
    })
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .context("HOME must be set to an absolute path")
}

/// Whether this host has systemd to install into.
///
/// Termux has none, and that is the supported case rather than an error: the
/// caller prints the Termux:Boot equivalent instead.
pub fn has_systemd() -> bool {
    which("systemctl").is_ok()
}

/// What an install attempt did.
pub enum InstallOutcome {
    /// Nothing was written. `unit` is the exact bytes that would have been.
    DryRun { path: PathBuf, unit: String },
    /// The unit was written, the daemon reloaded and the unit enabled.
    Installed { path: PathBuf },
}

/// Render the unit and, unless `dry_run`, write it and reload the daemon.
///
/// Installing does **not** start the service. Starting is a lifecycle decision
/// an operator makes; conflating it with installation means a config change
/// silently becomes a restart.
/// What would stop the unit `install` is about to write from ever starting.
///
/// Measured on yukson 2026-09-17 (#118): a freshly installed unit crash-looped
/// thirteen times because it declared none of the configuration the service
/// requires, and `install` had reported success. An install whose only possible
/// outcome is a crash loop is not an install.
///
/// Both checks are decidable here, before anything is written: the unit's own
/// `Environment=` lines plus its drop-ins are exactly what systemd will hand
/// the service, and the working directory and state root are both in the spec.
///
/// Only names are reported, never values — one of them is a bot token.
pub fn install_problems(spec: &UnitSpec, unit: &str, unit_path: &Path) -> Vec<String> {
    let declared =
        danso_ops::unit::declared_environment(unit, &danso_ops::unit::drop_in_dir(unit_path));
    let mut problems = Vec::new();

    if !declared.contains_key(crate::telegram::BOT_TOKEN_ENV) {
        problems.push(format!(
            "{} is not set for the unit",
            crate::telegram::BOT_TOKEN_ENV
        ));
    }
    // The model can arrive under any of the names the service accepts; naming
    // only the canonical one in the error would send an operator to change a
    // variable that was already fine.
    let model_names = [crate::telegram::MODEL_ENV, "DANSO_MODEL"];
    if !model_names.iter().any(|name| declared.contains_key(*name)) {
        problems.push(format!(
            "none of {} is set for the unit",
            model_names.join(", ")
        ));
    }

    // With no workspace declared the service falls back to its working
    // directory, which this unit sets to the home that contains the state
    // root — and journals inside the workspace are refused. The default
    // layout is self-contradictory, so it has to be said at install time
    // rather than discovered in the journal.
    let workspace = declared
        .get(crate::telegram::WORKSPACE_ENV)
        .or_else(|| declared.get("DANSO_WORKSPACE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| spec.working_directory.clone());
    if spec.data_dir.starts_with(&workspace) {
        problems.push(format!(
            "the state root is inside the workspace the unit would use; set {} to a directory that does not contain it",
            crate::telegram::WORKSPACE_ENV
        ));
    }
    problems
}

fn drop_in_hint(unit_path: &Path) -> String {
    format!(
        "supply them through a systemd drop-in at {}/10-danso.conf \
         ([Service] with one Environment= line each); the unit itself must not \
         carry the bot token, because it is world-readable",
        danso_ops::unit::drop_in_dir(unit_path).display()
    )
}

pub fn install(spec: &UnitSpec, dry_run: bool) -> Result<InstallOutcome> {
    let path = spec.scope.unit_path(&spec.home);
    let unit = spec.render();
    let problems = install_problems(spec, &unit, &path);
    if dry_run {
        // A dry run changes nothing, so it reports rather than refuses — the
        // rendered unit on stdout stays usable as `--dry-run > danso.service`.
        for problem in &problems {
            eprintln!("warning: {problem}");
        }
        if !problems.is_empty() {
            eprintln!("warning: {}", drop_in_hint(&path));
        }
        return Ok(InstallOutcome::DryRun { path, unit });
    }
    if !problems.is_empty() {
        bail!(
            "refusing to install a unit that cannot start:\n  - {}\n{}",
            problems.join("\n  - "),
            drop_in_hint(&path)
        );
    }
    let dir = path.parent().context("unit path has no parent")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create unit directory: {}", dir.display()))?;
    std::fs::write(&path, &unit).with_context(|| format!("write unit: {}", path.display()))?;
    // Unlike reconcile's, these are substantive: a unit that was written but
    // never loaded or enabled is not installed, and reporting success would be
    // a lie the operator only discovers at the next boot.
    systemctl(spec.scope, &["daemon-reload"])?;
    systemctl(spec.scope, &["enable", danso_ops::unit::UNIT_NAME])?;
    Ok(InstallOutcome::Installed { path })
}

/// Report drift and reload the daemon. Changes nothing else.
pub fn reconcile(spec: &UnitSpec) -> Result<Drift> {
    let path = spec.scope.unit_path(&spec.home);
    let drift = danso_ops::unit::drift(spec, &path);
    // The reload is safe on its own: it re-reads unit files without restarting
    // or enabling anything. The drift itself is reported, never repaired here —
    // rewriting a unit an operator edited on purpose is not reconciliation.
    //
    // A failed reload does not fail the command. Reporting drift is what
    // `reconcile` is for, and `systemctl --user` fails outright on a host with
    // no user session bus (headless root, CI). Turning that into an error would
    // hide the answer behind an unrelated environment limitation.
    if has_systemd() {
        systemctl_best_effort(spec.scope, &["daemon-reload"]);
    }
    Ok(drift)
}

/// Disable and remove the unit.
///
/// Refuses while the service is still serving. Removing the unit under a live
/// process leaves something running that nothing supervises and that no unit
/// describes — the exact state `status` calls degraded.
pub fn uninstall(spec: &UnitSpec) -> Result<String> {
    let report = status(Some(spec.data_dir.clone()))?;
    if matches!(
        report.outcome(),
        danso_ops::StatusOutcome::State(
            danso_ops::ServiceState::Available | danso_ops::ServiceState::Degraded
        )
    ) {
        anyhow::bail!(
            "service is still {} — stop it before removing its unit",
            report.state
        );
    }
    let path = spec.scope.unit_path(&spec.home);
    if has_systemd() {
        // A unit that was never enabled makes `disable` fail; that is not a
        // reason to leave the file behind.
        let _ = systemctl(spec.scope, &["disable", danso_ops::unit::UNIT_NAME]);
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok("Unit: not installed".to_string());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("remove unit: {}", path.display()));
        }
    }
    // The removal is the substantive act and it already happened. A reload that
    // cannot run must not turn a completed uninstall into a reported failure.
    if has_systemd() {
        systemctl_best_effort(spec.scope, &["daemon-reload"]);
    }
    Ok(format!("Unit: removed {}", path.display()))
}

/// Run `systemctl` and report failure on stderr without failing the command.
///
/// For steps whose failure does not invalidate the result the caller already
/// produced. The failure is still surfaced — silently swallowing it would leave
/// an operator believing systemd picked up a change it never saw.
fn systemctl_best_effort(scope: Scope, args: &[&str]) {
    if let Err(error) = systemctl(scope, args) {
        eprintln!("warning: {error}");
    }
}

fn systemctl(scope: Scope, args: &[&str]) -> Result<()> {
    let mut command = std::process::Command::new("systemctl");
    if let Some(flag) = scope.systemctl_flag() {
        command.arg(flag);
    }
    command.args(args);
    let status = command
        .status()
        .with_context(|| format!("run systemctl {}", args.join(" ")))?;
    anyhow::ensure!(
        status.success(),
        "systemctl {} failed with {status}",
        args.join(" ")
    );
    Ok(())
}

/// The text `install` prints where systemd is absent.
pub fn termux_guidance(spec: &UnitSpec) -> String {
    let path = danso_ops::unit::termux_boot_path(&spec.home);
    format!(
        "Unit: systemd not present; nothing was installed.\n\
         Termux:Boot equivalent — create {} yourself with:\n\n{}",
        path.display(),
        danso_ops::unit::termux_boot_script(&spec.exe, &spec.data_dir)
    )
}

/// The text rendering of a stop outcome. Kept beside the states it names so a
/// new outcome cannot be added without deciding what it prints.
pub fn stop_text(outcome: StopOutcome) -> &'static str {
    match outcome {
        StopOutcome::NotRunning => "Bot stop: not running",
        StopOutcome::Drained => "Bot stop: drained",
        StopOutcome::Killed => "Bot stop: killed after the grace budget; pid and lock retained",
        StopOutcome::Survived => "Bot stop: target survived SIGKILL; pid and lock retained",
    }
}

#[cfg(test)]
mod install_preflight_tests {
    use super::*;
    use danso_ops::unit::Scope;

    fn spec(home: &Path, data_dir: &Path) -> UnitSpec {
        UnitSpec {
            scope: Scope::System,
            exe: PathBuf::from("/usr/local/bin/danso"),
            working_directory: home.to_path_buf(),
            data_dir: data_dir.to_path_buf(),
            home: home.to_path_buf(),
            path_env: "/usr/bin:/bin".into(),
        }
    }

    /// The state the acceptance run found: nothing supplied, and a state root
    /// inside the working directory the unit sets.
    #[test]
    fn a_bare_install_names_every_reason_it_cannot_start() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let spec = spec(home, &home.join(".danso/telegram"));
        let unit = spec.render();
        let problems = install_problems(&spec, &unit, &home.join("danso.service"));

        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("DANSO_TELEGRAM_BOT_TOKEN"))
        );
        assert!(problems.iter().any(|p| p.contains("DANSO_TELEGRAM_MODEL")));
        assert!(problems.iter().any(|p| p.contains("inside the workspace")));
    }

    #[test]
    fn a_drop_in_that_supplies_them_clears_the_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let workspace = home.join("ws");
        let spec = spec(home, &home.join(".danso/telegram"));
        let unit_path = home.join("danso.service");
        let drop_ins = danso_ops::unit::drop_in_dir(&unit_path);
        std::fs::create_dir_all(&drop_ins).unwrap();
        std::fs::write(
            drop_ins.join("10-danso.conf"),
            format!(
                "[Service]\nEnvironment={}=000000:TEST\nEnvironment={}=claude-opus-5\nEnvironment={}={}\n",
                crate::telegram::BOT_TOKEN_ENV,
                crate::telegram::MODEL_ENV,
                crate::telegram::WORKSPACE_ENV,
                workspace.display()
            ),
        )
        .unwrap();

        assert!(
            install_problems(&spec, &spec.render(), &unit_path).is_empty(),
            "a unit a drop-in completes is installable; refusing it would break \
             the ordinary way an operator supplies a secret"
        );
    }

    #[test]
    fn the_alternate_model_variable_counts() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let spec = spec(home, &home.join(".danso/telegram"));
        let unit_path = home.join("danso.service");
        let drop_ins = danso_ops::unit::drop_in_dir(&unit_path);
        std::fs::create_dir_all(&drop_ins).unwrap();
        std::fs::write(
            drop_ins.join("10-danso.conf"),
            format!(
                "[Service]\nEnvironment={}=000000:TEST\nEnvironment=DANSO_MODEL=claude-opus-5\nEnvironment={}={}\n",
                crate::telegram::BOT_TOKEN_ENV,
                crate::telegram::WORKSPACE_ENV,
                home.join("ws").display()
            ),
        )
        .unwrap();
        let problems = install_problems(&spec, &spec.render(), &unit_path);
        assert!(
            !problems.iter().any(|p| p.contains("MODEL")),
            "naming only the canonical variable would send an operator to \
             change one that was already fine: {problems:?}"
        );
    }

    #[test]
    fn a_workspace_that_contains_the_state_root_is_refused_however_it_was_set() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let spec = spec(home, &home.join("state"));
        let unit_path = home.join("danso.service");
        let drop_ins = danso_ops::unit::drop_in_dir(&unit_path);
        std::fs::create_dir_all(&drop_ins).unwrap();
        // Explicitly set, and still containing the state root.
        std::fs::write(
            drop_ins.join("10-danso.conf"),
            format!(
                "[Service]\nEnvironment={}=000000:TEST\nEnvironment={}=m\nEnvironment={}={}\n",
                crate::telegram::BOT_TOKEN_ENV,
                crate::telegram::MODEL_ENV,
                crate::telegram::WORKSPACE_ENV,
                home.display()
            ),
        )
        .unwrap();
        let problems = install_problems(&spec, &spec.render(), &unit_path);
        assert!(
            problems.iter().any(|p| p.contains("inside the workspace")),
            "{problems:?}"
        );
    }

    #[test]
    fn no_problem_names_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let spec = spec(home, &home.join(".danso/telegram"));
        let unit_path = home.join("danso.service");
        let drop_ins = danso_ops::unit::drop_in_dir(&unit_path);
        std::fs::create_dir_all(&drop_ins).unwrap();
        std::fs::write(
            drop_ins.join("10-danso.conf"),
            format!(
                "[Service]\nEnvironment={}=SECRET-TOKEN-VALUE\n",
                crate::telegram::BOT_TOKEN_ENV
            ),
        )
        .unwrap();
        for problem in install_problems(&spec, &spec.render(), &unit_path) {
            assert!(
                !problem.contains("SECRET-TOKEN-VALUE"),
                "the report names variables, never their values: {problem}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn service_and_telegram_processes_are_attributed() {
        assert!(is_service_argv(&argv(&[
            "/usr/local/bin/danso",
            "service",
            "run"
        ])));
        assert!(is_service_argv(&argv(&["danso", "telegram"])));
    }

    #[test]
    fn an_unrelated_process_is_not_attributed() {
        assert!(!is_service_argv(&argv(&["cat", "service"])));
        assert!(!is_service_argv(&argv(&["/bin/tail", "-f", "danso"])));
        assert!(!is_service_argv(&argv(&["danso"])));
        assert!(!is_service_argv(&argv(&[])));
    }

    #[test]
    fn a_status_inspection_is_not_a_holder() {
        // `status` opens the lock file to probe it. Attributing that read as
        // the holder would make every inspection report a running service.
        assert!(!is_service_argv(&argv(&["danso", "service", "status"])));
        assert!(!is_service_argv(&argv(&[
            "danso", "service", "status", "--json"
        ])));
    }

    #[test]
    fn a_similarly_named_program_is_not_attributed() {
        assert!(!is_service_argv(&argv(&["dansomething", "service"])));
        assert!(is_service_argv(&argv(&["danso-test", "service"])));
    }
}
