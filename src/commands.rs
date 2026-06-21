//! Implementation of each CLI subcommand.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use owo_colors::OwoColorize;

use crate::ca::{LocalCa, TrustOutcome};
use crate::cassette::{CassetteReader, CassetteWriter, Interaction};
use crate::cli::{
    DashboardArgs, DiffArgs, InspectArgs, MatchArgs, RecordArgs, ReplayArgs, RunArgs, SetupArgs,
};
use crate::config::{default_ca_dir, Preset, Upstream};
use crate::divergence::DivergencePolicy;
use crate::matcher::{MatchConfig, Tier};
use crate::proxy::{self, record::RecordEngine, replay::ReplayEngine, Engine, ProxyState};
use crate::{dashboard, util};

/// `replaykit setup`
pub async fn setup(args: SetupArgs) -> Result<i32> {
    let dir = args.ca_dir.unwrap_or_else(default_ca_dir);
    let exists = dir.join("ca-cert.pem").exists();
    if exists && !args.force {
        println!("{} CA already exists at {}", "✓".green(), dir.display());
        println!("  (use {} to regenerate)", "--force".yellow());
    } else {
        LocalCa::generate(&dir).context("generating CA")?;
        println!("{} generated local CA at {}", "✓".green(), dir.display());
    }
    let ca = LocalCa::load(&dir)?;

    if args.no_trust {
        println!("{} skipping trust install (--no-trust)", "•".dimmed());
        print_trust_hint(&ca);
        return Ok(0);
    }

    match ca.install_trust()? {
        TrustOutcome::Installed => {
            println!("{} CA installed into the OS trust store", "✓".green());
        }
        TrustOutcome::Manual { instructions } => {
            println!("{} could not install trust automatically.", "!".yellow());
            println!("{instructions}");
        }
    }
    print_trust_hint(&ca);
    Ok(0)
}

fn print_trust_hint(ca: &LocalCa) {
    println!();
    println!("CA certificate: {}", ca.cert_path().display());
    println!(
        "For tools that use their own CA bundle (Python requests/httpx, Node), point them at it:"
    );
    println!(
        "  {}=\"{}\"",
        "REQUESTS_CA_BUNDLE".cyan(),
        ca.cert_path().display()
    );
    println!(
        "  {}=\"{}\"",
        "SSL_CERT_FILE".cyan(),
        ca.cert_path().display()
    );
    println!(
        "  {}=\"{}\"",
        "NODE_EXTRA_CA_CERTS".cyan(),
        ca.cert_path().display()
    );
}

/// `replaykit record`
pub async fn record(args: RecordArgs) -> Result<i32> {
    let (preset, upstream) = resolve_upstream(args.preset.as_deref(), args.upstream.as_deref())?;
    let match_config = build_match_config(&args.matching)?;
    let ca_dir = args.ca_dir.unwrap_or_else(default_ca_dir);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .context("invalid host/port")?;

    let run_id = args
        .out
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("run")
        .to_string();
    let upstream_str = upstream
        .as_ref()
        .map(|u| format!("{}://{}:{}", u.scheme, u.host, u.port));
    let writer = Arc::new(CassetteWriter::create(
        &args.out,
        run_id,
        util::now_rfc3339(),
        upstream_str,
    )?);
    // Persist the manifest header (incl. the upstream) up front so a cassette is
    // replayable even if recording is hard-killed before a clean shutdown.
    writer.finalize()?;

    let ca = load_ca_optional(&ca_dir, preset);
    let client_tls = crate::ca::upstream_client_config();
    let engine = Arc::new(RecordEngine::new(
        writer.clone(),
        client_tls,
        match_config.clone(),
    ));
    let state = Arc::new(ProxyState {
        engine: Engine::Record(engine),
        ca: ca.clone(),
        default_upstream: upstream.clone(),
    });

    let out_display = args.out.display().to_string();
    let writer_for_finalize = writer.clone();
    tokio::select! {
        r = proxy::serve(addr, state, |local| {
            print_banner("RECORD", local, preset, upstream.as_ref(), ca.is_some(), &match_config);
            println!("  {} {}", "cassette →".dimmed(), out_display.cyan());
            println!("\n  Recording. Run your agent, then press {} to stop.\n", "Ctrl-C".bold());
        }) => { r?; }
        _ = tokio::signal::ctrl_c() => {
            println!("\n{} finalising cassette…", "•".dimmed());
        }
    }

    let manifest = writer_for_finalize.finalize()?;
    println!(
        "{} recorded {} interaction(s) → {}",
        "✓".green(),
        manifest.interaction_count.bold(),
        args.out.display()
    );
    print_storage_summary(manifest.total_logical_bytes, manifest.total_blob_bytes);
    Ok(0)
}

