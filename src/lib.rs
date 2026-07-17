pub mod config;
pub mod error;
pub mod job;
pub mod logger;
pub mod pty;
pub mod routes;
#[cfg(feature = "tui")]
pub mod tui;

use std::sync::Arc;

use job::JobRegistry;

pub use config::Config;

/// Shared application state, cloned into every request handler. Both fields are
/// `Arc`, so cloning is cheap.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub registry: Arc<JobRegistry>,
}
