mod cli;
mod memory_cli;
use clap::Parser;
use cli::Args;
use danso::{
    app,
    failure::{self, Kind},
    output::{PrintSink, ProgressSink, report_budget, report_usage},
    tools,
    usage::Usage,
};
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

async fn interrupted() -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
    tokio::select! { _ = int.recv() => 130, _ = term.recv() => 143, _ = hup.recv() => 129 }
}

// The tool worker must not initialize Tokio: RLIMIT_AS intentionally leaves
// room for a shell, not a multithreaded async runtime with many thread stacks.
fn main() {
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
    let mut config = args.config();
    if config.long_task.is_some() {
        config.pause_requested = Some(Arc::clone(&pause_requested));
    }
    let mut sink = ProgressSink::new(PrintSink(args.output_mode()), args.progress_jsonl)
        .with_task_progress(args.task_progress);
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
            code = interrupted() => { eprintln!("run interrupted"); failure::report(Kind::Interrupted, code); code },
            result = tokio::time::timeout(Duration::from_secs(config.timeout_seconds), app::run(&config, &mut sink, &mut usage)) => {
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
                        if let Some(diagnostic) = failure::transport(&e) {
                            failure::report_transport(diagnostic);
                        }
                        if let Some(diagnostic) = failure::provider(&e) {
                            failure::report_provider(diagnostic);
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
    std::process::exit(code);
}
