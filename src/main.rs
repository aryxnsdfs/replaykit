//! replaykit — a deterministic record-and-replay proxy for AI agents.
//!
//! See the module docs in `proxy`, `cassette`, `matcher` and `divergence` for
//! the interesting parts. This file just wires up logging and dispatches the
//! CLI.

use clap::Parser;
use replaykit::cli::{Cli, Command};
use replaykit::commands;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    // rustls 0.23 requires a process-wide crypto provider to be installed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let result = match cli.command {
        Command::Setup(args) => commands::setup(args).await,
        Command::Record(args) => commands::record(args).await,
        Command::Replay(args) => commands::replay(args).await,
        Command::Inspect(args) => commands::inspect(args).await,
        Command::Diff(args) => commands::diff(args).await,
        Command::Dashboard(args) => commands::dashboard(args).await,
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("\x1b[31merror:\x1b[0m {e:#}");
            std::process::exit(2);
        }
    }
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "replaykit=info,warn",
        1 => "replaykit=debug,info",
        _ => "replaykit=trace,debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();
}
