mod cli;
use clap::Parser;
use cli::Args;
use danso::{
    app,
    output::{PrintSink, report_usage},
    tools,
    usage::Usage,
};
use std::{io::Read, time::Duration};

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
                report_usage(&Usage::default());
            }
            std::process::exit(code);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut usage = Usage::default();
    let config = args.config();
    let mut sink = PrintSink(args.output_mode());
    let code = runtime.block_on(async {
        tokio::select! {
            code = interrupted() => { eprintln!("run interrupted"); code },
            result = tokio::time::timeout(Duration::from_secs(args.timeout_seconds), app::run(&config, &mut sink, &mut usage)) => {
                match result {
                    Ok(Ok(())) => 0,
                    Ok(Err(e)) => { eprintln!("{e:#}"); if usage.attempted { 3 } else { 2 } },
                    Err(_) => { eprintln!("run timed out"); 124 },
                }
            }
        }
    });
    report_usage(&usage);
    std::process::exit(code);
}
