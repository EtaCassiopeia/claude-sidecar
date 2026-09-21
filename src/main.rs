use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::{
    fmt::{self, format::Writer, time::FormatTime},
    prelude::*,
    EnvFilter,
};

use claude_sidecar::{
    config as sidecar_config, events::EventBus, job::JobRegistry, metrics::MetricsRecorder, routes,
    AppState, Config,
};

#[derive(Debug, Parser)]
#[command(name = "claude-sidecar", version)]
struct Cli {
    /// Port to listen on.
    #[arg(short, long, default_value_t = 8765, env = "SIDECAR_PORT")]
    port: u16,

    /// Enable verbose per-line logging to stderr.
    #[arg(short, long, env = "SIDECAR_VERBOSE")]
    verbose: bool,

    /// Maximum concurrent jobs before returning 503.
    #[arg(long, default_value_t = 100, env = "SIDECAR_MAX_JOBS")]
    max_jobs: usize,

    /// Maximum output lines retained in memory per job. Older lines are evicted
    /// once this is exceeded, bounding memory for very chatty builds and tests.
    #[arg(long, default_value_t = 50_000, env = "SIDECAR_MAX_LINES")]
    max_lines: usize,

    /// Spill lines beyond --max-lines to a per-job temp file instead of dropping
    /// them, keeping the full log retrievable. Off by default (memory-only).
    #[arg(long, env = "SIDECAR_SPILL")]
    spill: bool,

    /// Seconds after job completion before the job record is evicted.
    #[arg(long, default_value_t = 600, env = "SIDECAR_JOB_TTL")]
    job_ttl: u64,

    /// Seconds a SIGTERM'd job gets to shut down cleanly before SIGKILL. Set 0
    /// to skip straight to SIGKILL.
    #[arg(long, default_value_t = 5, env = "SIDECAR_KILL_GRACE")]
    kill_grace: u64,

    /// Path to the policy file (default:
    /// `~/.config/claude-sidecar/config.toml`). A missing file means the
    /// built-in defaults; a malformed one is a startup error.
    #[arg(long, env = "SIDECAR_CONFIG")]
    config: Option<PathBuf>,

    /// Directory for the durable call metrics (default:
    /// `~/.config/claude-sidecar/metrics`).
    #[arg(long, env = "SIDECAR_METRICS_DIR")]
    metrics_dir: Option<PathBuf>,

    /// Do not record call metrics to disk.
    #[arg(long, env = "SIDECAR_NO_METRICS")]
    no_metrics: bool,

    /// Leave the launching terminal: run in a new session with no controlling
    /// tty. Pass this whenever the sidecar is started in the background.
    #[arg(long, env = "SIDECAR_DETACH")]
    detach: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Run SQL against the recorded call metrics and print the result.
    ///
    /// Reads the store directly rather than going through a running sidecar, so
    /// it works whether or not the daemon is up.
    Query {
        /// The statement. The table is `calls`; columns are `date`, `ts`, `kind`,
        /// `cmd`, `sub`, `nargs`, `ms`, `exit`, `error`.
        sql: String,
    },
    /// Print a summary of recorded activity.
    Stats {
        /// How many days to include.
        #[arg(long, default_value_t = 7)]
        days: usize,
    },
}

