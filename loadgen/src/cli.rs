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
    /// Idempotent pre-seed of the bench keyspace (PutItem fill).
    Preseed(PreseedArgs),
    /// Re-render summary.md from an existing results directory.
    Report(ReportArgs),
    /// Combine two single-leg results dirs into a compare-summary.
    ReportCompare(ReportCompareArgs),
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

    /// Ensure the bench keyspace is pre-seeded before the sweep.
    /// Implicitly true for read/update/mixed workloads via `requires_preseed()`.
    #[arg(long)]
    pub ensure_preseed: bool,

    /// RPS cap for the implicit pre-seed phase.
    #[arg(long, default_value_t = 50_000)]
    pub preseed_rps: u64,

    /// S3 bucket for the pre-seed stamp file. Defaults to `EXTENDDB_BENCH_RESULTS_BUCKET`.
    #[arg(long, env = "EXTENDDB_BENCH_RESULTS_BUCKET")]
    pub stamp_bucket: Option<String>,

    /// Read:Write ratio for the `mixed-rw` workload. Format `R:W` summing to 100.
    #[arg(long, default_value = "80:20")]
    pub rw_ratio: String,

    /// Tag this run as a leg of a compare run (writes leg metadata + emits a leg-tag metric).
    #[arg(long)]
    pub leg_tag: Option<String>,

    /// Compare-run id (set by `compare-shas.sh`; recorded in meta.json).
    #[arg(long)]
    pub compare_id: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct PreseedArgs {
    /// ExtendDB endpoint, e.g. https://10.42.1.42:8000
    #[arg(long)]
    pub target: String,

    /// DynamoDB table name (must already exist).
    #[arg(long, default_value = "bench")]
    pub table_name: String,

    /// Number of items to seed (deterministic key range [0, keyspace)).
    #[arg(long, default_value_t = 1_000_000)]
    pub keyspace: u64,

    /// Item payload size in bytes.
    #[arg(long, default_value_t = 1024)]
    pub item_size_bytes: usize,

    /// Concurrent in-flight PutItems.
    #[arg(long, default_value_t = 256)]
    pub connections: u32,

    /// RPS cap for the pre-seed phase.
    #[arg(long, default_value_t = 50_000)]
    pub preseed_rps: u64,

    /// AWS region for SigV4.
    #[arg(long, default_value = "us-east-1", env = "AWS_REGION")]
    pub aws_region: String,

    /// PEM CA bundle that signs the SUT's self-signed TLS cert.
    #[arg(long, env = "EXTENDDB_BENCH_CA_BUNDLE")]
    pub tls_ca_bundle: Option<PathBuf>,

    /// Skip TLS verification.
    #[arg(long)]
    pub tls_insecure: bool,

    /// S3 bucket for the stamp file.
    #[arg(long, env = "EXTENDDB_BENCH_RESULTS_BUCKET")]
    pub stamp_bucket: Option<String>,

    /// Pinned ExtendDB SHA. The stamp key is keyed by SHA so a swap re-seeds.
    #[arg(long, env = "EXTENDDB_BENCH_SHA")]
    pub extenddb_sha: Option<String>,

    /// Force a re-seed even if a stamp is present.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ReportArgs {
    /// Results directory containing meta.json + sweep.json.
    #[arg(long)]
    pub input: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct ReportCompareArgs {
    /// Baseline-leg results directory (contains meta.json + sweep.json).
    #[arg(long)]
    pub baseline: PathBuf,
    /// Candidate-leg results directory.
    #[arg(long)]
    pub candidate: PathBuf,
    /// Output directory for compare-summary.{json,md}.
    #[arg(long)]
    pub output: PathBuf,
    /// Compare run id (forwarded into compare-summary.json).
    #[arg(long)]
    pub compare_id: Option<String>,
    /// Bootstrap resamples for the 95% CI (default 1000).
    #[arg(long, default_value_t = 1000)]
    pub resamples: u32,
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
