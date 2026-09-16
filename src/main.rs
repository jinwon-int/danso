mod cli;
mod memory_cli;
use clap::Parser;
use cli::Args;
use danso::{
    app,
    failure::{self, Kind},
    output::{PrintSink, ProgressSink, report_budget, report_timing, report_usage},
    tools,
    usage::Usage,
};
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

async fn interrupted(reason: &AtomicU8) -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
    let code =
        tokio::select! { _ = int.recv() => 130, _ = term.recv() => 143, _ = hup.recv() => 129 };
    reason.store(if code == 130 { 1 } else { 2 }, Ordering::Release);
    code
}

// The tool worker must not initialize Tokio: RLIMIT_AS intentionally leaves
// room for a shell, not a multithreaded async runtime with many thread stacks.
fn main() {
    // Body-free timing receipt (issue #98 e): startup is measured from
    // process start until the run begins.
    let process_started = std::time::Instant::now();
    // The doctor is a synchronous, read-only state projection.  Keep it ahead
    // of service, memory, config and async runtime setup so inspection cannot
    // acquire a service lock, create state, call a provider or touch a network.
    if std::env::args().nth(1).as_deref() == Some("doctor") {
        let _args = match danso::doctor::DoctorArgs::try_parse_from(std::env::args_os().skip(1)) {
            Ok(args) => args,
            Err(_) => {
                eprintln!("doctor failed: invalid arguments");
                std::process::exit(2);
            }
        };
        match danso::doctor::run() {
            Ok(report) => {
                let code = report.exit_code();
                println!("{}", serde_json::to_string(&report).expect("serializable"));
                std::process::exit(code);
            }
            Err(_) => {
                // Keep the only non-report path body-free.  In particular, do
                // not echo HOME/DANSO_HOME or any filesystem error text.
                eprintln!("doctor failed: state home unavailable");
                std::process::exit(2);
            }
        }
    }
    // `service` is the resident-service surface. `status` in particular must
    // stay ahead of config and async runtime setup for the same reason doctor
    // does: inspecting a service must not acquire its lock or create its state.
    #[cfg(feature = "ops")]
    if std::env::args().nth(1).as_deref() == Some("service") {
        use danso::service::{ServiceArgs, ServiceCommand};
        let args = match ServiceArgs::try_parse_from(std::env::args_os().skip(1)) {
            Ok(args) => args,
            Err(error) => {
                let code = error.exit_code();
                error.print().ok();
                std::process::exit(code);
            }
        };
        match args.command {
            ServiceCommand::Status { data_dir, json } => match danso::service::status(data_dir) {
                Ok(report) => {
                    if json {
                        println!("{}", serde_json::to_string(&report).expect("serializable"));
                    } else {
                        print!("{}", report.to_text());
                    }
                    std::process::exit(report.exit_code());
                }
                Err(_) => {
                    // Body-free: never echo HOME/DANSO_HOME or filesystem text.
                    // Exit 3 is "could not determine", never a false DOWN.
                    eprintln!("service status failed: state home unavailable");
                    std::process::exit(3);
                }
            },
            ServiceCommand::Run {
                data_dir,
                supervise,
            } => {
                if supervise {
                    match danso::service::supervise(data_dir) {
                        Ok(0) => return,
                        Ok(code) => std::process::exit(code),
                        Err(error) => {
                            eprintln!("service supervision failed: {error}");
                            std::process::exit(1);
                        }
                    }
                }
                if let Err(error) = danso::service::run(data_dir) {
                    eprintln!("service run failed: {error}");
                    std::process::exit(1);
                }
                return;
            }
            ServiceCommand::Install {
                data_dir,
                user,
                dry_run,
            } => {
                let spec = match danso::service::unit_spec(data_dir, user) {
                    Ok(spec) => spec,
                    Err(error) => {
                        eprintln!("service install failed: {error}");
                        std::process::exit(1);
                    }
                };
                if !danso::service::has_systemd() && !dry_run {
                    // Not an error: Termux is a supported target that has no
                    // systemd. Say what to do instead, install nothing.
                    println!("{}", danso::service::termux_guidance(&spec));
                    return;
                }
                match danso::service::install(&spec, dry_run) {
                    Ok(danso::service::InstallOutcome::DryRun { path, unit }) => {
                        // The unit goes to stdout by itself, byte for byte, so
                        // `--dry-run > danso.service` produces a usable file.
                        // The commentary belongs on stderr.
                        eprintln!("would write {}", path.display());
                        print!("{unit}");
                        return;
                    }
                    Ok(danso::service::InstallOutcome::Installed { path }) => {
                        println!("Unit: installed {}", path.display());
                        println!("Unit: enabled (not started — use `systemctl start`)");
                        return;
                    }
                    Err(error) => {
                        eprintln!("service install failed: {error}");
                        std::process::exit(1);
                    }
                }
            }
            ServiceCommand::Reconcile { data_dir, user } => {
                match danso::service::unit_spec(data_dir, user)
                    .and_then(|spec| danso::service::reconcile(&spec))
                {
                    Ok(drift) => {
                        println!("{}", drift.summary());
                        std::process::exit(drift.exit_code());
                    }
                    Err(error) => {
                        eprintln!("service reconcile failed: {error}");
                        std::process::exit(3);
                    }
                }
            }
            ServiceCommand::Uninstall { data_dir, user } => {
                match danso::service::unit_spec(data_dir, user)
                    .and_then(|spec| danso::service::uninstall(&spec))
                {
                    Ok(text) => {
                        println!("{text}");
                        return;
                    }
                    Err(error) => {
                        eprintln!("service uninstall failed: {error}");
                        std::process::exit(1);
                    }
                }
            }
            ServiceCommand::Stop {
                data_dir,
                grace_secs,
            } => {
                // The budget was validated during parsing, before any signal.
                match danso::service::stop(data_dir, std::time::Duration::from_secs(grace_secs)) {
                    Ok(outcome) => {
                        println!("{}", danso::service::stop_text(outcome));
                        std::process::exit(outcome.exit_code());
                    }
                    Err(_) => {
                        eprintln!("service stop failed: state home unavailable");
                        std::process::exit(3);
                    }
                }
            }
        }
    }
    // `update` is state inspection in this slice: read-only, offline, and no
    // lock. Keep it ahead of config and async runtime setup for the same reason
    // doctor is — inspecting an installation must not change it.
    #[cfg(feature = "ops")]
    if std::env::args().nth(1).as_deref() == Some("update") {
        use danso::update::{UpdateArgs, UpdateCommand};
        let args = match UpdateArgs::try_parse_from(std::env::args_os().skip(1)) {
            Ok(args) => args,
            Err(error) => {
                let code = error.exit_code();
                error.print().ok();
                std::process::exit(code);
            }
        };
        let UpdateCommand::Status { json } = args.command;
        match danso::update::status() {
            Ok(report) => {
                if json {
                    println!("{}", serde_json::to_string(&report).expect("serializable"));
                } else {
                    println!("{}", report.summary());
                }
                std::process::exit(report.exit_code());
            }
            Err(_) => {
                // Body-free: never echo HOME/DANSO_HOME or filesystem text.
                eprintln!("update status failed: state home unavailable");
                std::process::exit(2);
            }
        }
    }
    // Telegram is a service entry point, not a normal prompt positional. It
    // owns one process-wide token lock and keeps all turns in this process.
    if std::env::args().nth(1).as_deref() == Some("telegram") {
        if let Err(error) =
            danso::telegram::TelegramArgs::try_parse_from(std::env::args_os().skip(1))
        {
            let code = error.exit_code();
            error.print().ok();
            std::process::exit(code);
        }
        if let Err(_error) = danso::telegram::run() {
            eprintln!("telegram service failed");
            std::process::exit(1);
        }
        return;
    }
    // The memory subcommand is synchronous and provider-free; it short-circuits
    // before the async runtime is touched (issue #52 M1).
    if std::env::args().nth(1).as_deref() == Some("memory") {
        let args = match memory_cli::MemoryArgs::try_parse_from(std::env::args_os().skip(1)) {
            Ok(args) => args,
            Err(error) => {
                let code = error.exit_code();
                error.print().ok();
                std::process::exit(code);
            }
        };
        if let Err(error) = memory_cli::validate(&args) {
            eprintln!("memory configuration error: {error}");
            std::process::exit(2);
        }
        match memory_cli::run(&args) {
            Ok(Some(value)) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).expect("serializable")
                );
            }
            Ok(None) => {}
            Err(error) => {
                // Body-free refusal: memory commands never echo record bodies
                // through the error path.
                eprintln!("memory command refused: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    // Backup and restore are synchronous, provider-free state operations. They
    // must stay ahead of Tokio, service setup, and the normal agent route.
    if std::env::args().nth(1).as_deref() == Some("backup") {
        if let Err(error) = danso::backup::BackupArgs::try_parse_from(std::env::args_os().skip(1)) {
            let code = error.exit_code();
            error.print().ok();
            std::process::exit(code);
        }
        match danso::backup::run_backup() {
            Ok(path) => println!("{}", path.display()),
            Err(error) => {
                eprintln!("backup failed: {}", error.category());
                std::process::exit(error.exit_code());
            }
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("restore") {
        let args = match danso::backup::RestoreArgs::try_parse_from(std::env::args_os().skip(1)) {
            Ok(args) => args,
            Err(error) => {
                let code = error.exit_code();
                error.print().ok();
                std::process::exit(code);
            }
        };
        match danso::backup::run_restore(&args) {
            Ok(report) => println!(
                "{}",
                serde_json::to_string(&report).expect("restore report is serializable")
            ),
            Err(error) => {
                eprintln!("restore failed: {}", error.category());
                std::process::exit(error.exit_code());
            }
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("config") {
        #[derive(clap::Parser)]
        #[command(
            name = "danso config",
            about = "Validate $DANSO_HOME/config.toml without echoing values. No provider or network access."
        )]
        struct ConfigArgs {
            #[command(subcommand)]
            command: ConfigCommand,
        }
        #[derive(clap::Subcommand)]
        enum ConfigCommand {
            /// Parse and validate; print a body-free key report.
            Check {
                #[arg(long)]
                file: Option<std::path::PathBuf>,
            },
        }
        let args = ConfigArgs::parse_from(std::env::args_os().skip(1));
        let ConfigCommand::Check { file } = args.command;
        match danso::config::check(file.as_deref()) {
            Ok(report) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("serializable")
                );
            }
            Err(error) => {
                eprintln!("config check failed: {error:#}");
                std::process::exit(2);
            }
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("auth-adopt") {
        #[derive(clap::Parser)]
        #[command(
            name = "danso auth-adopt",
            about = "Transfer a quiescent isolated Codex auth.json to Danso-managed refresh. No network calls."
        )]
        struct Adopt {
            #[arg(long)]
            source: std::path::PathBuf,
        }
        let args = Adopt::parse_from(std::env::args_os().skip(1));
        match danso::provider::adopt_chatgpt_auth(&args.source) {
            Ok(path) => println!(
                "Adopted ChatGPT credentials; set DANSO_CHATGPT_AUTH_FILE={}",
                path.display()
            ),
            Err(_) => {
                eprintln!(
                    "ChatGPT auth adoption failed; inspect private recovery artifacts, do not retry blindly"
                );
                std::process::exit(2);
            }
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("__supervise") {
        let result = std::env::args()
            .nth(2)
            .and_then(|p| p.parse::<libc::pid_t>().ok())
            .filter(|p| *p > 0)
            .ok_or_else(|| anyhow::anyhow!("missing supervisor parent"))
            .and_then(tools::supervisor::run);
        match result {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
    if std::env::args().nth(1).as_deref() == Some("__tool") {
        let result = (|| {
            let mut input = String::new();
            std::io::stdin()
                .take(1024 * 1024)
                .read_to_string(&mut input)?;
            tools::worker(serde_json::from_str(&input)?)
        })();
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(e) => {
            let code = e.exit_code();
            e.print().ok();
            if code != 0 {
                failure::report(Kind::Configuration, code);
                report_usage(&Usage::default());
            }
            std::process::exit(code);
        }
    };
    if args.task_status {
        let path = if args.session.is_absolute() {
            args.session.clone()
        } else {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join(&args.session),
                Err(error) => {
                    eprintln!("task status refused: {error}");
                    std::process::exit(2);
                }
            }
        };
        match danso::session::Session::read_status(&path) {
            Ok(status) => {
                println!("{status}");
                return;
            }
            Err(error) => {
                eprintln!("task status refused: {error}");
                std::process::exit(2);
            }
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut usage = Usage::default();
    let pause_requested = Arc::new(AtomicBool::new(false));
    let cancellation_reason = Arc::new(AtomicU8::new(0));
    let mut config = args.config();
    config.cancellation_reason = Some(Arc::clone(&cancellation_reason));
    if config.long_task.is_some() {
        config.pause_requested = Some(Arc::clone(&pause_requested));
    }
    let mut sink = ProgressSink::new(PrintSink::new(args.output_mode()), args.progress_jsonl)
        .with_task_progress(args.task_progress)
        .with_request_progress(args.stream_requests);
    // Startup ends where the run begins (issue #98 e).
    usage.record_startup(
        process_started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    );
    let code = runtime.block_on(async {
        let pause_listener = if config.long_task.is_some() {
            use tokio::signal::unix::{SignalKind, signal};
            // Register the handler before the run's durable stage-0 handshake.
            let mut usr1 = signal(SignalKind::user_defined1()).expect("SIGUSR1 handler");
            let flag = Arc::clone(&pause_requested);
            Some(tokio::spawn(async move {
                usr1.recv().await;
                flag.store(true, Ordering::Release);
            }))
        } else {
            None
        };
        let code = tokio::select! {
            code = interrupted(&cancellation_reason) => { eprintln!("run interrupted"); failure::report(Kind::Interrupted, code); code },
            result = async {
                tokio::select! {
                    result = app::run(&config, &mut sink, &mut usage) => Ok(result),
                    _ = async {
                        tokio::time::sleep(Duration::from_secs(config.timeout_seconds)).await;
                        cancellation_reason.store(3, Ordering::Release);
                    } => Err(()),
                }
            } => {
                match result {
                    Ok(Ok(())) => 0,
                    Ok(Err(e)) => {
                        let code = match failure::category(&e) {
                            Some(Kind::RunTimeout) => 124,
                            Some(Kind::Interrupted) => 130,
                            _ if usage.attempted => 3,
                            _ => 2,
                        };
                        eprintln!("{e:#}");
                        failure::report(failure::category(&e).unwrap_or(Kind::Configuration), code);
                        if let Some(diagnostic) = failure::task_recovery(&e) {
                            failure::report_task_recovery(diagnostic);
                        }
                        if let Some(diagnostic) = failure::transport(&e) {
                            failure::report_transport(diagnostic);
                        }
                        if let Some(diagnostic) = failure::provider(&e) {
                            failure::report_provider(diagnostic);
                        }
                        if let Some(diagnostic) = e.chain().find_map(|cause| cause.downcast_ref::<danso::provider::http_diagnostic::HttpDiagnostic>()) {
                            eprintln!("DANSO_HTTP={}", serde_json::to_string(diagnostic).expect("fixed diagnostic"));
                        }
                        code
                    },
                    Err(_) => { eprintln!("run timed out"); failure::report(Kind::RunTimeout, 124); 124 },
                }
            }
        };
        if let Some(listener) = pause_listener {
            listener.abort();
        }
        code
    });
    report_usage(&usage);
    report_budget(&config, &usage);
    report_timing(&usage);
    std::process::exit(code);
}
