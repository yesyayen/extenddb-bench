//! Prometheus exposition for live observability during a sweep.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use metrics_exporter_prometheus::PrometheusBuilder;

pub fn install_recorder(port: u16) -> Result<()> {
    let bind: SocketAddr = ([0, 0, 0, 0], port).into();
    PrometheusBuilder::new()
        .with_http_listener(bind)
        .install()
        .with_context(|| format!("install Prometheus exporter on :{port}"))?;
    tracing::info!(target: "extenddb_bench", "Prometheus exposition on http://{bind}/metrics");
    Ok(())
}

/// Metric names. Centralized so output.rs and runner.rs agree on labels.
pub mod names {
    pub const TARGET_RPS: &str = "loadgen_target_rps";
    pub const ACHIEVED_RPS: &str = "loadgen_achieved_rps";
    pub const INFLIGHT: &str = "loadgen_inflight_requests";
    pub const REQUEST_DURATION: &str = "loadgen_request_duration_seconds";
    pub const ERRORS_TOTAL: &str = "loadgen_errors_total";
    pub const STEP_INDEX: &str = "loadgen_step_index";
    pub const ITERATION_INDEX: &str = "loadgen_iteration_index";
}