/// Leave the launching terminal for good: fork, let the parent exit, and put the
/// child in a brand-new session via `setsid`.
///
/// The fork is what makes `setsid` work at all. A process started with `&` from an
/// interactive shell is *already* a process-group leader, and `setsid` fails with
/// `EPERM` for a group leader — so calling it directly from the launched process
/// is a silent no-op that leaves the daemon on the user's tty. The forked child is
/// not a group leader, so its `setsid` succeeds.
///
/// Why bother: a daemon sharing the user's terminal is a process group competing
/// for it, and `sidecar-tui` is the one that loses — it takes `SIGTTIN` on its
/// next stdin read and suspends. Detaching removes the whole class of problem
/// rather than the one instance of it.
///
/// stdout/stderr are deliberately left alone: the caller has usually pointed them
/// at a log file, and that redirection is how the banner and any startup failure
/// stay visible. A process in another session may still write to a tty (`SIGTTOU`
/// only fires with `TOSTOP` set), so an un-redirected launch keeps its output too.
///
/// Best-effort: if the fork fails there is nothing useful to do but keep running
/// attached, which is exactly the old behavior.
#[cfg(unix)]
fn detach_from_terminal() {
    // SAFETY: called before the tokio runtime or any other thread exists, so the
    // usual fork-in-a-multithreaded-process hazards do not apply. The parent uses
    // `_exit` rather than `exit` so it cannot run atexit handlers or flush buffers
    // the child also owns.
    unsafe {
        match libc::fork() {
            -1 => eprintln!(
                "{} ⚠ could not detach from the terminal; running attached",
                claude_sidecar::logger::prefix()
            ),
            0 => {
                libc::setsid();
            }
            _ => libc::_exit(0),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Before the runtime: detaching forks, and forking a process that already has
    // a thread pool is how you get a child holding locks nobody will release.
    // Subcommands are one-shot reporting tools run in the foreground on purpose.
    #[cfg(unix)]
    if cli.detach && cli.command.is_none() {
        detach_from_terminal();
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    // The subcommands are one-shot reporting tools, not the server. They run
    // before tracing is installed so their output is the only thing on stdout.
    if let Some(command) = &cli.command {
        return run_command(command, cli.metrics_dir.clone()).await;
    }

    init_tracing();

    // Load the command policy before anything can serve a request. A malformed
    // file aborts startup rather than falling back to defaults, which would
    // permit exactly what the user was trying to deny.
    let policy_path = cli.config.clone().or_else(sidecar_config::config_path);
    let policy = match &policy_path {
        Some(path) => match sidecar_config::load_policy_from(path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{} ✗ {e}", claude_sidecar::logger::prefix());
                std::process::exit(1);
            }
        },
        None => sidecar_config::Policy::default(),
    };
    sidecar_config::init_policy(policy);

    let config = Arc::new(Config {
        port: cli.port,
        verbose: cli.verbose,
        max_jobs: cli.max_jobs,
        max_lines_per_job: cli.max_lines,
        spill_to_disk: cli.spill,
        job_ttl_secs: cli.job_ttl,
        kill_grace_secs: cli.kill_grace,
    });

    let registry = JobRegistry::new(
        config.max_jobs,
        config.max_lines_per_job,
        config.spill_to_disk,
        config.job_ttl_secs,
    );
    Arc::clone(&registry).spawn_cleanup();
    claude_sidecar::env_refresh::spawn_refresher();

    let events = EventBus::new();
    events.spawn_job_watch(Arc::clone(&registry));

    // Durable call metrics. A failure here is not fatal: the sidecar's job is to
    // run commands, and it must still do that on a full or read-only disk.
    let metrics = if cli.no_metrics {
        None
    } else {
        match open_metrics(cli.metrics_dir.clone()) {
            Ok(recorder) => {
                events.set_metrics(Arc::clone(&recorder));
                // Seals any day left open while the machine was off — the common
                // case on a laptop, where the hourly tick rarely coincides with
                // a midnight boundary.
                recorder.spawn_rollup();
                Some(recorder)
            }
            Err(e) => {
                eprintln!(
                    "{} ⚠ metrics disabled: {e}",
                    claude_sidecar::logger::prefix()
                );
                None
            }
        }
    };

    let state = AppState {
        config: Arc::clone(&config),
        registry,
        events,
        metrics,
    };

    let app = routes::router(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], config.port));
    let listener = match TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding to {addr}"))
    {
        Ok(l) => l,
        Err(e) => {
            // Report and exit non-zero directly. Returning the error would make
            // `main`'s default handler append a second, unattributed `Error:`
            // dump — exactly the line that reads as the *running* server
            // crashing when several instances share one log.
            claude_sidecar::logger::print_startup_failure(config.port, &e);
            std::process::exit(1);
        }
    };

    claude_sidecar::logger::print_banner(config.port);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

/// Open the metrics store at `explicit`, or the default location.
fn open_metrics(explicit: Option<PathBuf>) -> anyhow::Result<Arc<MetricsRecorder>> {
    let root = explicit
        .or_else(MetricsRecorder::default_root)
        .context("no metrics directory: neither XDG_CONFIG_HOME nor HOME is set")?;
    MetricsRecorder::open(root.clone())
        .with_context(|| format!("opening metrics store at {}", root.display()))
}

/// Run a one-shot reporting subcommand against the metrics store.
async fn run_command(command: &Command, metrics_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let recorder = open_metrics(metrics_dir)?;
    match command {
        Command::Query { sql } => {
            println!("{}", recorder.query(sql).await?);
        }
        Command::Stats { days } => {
            let snapshot = recorder.snapshot(*days).await?;
            if snapshot.days.is_empty() {
                println!("No calls recorded yet.");
                return Ok(());
            }
            println!(
                "{} calls over {} day(s)\n",
                snapshot.total_calls,
                snapshot.days.len()
            );
            let widest = snapshot
                .days
                .iter()
                .map(|d| d.calls)
                .max()
                .unwrap_or(1)
                .max(1);
            for day in &snapshot.days {
                // Scaled to the busiest day, so the shape is visible regardless
                // of absolute volume.
                let width = (day.calls * 40 / widest) as usize;
                let failures = if day.failures > 0 {
                    format!("  ({} failed)", day.failures)
                } else {
                    String::new()
                };
                println!(
                    "  {}  {:<40} {}{}",
                    day.date,
                    "█".repeat(width),
                    day.calls,
                    failures
                );
            }
            if !snapshot.commands.is_empty() {
                println!("\nTop commands");
                for stat in &snapshot.commands {
                    let p50 = stat
                        .p50_ms
                        .map(|ms| format!("  p50 {ms}ms"))
                        .unwrap_or_default();
                    println!("  {:<20} {:>6}{}", stat.cmd, stat.calls, p50);
                }
            }
        }
    }
    Ok(())
}

/// Stamps `tracing` events with the same local-time + PID prefix the request log
/// uses, so a `tracing::error!` can be attributed to an instance and correlated
/// with the surrounding request lines.
struct LocalTimePid;

impl FormatTime for LocalTimePid {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", claude_sidecar::logger::prefix())
    }
}

