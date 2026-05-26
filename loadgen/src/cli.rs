//! CLI definitions for `extenddb-bench`.
//!
//! Top-level subcommands:
//! - `run`     — execute a sweep
//! - `report`  — re-render `summary.md` from an existing results dir
//! - `version` — print bench tool SHA + ExtendDB SDK version

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "extenddb-bench")]
#[command(about = "Open-loop load generator for the ExtendDB perf POC", long_about = None)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Execute a benchmark sweep.
    Run(RunArgs),
    /// Re-render summary.md from an existing results directory.
    Report(ReportArgs),
    /// Print bench tool's git SHA + ExtendDB SDK version.
    Version,
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// ExtendDB endpoint, e.g. https://10.42.1.42:8000
    #[arg(long)]
    pub target: String,

    /// Workload selector (only `putitem-1kb` is supported in v0.1).
    #[arg(long, default_value = "putitem-1kb")]
    pub workload: String,

    /// Comma-separated RPS sweep, e.g. 1000,5000,25000,100000,250000.
    #[arg(long, conflicts_with = "rps_sweep_file", default_value = "1000,5000,25000,100000,250000")]
    pub rps_sweep: String,

    /// File containing a CSV of RPS values (one or many lines).
    #[arg(long, conflicts_with = "rps_sweep")]
    pub rps_sweep_file: Option<PathBuf>,

    /// Per-step measure window.
    #[arg(long, default_value = "60s", value_parser = humantime::parse_duration)]
    pub duration: Duration,

    /// Discarded warmup before measure.
    #[arg(long, default_value = "10s", value_parser = humantime::parse_duration)]
    pub warmup: Duration,

    /// Quiet window between iterations and steps.
    #[arg(long, default_value = "5s", value_parser = humantime::parse_duration)]
    pub cooldown: Duration,

    /// Iterations per RPS step.
    #[arg(long, default_value_t = 3)]
    pub iterations: u32,

    /// Concurrent in-flight requests.
    #[arg(long, default_value_t = 64)]
    pub connections: u32,

    /// Uniform-random key cardinality for PutItem.
    #[arg(long, default_value_t = 1_000_000)]
    pub keyspace: u64,

    /// Item payload size in bytes (PutItem `val` attribute).
    #[arg(long, default_value_t = 1024)]
    pub item_size_bytes: usize,

    /// Results directory. Defaults to ./results/<UTC-timestamp>.
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// AWS region for SigV4.
    #[arg(long, default_value = "us-east-1", env = "AWS_REGION")]
    pub aws_region: String,

    /// PEM CA bundle that signs the SUT's self-signed TLS cert.
    #[arg(long, env = "EXTENDDB_BENCH_CA_BUNDLE")]
    pub tls_ca_bundle: Option<PathBuf>,

    /// Skip TLS verification (useful for a quick smoke test only).
    #[arg(long)]
    pub tls_insecure: bool,

    /// DynamoDB table name (must already exist).
    #[arg(long, default_value = "bench")]
    pub table_name: String,

    /// Prometheus exposition port.
    #[arg(long, default_value_t = 9090)]
    pub metrics_port: u16,

    /// Pinned ExtendDB SHA (40-char). Echoed in result files; not validated against the server.
    #[arg(long, env = "EXTENDDB_BENCH_SHA")]
    pub extenddb_sha: Option<String>,

    /// Stop the sweep once a step trips the saturation rule (default: continue).
    #[arg(long)]
    pub stop_at_saturation: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ReportArgs {
    /// Results directory containing meta.json + sweep.json.
    #[arg(long)]
    pub input: PathBuf,
}

mod humantime {
    use std::time::Duration;

    pub fn parse_duration(input: &str) -> Result<Duration, String> {
        let trimmed = input.trim();
        let (num_str, unit) = trimmed
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .map(|i| trimmed.split_at(i))
            .unwrap_or((trimmed, "s"));
        let value: f64 = num_str.parse().map_err(|e| format!("invalid number: {e}"))?;
        let secs = match unit {
            "" | "s" | "sec" | "secs" => value,
            "ms" => value / 1000.0,
            "m" | "min" | "mins" => value * 60.0,
            "h" | "hr" | "hrs" => value * 3600.0,
            other => return Err(format!("unknown unit: {other:?}")),
        };
        if !secs.is_finite() || secs < 0.0 {
            return Err("duration must be non-negative".into());
        }
        Ok(Duration::from_secs_f64(secs))
    }
}
