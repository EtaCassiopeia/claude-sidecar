pub mod config;
pub mod env_refresh;
pub mod error;
pub mod events;
pub mod gdocs;
pub mod job;
pub mod logger;
pub mod metrics;
pub mod pty;
pub mod routes;
#[cfg(feature = "tui")]
pub mod tui;

use std::sync::Arc;

use events::EventBus;
use job::JobRegistry;
use metrics::MetricsRecorder;

pub use config::Config;

/// Shared application state, cloned into every request handler. Every field is
/// `Arc`, so cloning is cheap.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub registry: Arc<JobRegistry>,
    /// Server-wide event fan-out for watchers. See [`events`].
    pub events: Arc<EventBus>,
    /// Durable call metrics. `None` when no writable directory could be found,
    /// in which case the sidecar runs normally and only `/stats` is unavailable.
    pub metrics: Option<Arc<MetricsRecorder>>,
}