/// `replaykit replay`
pub async fn replay(args: ReplayArgs) -> Result<i32> {
    let policy = DivergencePolicy::parse(&args.on_divergence)
        .with_context(|| format!("unknown --on-divergence policy: {}", args.on_divergence))?;
    let match_config = build_match_config(&args.matching)?;
    let ca_dir = args.ca_dir.unwrap_or_else(default_ca_dir);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .context("invalid host/port")?;

    let reader = Arc::new(CassetteReader::open(&args.run)?);
    let n = reader.interactions().len();

    // Upstream/CA only matter for the passthrough policy, but we still load the
    // CA so the agent can keep talking to us over HTTPS_PROXY during replay, and
    // the upstream host is needed to reconstruct origin-form request identity.
    // If not given, fall back to the upstream recorded in the manifest.
    let (preset, upstream) = match (args.preset.as_deref(), args.upstream.as_deref()) {
        (None, None) => match reader.manifest().default_upstream.as_deref() {
            Some(u) => (Preset::Custom, Some(Upstream::parse(u)?)),
            None => (Preset::Custom, None),
        },
        (p, u) => resolve_upstream(p, u)?,
    };
    let ca = load_ca_optional(&ca_dir, preset);
    let allow_live = matches!(policy, DivergencePolicy::PassthroughLive);
    let client_tls = if allow_live {
        Some(crate::ca::upstream_client_config())
    } else {
        None
    };

    let engine = Arc::new(ReplayEngine::new(
        reader.clone(),
        policy,
        match_config.clone(),
        args.preserve_timing,
        client_tls,
        upstream.clone(),
    ));
    let state = Arc::new(ProxyState {
        engine: Engine::Replay(engine.clone()),
        ca: ca.clone(),
        default_upstream: upstream.clone(),
    });

    let run_display = args.run.display().to_string();
    tokio::select! {
        r = proxy::serve(addr, state, |local| {
            print_banner("REPLAY (offline)", local, preset, upstream.as_ref(), ca.is_some(), &match_config);
            println!("  {} {}  ({} interactions)", "cassette ←".dimmed(), run_display.cyan(), n);
            println!("  {} {}", "on divergence:".dimmed(), args.on_divergence.yellow());
            println!("\n  Replaying. You can disconnect the network. Press {} to stop.\n", "Ctrl-C".bold());
        }) => { r?; }
        _ = tokio::signal::ctrl_c() => {
            println!("\n{} writing replay report…", "•".dimmed());
        }
    }

    engine.write_report();
    let divs = engine.divergences();
    if divs.is_empty() {
        println!("{} replay finished with no divergences", "✓".green());
        Ok(0)
    } else {
        println!(
            "{} replay finished with {} divergence(s):",
            "✗".red(),
            divs.len().bold()
        );
        for d in &divs {
            println!("  {} {}", "•".red(), d.summary);
        }
        println!(
            "\n  See {} or run {}",
            args.run
                .join("last-replay.json")
                .display()
                .to_string()
                .dimmed(),
            format!("replaykit dashboard --run {}", args.run.display()).cyan()
        );
        Ok(if engine.failed() { 1 } else { 0 })
    }
}