fn init_tracing() {
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_timer(LocalTimePid),
        )
        // Capture warn/error into a retained ring as well as stderr. Without it a
        // failure is only visible to whoever happened to be watching the terminal
        // the daemon was launched from — which for an autostarted sidecar is
        // nobody.
        .with(CaptureDiagnostics)
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();
}

/// A `tracing` layer that retains warnings and errors in memory so `/logs` can
/// serve them after the fact.
struct CaptureDiagnostics;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureDiagnostics {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = *event.metadata().level();
        // Info and below is normal operation and would bury the failures.
        if level > tracing::Level::WARN {
            return;
        }
        let mut message = String::new();
        event.record(&mut MessageVisitor(&mut message));
        if message.is_empty() {
            return;
        }
        claude_sidecar::events::record_diagnostic(
            level.as_str(),
            event.metadata().target(),
            message,
        );
    }
}

/// Pulls the `message` field out of a `tracing` event; other fields are ignored
/// since the message carries what a reader needs.
struct MessageVisitor<'a>(&'a mut String);

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(self.0, "{value:?}");
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }
}

/// Resolve when the process receives Ctrl-C, letting axum drain in-flight
/// requests before exiting.
async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("failed to install Ctrl-C handler: {e}");
    }
    tracing::info!("shutdown signal received");
}
