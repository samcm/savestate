//! Logging and Prometheus metrics for the daemons. Metrics never carry per-object
//! labels.

use std::net::SocketAddr;

/// Initialises `tracing` to stderr as JSON lines. `RUST_LOG` overrides the default
/// `info` filter.
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // Fails only if a subscriber is already set, which then keeps logging.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .json()
        .try_init();
}

/// Serves Prometheus metrics on `listen` at `/metrics`. Needs a Tokio runtime.
pub fn init_metrics(listen: SocketAddr) -> anyhow::Result<()> {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(listen)
        .install()?;
    Ok(())
}