/// `replaykit inspect`
pub async fn inspect(args: InspectArgs) -> Result<i32> {
    let reader = CassetteReader::open(&args.run)?;
    let manifest = reader.manifest();

    if args.json {
        #[derive(serde::Serialize)]
        struct Out<'a> {
            manifest: &'a crate::cassette::Manifest,
            interactions: &'a [Interaction],
        }
        let out = Out {
            manifest,
            interactions: reader.interactions(),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    println!("{}", format!("Cassette: {}", args.run.display()).bold());
    println!("  run id        {}", manifest.run_id);
    println!("  recorded      {}", manifest.created_utc);
    println!("  tool version  {}", manifest.tool_version);
    println!("  format        v{}", manifest.format_version);
    println!("  providers     {}", manifest.providers.join(", "));
    println!();
    println!(
        "  {:<5} {:<6} {:<40} {:>6} {:>9} {:>9} {:>6}",
        "step".bold(),
        "method".bold(),
        "endpoint".bold(),
        "status".bold(),
        "req".bold(),
        "resp".bold(),
        "stream".bold()
    );
    for i in reader.interactions() {
        let endpoint = format!("{}{}", i.request.host, i.request.path);
        let endpoint = truncate(&endpoint, 40);
        println!(
            "  {:<5} {:<6} {:<40} {:>6} {:>9} {:>9} {:>6}",
            i.step,
            i.request.method,
            endpoint,
            i.response.status,
            util::human_bytes(i.request.body_len),
            util::human_bytes(i.response.body_len),
            if i.response.stream { "yes" } else { "" }
        );
    }
    println!();
    println!(
        "  {} {}",
        "interactions".dimmed(),
        manifest.interaction_count.bold()
    );
    print_storage_summary(manifest.total_logical_bytes, manifest.total_blob_bytes);

    // Show divergences if a replay report exists.
    if let Ok(s) = std::fs::read_to_string(args.run.join("last-replay.json")) {
        if let Ok(report) = serde_json::from_str::<serde_json::Value>(&s) {
            if let Some(divs) = report.get("divergences").and_then(|d| d.as_array()) {
                if !divs.is_empty() {
                    println!(
                        "\n  {} {} divergence(s) from last replay:",
                        "✗".red(),
                        divs.len()
                    );
                    for d in divs {
                        if let Some(sum) = d.get("summary").and_then(|s| s.as_str()) {
                            println!("    {} {sum}", "•".red());
                        }
                    }
                }
            }
        }
    }
    Ok(0)
}

/// `replaykit diff --step N`
pub async fn diff(args: DiffArgs) -> Result<i32> {
    let reader = CassetteReader::open(&args.run)?;
    let interaction = reader
        .interactions()
        .iter()
        .find(|i| i.step == args.step)
        .with_context(|| format!("no interaction with step {}", args.step))?;

    println!(
        "{}",
        format!("── Step {} ─────────────────────────────", interaction.step).bold()
    );
    println!("{}", "REQUEST".cyan().bold());
    println!(
        "  {} {}",
        interaction.request.method.bold(),
        interaction.request.url
    );
    for h in &interaction.request.headers {
        println!("  {}: {}", h.name.dimmed(), redact(&h.name, &h.value));
    }
    let req_body = reader.request_body(interaction)?;
    if !req_body.is_empty() {
        println!("\n{}", indent(&util::pretty_json_or_text(&req_body)));
    }

    println!("\n{}", "RESPONSE".green().bold());
    println!("  status {}", interaction.response.status.bold());
    for h in &interaction.response.headers {
        println!("  {}: {}", h.name.dimmed(), h.value);
    }
    let resp_body = reader.response_body(interaction)?;
    if !resp_body.is_empty() {
        println!("\n{}", indent(&util::pretty_json_or_text(&resp_body)));
    }
    println!("\n{}", "MATCH KEYS".magenta().bold());
    println!("  endpoint    {}", interaction.keys.endpoint);
    println!(
        "  exact       {}",
        &interaction.keys.exact[..16.min(interaction.keys.exact.len())]
    );
    println!(
        "  normalized  {}",
        &interaction.keys.normalized[..16.min(interaction.keys.normalized.len())]
    );
    println!(
        "  structural  {}",
        &interaction.keys.structural[..16.min(interaction.keys.structural.len())]
    );
    Ok(0)
}

/// `replaykit dashboard`
pub async fn dashboard(args: DashboardArgs) -> Result<i32> {
    let reader = Arc::new(CassetteReader::open(&args.run)?);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .context("invalid host/port")?;
    dashboard::serve(addr, reader, !args.no_open).await?;
    Ok(0)
}

// ----- helpers ------------------------------------------------------------

fn resolve_upstream(
    preset: Option<&str>,
    upstream: Option<&str>,
) -> Result<(Preset, Option<Upstream>)> {
    let preset = match preset {
        Some(p) => Preset::parse(p).with_context(|| format!("unknown preset: {p}"))?,
        None => {
            if upstream.is_some() {
                Preset::Custom
            } else {
                // No preset and no upstream: forward-proxy-only (HTTPS_PROXY) mode.
                return Ok((Preset::Custom, None));
            }
        }
    };
    let up = match upstream {
        Some(u) => Some(Upstream::parse(u)?),
        None => match preset.default_upstream() {
            Some(u) => Some(Upstream::parse(u)?),
            None => bail!("preset `custom` requires --upstream <URL>"),
        },
    };
    Ok((preset, up))
}

fn build_match_config(m: &MatchArgs) -> Result<MatchConfig> {
    let min_tier =
        Tier::parse(&m.min_tier).with_context(|| format!("unknown --min-tier: {}", m.min_tier))?;
    let mut cfg = MatchConfig {
        min_tier,
        enable_similarity: m.similarity,
        similarity_threshold: m.similarity_threshold,
        ..MatchConfig::default()
    };
    for h in &m.volatile_headers {
        cfg.volatile_headers.push(h.to_lowercase());
    }
    for f in &m.volatile_fields {
        cfg.volatile_json_fields.push(f.to_lowercase());
    }
    Ok(cfg)
}

/// Load the CA if it exists. Local presets never need it; cloud presets use it
/// for HTTPS interception (a warning is printed if it is missing).
fn load_ca_optional(ca_dir: &std::path::Path, preset: Preset) -> Option<Arc<LocalCa>> {
    if preset.is_local() {
        return None;
    }
    match LocalCa::load(ca_dir) {
        Ok(ca) => Some(Arc::new(ca)),
        Err(_) => None,
    }
}

fn print_banner(
    mode: &str,
    local: SocketAddr,
    preset: Preset,
    upstream: Option<&Upstream>,
    has_ca: bool,
    cfg: &MatchConfig,
) {
    println!();
    println!(
        "  {}  {}",
        "replaykit".bold().on_bright_black(),
        mode.bold()
    );
    println!("  {}", "─".repeat(52).dimmed());
    println!(
        "  {} http://{}",
        "proxy   ".dimmed(),
        local.to_string().cyan()
    );
    if let Some(u) = upstream {
        println!(
            "  {} {}://{}:{}",
            "upstream".dimmed(),
            u.scheme,
            u.host,
            u.port
        );
    }
    println!("  {} {}", "preset  ".dimmed(), preset.name());
    println!(
        "  {} {}",
        "tls     ".dimmed(),
        if has_ca {
            "CA loaded — HTTPS interception ON".green().to_string()
        } else {
            "no CA — HTTPS interception OFF (reverse-proxy/HTTP only)"
                .yellow()
                .to_string()
        }
    );
    println!(
        "  {} min-tier={}",
        "match   ".dimmed(),
        cfg.min_tier.label()
    );
    println!("  {}", "─".repeat(52).dimmed());
    println!("\n  {} point your agent at the proxy:", "→".bold());
    println!("    {}=http://{}", "HTTPS_PROXY".cyan(), local);
    println!("    {}=http://{}", "HTTP_PROXY ".cyan(), local);
    if upstream.is_some() {
        println!(
            "    {} set the SDK base_url to  http://{}",
            "or".dimmed(),
            local
        );
    }
}

fn print_storage_summary(logical: u64, on_disk: u64) {
    let ratio = if on_disk > 0 {
        logical as f64 / on_disk as f64
    } else {
        1.0
    };
    println!(
        "  {} {} logical → {} on disk  ({}× smaller)",
        "storage".dimmed(),
        util::human_bytes(logical),
        util::human_bytes(on_disk),
        format!("{ratio:.1}").green()
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

fn redact(name: &str, value: &str) -> String {
    let lname = name.to_lowercase();
    if lname == "authorization" || lname == "x-api-key" || lname == "api-key" || lname == "cookie" {
        "<redacted>".to_string()
    } else {
        value.to_string()
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `replaykit run -- <cmd> [args...]`
///
/// One-shot wrapper: spawns the proxy, runs the child command with the proxy
/// wired into common base-URL and proxy env vars, waits for the child, then
/// shuts the proxy down cleanly. Picks record vs replay automatically based
/// on whether the cassette already has interactions.
pub async fn run(args: RunArgs) -> Result<i32> {
    if args.record && args.replay {
        bail!("--record and --replay are mutually exclusive");
    }

    let cassette_existed = cassette_has_interactions(&args.cassette);
    let mode = if args.record {
        RunMode::Record
    } else if args.replay {
        if !cassette_existed {
            bail!(
                "--replay requested but cassette `{}` has no interactions yet",
                args.cassette.display()
            );
        }
        RunMode::Replay
    } else if cassette_existed {
        RunMode::Replay
    } else {
        RunMode::Record
    };

    let match_config = build_match_config(&args.matching)?;
    let ca_dir = args.ca_dir.clone().unwrap_or_else(default_ca_dir);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .context("invalid host/port")?;

    let (state, mode_label, on_finish): (Arc<ProxyState>, &'static str, FinishHook) = match mode {
        RunMode::Record => build_record_state(&args, &ca_dir, &match_config)?,
        RunMode::Replay => build_replay_state(&args, &ca_dir, &match_config)?,
    };

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding proxy listener on {addr}"))?;
    let local = listener.local_addr()?;
    println!(
        "  {} {}  {}  {}",
        "replaykit run".bold(),
        mode_label.cyan().bold(),
        "·".dimmed(),
        format!("proxy http://{local}").dimmed()
    );
    println!("  {} {}", "cassette".dimmed(), args.cassette.display());
    println!("  {} {}", "command ".dimmed(), args.cmd.join(" ").dimmed());
    println!();

    let serve_state = state.clone();
    let serve_task = tokio::spawn(async move { serve_on_listener(listener, serve_state).await });

    let proxy_url = format!("http://{local}");
    let mut cmd = tokio::process::Command::new(&args.cmd[0]);
    cmd.args(&args.cmd[1..]);
    cmd.env("HTTP_PROXY", &proxy_url);
    cmd.env("HTTPS_PROXY", &proxy_url);
    cmd.env("http_proxy", &proxy_url);
    cmd.env("https_proxy", &proxy_url);
    cmd.env("REPLAYKIT_PROXY", &proxy_url);
    cmd.env("OPENAI_BASE_URL", format!("{proxy_url}/v1"));
    cmd.env("ANTHROPIC_BASE_URL", &proxy_url);
    cmd.env("GEMINI_PROXY", &proxy_url);
    cmd.env("GOOGLE_GENAI_BASE_URL", &proxy_url);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning `{}`", args.cmd[0]))?;
    // A foreground Ctrl-C is delivered to the whole console/process group, so
    // the child receives it directly and exits; we just wait for it. (No
    // separate signal branch — that would need a second &mut borrow of child.)
    let exit = child.wait().await.context("waiting on child")?;

    serve_task.abort();
    let _ = on_finish();

    let code = exit.code().unwrap_or(1);
    println!(
        "  {} child exited with status {}",
        "•".dimmed(),
        code.to_string().bold()
    );
    Ok(code)
}

type FinishHook = Box<dyn FnOnce() -> i32 + Send>;

enum RunMode {
    Record,
    Replay,
}

fn cassette_has_interactions(dir: &std::path::Path) -> bool {
    match std::fs::metadata(dir.join("interactions.jsonl")) {
        Ok(m) => m.len() > 0,
        Err(_) => false,
    }
}

fn build_record_state(
    args: &RunArgs,
    ca_dir: &std::path::Path,
    match_config: &MatchConfig,
) -> Result<(Arc<ProxyState>, &'static str, FinishHook)> {
    let (preset, upstream) = resolve_upstream(args.preset.as_deref(), args.upstream.as_deref())?;
    if args.cassette.exists() {
        std::fs::remove_dir_all(&args.cassette)
            .with_context(|| format!("wiping prior cassette at {}", args.cassette.display()))?;
    }
    let run_id = args
        .cassette
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("run")
        .to_string();
    let upstream_str = upstream
        .as_ref()
        .map(|u| format!("{}://{}:{}", u.scheme, u.host, u.port));
    let writer = Arc::new(CassetteWriter::create(
        &args.cassette,
        run_id,
        util::now_rfc3339(),
        upstream_str,
    )?);
    writer.finalize()?;
    let ca = load_ca_optional(ca_dir, preset);
    let client_tls = crate::ca::upstream_client_config();
    let engine = Arc::new(RecordEngine::new(
        writer.clone(),
        client_tls,
        match_config.clone(),
    ));
    let state = Arc::new(ProxyState {
        engine: Engine::Record(engine),
        ca,
        default_upstream: upstream,
    });
    let writer_finalize = writer.clone();
    let cassette_for_msg = args.cassette.clone();
    let hook: FinishHook = Box::new(move || {
        match writer_finalize.finalize() {
            Ok(manifest) => {
                println!(
                    "{} recorded {} interaction(s) → {}",
                    "✓".green(),
                    manifest.interaction_count.bold(),
                    cassette_for_msg.display()
                );
                print_storage_summary(manifest.total_logical_bytes, manifest.total_blob_bytes);
            }
            Err(e) => eprintln!("warning: finalising cassette: {e:#}"),
        }
        0
    });
    Ok((state, "RECORD", hook))
}

fn build_replay_state(
    args: &RunArgs,
    ca_dir: &std::path::Path,
    match_config: &MatchConfig,
) -> Result<(Arc<ProxyState>, &'static str, FinishHook)> {
    let policy = DivergencePolicy::parse(&args.on_divergence)
        .with_context(|| format!("unknown --on-divergence: {}", args.on_divergence))?;
    let reader = Arc::new(CassetteReader::open(&args.cassette)?);
    let (preset, upstream) = match (args.preset.as_deref(), args.upstream.as_deref()) {
        (None, None) => match reader.manifest().default_upstream.as_deref() {
            Some(u) => (Preset::Custom, Some(Upstream::parse(u)?)),
            None => (Preset::Custom, None),
        },
        (p, u) => resolve_upstream(p, u)?,
    };
    let ca = load_ca_optional(ca_dir, preset);
    let allow_live = matches!(policy, DivergencePolicy::PassthroughLive);
    let client_tls = if allow_live {
        Some(crate::ca::upstream_client_config())
    } else {
        None
    };
    let engine = Arc::new(ReplayEngine::new(
        reader.clone(),
        policy,
        match_config.clone(),
        false,
        client_tls,
        upstream.clone(),
    ));
    let state = Arc::new(ProxyState {
        engine: Engine::Replay(engine.clone()),
        ca,
        default_upstream: upstream,
    });
    let cassette_for_msg = args.cassette.clone();
    let hook: FinishHook = Box::new(move || {
        engine.write_report();
        let divs = engine.divergences();
        if divs.is_empty() {
            println!("{} replay finished with no divergences", "✓".green());
            0
        } else {
            println!(
                "{} replay finished with {} divergence(s):",
                "✗".red(),
                divs.len().bold()
            );
            for d in &divs {
                println!("  {} {}", "•".red(), d.summary);
            }
            println!(
                "  see {}",
                cassette_for_msg
                    .join("last-replay.json")
                    .display()
                    .to_string()
                    .dimmed()
            );
            if engine.failed() {
                1
            } else {
                0
            }
        }
    });
    Ok((state, "REPLAY (offline)", hook))
}

/// Variant of `proxy::serve` that uses an already-bound listener so the caller
/// knows the local port before the accept loop starts.
async fn serve_on_listener(
    listener: tokio::net::TcpListener,
    state: Arc<ProxyState>,
) -> Result<()> {
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tracing::{debug, warn};

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!("accept failed: {e}");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: hyper::Request<Incoming>| {
                let state = state.clone();
                async move {
                    Ok::<_, std::convert::Infallible>(
                        crate::proxy::outer_dispatch_pub(req, state).await,
                    )
                }
            });
            if let Err(e) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                debug!("connection from {peer} ended: {e}");
            }
        });
    }
}
