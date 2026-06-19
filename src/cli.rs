//! Command-line interface (clap).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "replaykit",
    version,
    about = "A deterministic record-and-replay proxy for AI agents.",
    long_about = "replaykit records all traffic between an AI agent and the outside world \
(LLM APIs + tool APIs), then replays those exact responses offline so any agent run is \
perfectly reproducible and debuggable.",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create and trust a local CA so HTTPS traffic can be intercepted (one time).
    Setup(SetupArgs),
    /// Record agent traffic to a cassette by forwarding to the real upstream.
    Record(RecordArgs),
    /// Replay a cassette offline, returning the recorded responses.
    Replay(ReplayArgs),
    /// List the interactions in a cassette with sizes and totals.
    Inspect(InspectArgs),
    /// Show one recorded interaction (request + response) in full.
    Diff(DiffArgs),
    /// Serve the local web dashboard for a cassette.
    Dashboard(DashboardArgs),
}

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Directory to store the CA cert/key (default: ~/.replaykit/ca).
    #[arg(long)]
    pub ca_dir: Option<PathBuf>,
    /// Regenerate the CA even if one already exists.
    #[arg(long)]
    pub force: bool,
    /// Generate the CA but skip installing it into the OS trust store.
    #[arg(long)]
    pub no_trust: bool,
}

/// Shared matching knobs.
#[derive(Args, Debug, Clone)]
pub struct MatchArgs {
    /// Lowest acceptable match tier: exact | normalized | structural | similarity.
    #[arg(long, default_value = "structural")]
    pub min_tier: String,
    /// Enable the optional prompt-similarity tier.
    #[arg(long)]
    pub similarity: bool,
    /// Similarity threshold in [0,1] when --similarity is set.
    #[arg(long, default_value_t = 0.85)]
    pub similarity_threshold: f64,
    /// Extra header names (lower-case) to treat as volatile (repeatable).
    #[arg(long = "volatile-header")]
    pub volatile_headers: Vec<String>,
    /// Extra JSON field names to treat as volatile (repeatable).
    #[arg(long = "volatile-field")]
    pub volatile_fields: Vec<String>,
}

#[derive(Args, Debug)]
pub struct RecordArgs {
    /// Provider preset: openai | anthropic | google | ollama | vllm | lmstudio | custom.
    #[arg(long)]
    pub preset: Option<String>,
    /// Explicit upstream base URL (required for `--preset custom`).
    #[arg(long)]
    pub upstream: Option<String>,
    /// Directory to write the cassette to.
    #[arg(long)]
    pub out: PathBuf,
    /// Port to listen on (0 = pick a free port).
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// Address to bind (default 127.0.0.1).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// CA directory (default: ~/.replaykit/ca).
    #[arg(long)]
    pub ca_dir: Option<PathBuf>,
    #[command(flatten)]
    pub matching: MatchArgs,
}

#[derive(Args, Debug)]
pub struct ReplayArgs {
    /// Cassette directory to replay.
    #[arg(long)]
    pub run: PathBuf,
    /// Divergence policy: fail-fast | warn-and-passthrough-to-live | warn-and-return-closest.
    #[arg(long, default_value = "fail-fast")]
    pub on_divergence: String,
    /// Port to listen on (0 = pick a free port).
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// Address to bind (default 127.0.0.1).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Replay SSE chunks with their original inter-chunk delays.
    #[arg(long)]
    pub preserve_timing: bool,
    /// Provider preset (only needed for passthrough divergence policy).
    #[arg(long)]
    pub preset: Option<String>,
    /// Upstream base URL (only needed for passthrough divergence policy).
    #[arg(long)]
    pub upstream: Option<String>,
    /// CA directory (default: ~/.replaykit/ca).
    #[arg(long)]
    pub ca_dir: Option<PathBuf>,
    #[command(flatten)]
    pub matching: MatchArgs,
}

#[derive(Args, Debug)]
pub struct InspectArgs {
    /// Cassette directory to inspect.
    pub run: PathBuf,
    /// Output as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Cassette directory.
    pub run: PathBuf,
    /// Step (interaction index) to show.
    #[arg(long)]
    pub step: usize,
}

#[derive(Args, Debug)]
pub struct DashboardArgs {
    /// Cassette directory to view.
    #[arg(long)]
    pub run: PathBuf,
    /// Port to serve the dashboard on.
    #[arg(long, default_value_t = 7777)]
    pub port: u16,
    /// Address to bind (default 127.0.0.1).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Do not try to open a browser automatically.
    #[arg(long)]
    pub no_open: bool,
}
