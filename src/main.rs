use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use claude_sidecar::{job::JobRegistry, routes, AppState, Config};

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing();

    let config = Arc::new(Config {
        port: cli.port,
        verbose: cli.verbose,
        max_jobs: cli.max_jobs,
        max_lines_per_job: cli.max_lines,
        spill_to_disk: cli.spill,
        job_ttl_secs: cli.job_ttl,
    });

    let registry = JobRegistry::new(
        config.max_jobs,
        config.max_lines_per_job,
        config.spill_to_disk,
        config.job_ttl_secs,
    );
    Arc::clone(&registry).spawn_cleanup();

    let state = AppState {
        config: Arc::clone(&config),
        registry,
    };

    let app = routes::router(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], config.port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding to {addr}"))?;

    claude_sidecar::logger::print_banner(config.port);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

fn init_tracing() {
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr).without_time())
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();
}

/// Resolve when the process receives Ctrl-C, letting axum drain in-flight
/// requests before exiting.
async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("failed to install Ctrl-C handler: {e}");
    }
    tracing::info!("shutdown signal received");
}
