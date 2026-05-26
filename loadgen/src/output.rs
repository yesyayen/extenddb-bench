//! Output writers: meta.json, sweep.json, saturation.json, summary.md.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::histogram::Percentiles;
use crate::lg_health::LgHealthReport;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: u32,
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub extenddb_sha: Option<String>,
    pub bench_sha: Option<String>,
    pub workload: String,
    pub rps_sweep: Vec<u64>,
    pub duration_secs: u64,
    pub warmup_secs: u64,
    pub cooldown_secs: u64,
    pub iterations: u32,
    pub connections: u32,
    pub keyspace: u64,
    pub item_size_bytes: usize,
    pub target: String,
    pub aws_region: String,
    pub table_name: String,
    /// Leg of a compare run (`baseline` / `candidate`) when set.
    /// Optional and additive: existing v0.1 single-leg meta.json stays valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg: Option<String>,
    /// Compare-run id when this run is a leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub step_target_rps: u64,
    pub iteration: u32,
    pub achieved_rps: f64,
    pub total_requests: u64,
    pub errors_total: u64,
    pub error_rate: f64,
    pub p50_us: u64,
    pub p90_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
    pub max_us: u64,
    pub lg_cpu_p99_pct: f64,
    pub lg_cpu_mean_pct: f64,
    pub lg_rss_max_bytes: u64,
    pub lg_bottlenecked: bool,
    pub histogram_file: String,
    /// Per-op-kind p99 split (only set for workloads that expose `op_kind`,
    /// e.g. `mixed-rw`). Otherwise omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_p99_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_p50_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_p99_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_p50_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saturation {
    pub max_sustained_rps: Option<u64>,
    pub p99_at_max_us: Option<u64>,
    pub cliff_step_rps: Option<u64>,
    pub cliff_reason: Option<String>,
    pub p99_at_cliff_us: Option<u64>,
    pub error_rate_at_cliff: Option<f64>,
    pub relative_stddev_at_max_pct: Option<f64>,
}

pub fn write_meta(dir: &Path, meta: &Meta) -> Result<()> {
    let path = dir.join("meta.json");
    let f = std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(f, meta).context("serialize meta.json")?;
    Ok(())
}

pub fn write_sweep(dir: &Path, records: &[StepRecord]) -> Result<()> {
    let path = dir.join("sweep.json");
    let f = std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(f, records).context("serialize sweep.json")?;
    Ok(())
}

pub fn write_saturation(dir: &Path, sat: &Saturation) -> Result<()> {
    let path = dir.join("saturation.json");
    let f = std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(f, sat).context("serialize saturation.json")?;
    Ok(())
}

pub fn build_step_record(
    target_rps: u64,
    iteration: u32,
    achieved_rps: f64,
    total_requests: u64,
    errors_total: u64,
    pct: Percentiles,
    lg: LgHealthReport,
    histogram_file: String,
    split: Option<(SplitPct, SplitPct)>,
) -> StepRecord {
    let error_rate = if total_requests + errors_total == 0 {
        0.0
    } else {
        errors_total as f64 / (total_requests + errors_total) as f64
    };
    let (read_p50, read_p99, read_count, write_p50, write_p99, write_count) = match split {
        Some((r, w)) => (
            Some(r.p50_us),
            Some(r.p99_us),
            Some(r.count),
            Some(w.p50_us),
            Some(w.p99_us),
            Some(w.count),
        ),
        None => (None, None, None, None, None, None),
    };
    StepRecord {
        step_target_rps: target_rps,
        iteration,
        achieved_rps,
        total_requests,
        errors_total,
        error_rate,
        p50_us: pct.p50_us,
        p90_us: pct.p90_us,
        p99_us: pct.p99_us,
        p999_us: pct.p999_us,
        max_us: pct.max_us,
        lg_cpu_p99_pct: lg.cpu_p99_pct,
        lg_cpu_mean_pct: lg.cpu_mean_pct,
        lg_rss_max_bytes: lg.rss_max_bytes,
        lg_bottlenecked: lg.bottlenecked,
        histogram_file,
        read_p50_us: read_p50,
        read_p99_us: read_p99,
        read_count,
        write_p50_us: write_p50,
        write_p99_us: write_p99,
        write_count,
    }
}

/// Per-op-kind percentile slice for split records.
#[derive(Debug, Clone, Copy)]
pub struct SplitPct {
    pub p50_us: u64,
    pub p99_us: u64,
    pub count: u64,
}

const SATURATION_P99_THRESHOLD_US: u64 = 100_000; // 100 ms
const SATURATION_ERROR_RATE_THRESHOLD: f64 = 0.01; // 1 %

pub fn compute_saturation(records: &[StepRecord]) -> Saturation {
    if records.is_empty() {
        return empty_saturation();
    }

    // Group by step_target_rps; aggregate iterations.
    let mut steps: std::collections::BTreeMap<u64, Vec<&StepRecord>> = Default::default();
    for r in records {
        steps.entry(r.step_target_rps).or_default().push(r);
    }

    let mut sustained_step: Option<u64> = None;
    let mut sustained_p99: Option<u64> = None;
    let mut sustained_relstddev_pct: Option<f64> = None;
    let mut cliff_step: Option<u64> = None;
    let mut cliff_reason: Option<String> = None;
    let mut cliff_p99: Option<u64> = None;
    let mut cliff_err_rate: Option<f64> = None;

    for (rps, iters) in steps {
        let mean_p99 = mean(iters.iter().map(|r| r.p99_us as f64));
        let mean_err = mean(iters.iter().map(|r| r.error_rate));
        if mean_p99 > SATURATION_P99_THRESHOLD_US as f64 {
            cliff_step = Some(rps);
            cliff_reason = Some("p99_exceeded_100ms".into());
            cliff_p99 = Some(mean_p99 as u64);
            cliff_err_rate = Some(mean_err);
            break;
        }
        if mean_err > SATURATION_ERROR_RATE_THRESHOLD {
            cliff_step = Some(rps);
            cliff_reason = Some("error_rate_exceeded_1pct".into());
            cliff_p99 = Some(mean_p99 as u64);
            cliff_err_rate = Some(mean_err);
            break;
        }
        // This step is sustained. Track relative stddev of achieved_rps across iterations.
        sustained_step = Some(rps);
        sustained_p99 = Some(mean_p99 as u64);
        let achieved: Vec<f64> = iters.iter().map(|r| r.achieved_rps).collect();
        sustained_relstddev_pct = Some(relative_stddev_pct(&achieved));
    }

    Saturation {
        max_sustained_rps: sustained_step,
        p99_at_max_us: sustained_p99,
        cliff_step_rps: cliff_step,
        cliff_reason,
        p99_at_cliff_us: cliff_p99,
        error_rate_at_cliff: cliff_err_rate,
        relative_stddev_at_max_pct: sustained_relstddev_pct,
    }
}

fn empty_saturation() -> Saturation {
    Saturation {
        max_sustained_rps: None,
        p99_at_max_us: None,
        cliff_step_rps: None,
        cliff_reason: None,
        p99_at_cliff_us: None,
        error_rate_at_cliff: None,
        relative_stddev_at_max_pct: None,
    }
}

fn mean<I: Iterator<Item = f64>>(iter: I) -> f64 {
    let xs: Vec<f64> = iter.collect();
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / (xs.len() as f64)
    }
}

fn relative_stddev_pct(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = xs.iter().sum::<f64>() / xs.len() as f64;
    if m.abs() < f64::EPSILON {
        return 0.0;
    }
    let var = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() as f64 - 1.0);
    (var.sqrt() / m) * 100.0
}

pub fn write_summary(dir: &Path, meta: &Meta, records: &[StepRecord], sat: &Saturation) -> Result<()> {
    let path = dir.join("summary.md");
    let mut f = std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
    write_summary_to(&mut f, meta, records, sat)
}

fn write_summary_to<W: Write>(
    out: &mut W,
    meta: &Meta,
    records: &[StepRecord],
    sat: &Saturation,
) -> Result<()> {
    writeln!(out, "# extenddb-bench: {} ({})", meta.workload, meta.run_id)?;
    writeln!(out)?;
    writeln!(out, "## Run metadata")?;
    writeln!(out)?;
    writeln!(out, "| Field | Value |")?;
    writeln!(out, "|---|---|")?;
    writeln!(out, "| run_id | `{}` |", meta.run_id)?;
    writeln!(out, "| started_at | `{}` |", meta.started_at.to_rfc3339())?;
    if let Some(t) = meta.ended_at {
        writeln!(out, "| ended_at | `{}` |", t.to_rfc3339())?;
    }
    if let Some(s) = &meta.extenddb_sha {
        writeln!(out, "| extenddb_sha | `{s}` |")?;
    }
    writeln!(out, "| target | `{}` |", meta.target)?;
    writeln!(out, "| table | `{}` |", meta.table_name)?;
    writeln!(out, "| region | `{}` |", meta.aws_region)?;
    writeln!(out, "| workload | `{}` |", meta.workload)?;
    writeln!(out, "| rps_sweep | `{:?}` |", meta.rps_sweep)?;
    writeln!(out, "| duration_secs | {} |", meta.duration_secs)?;
    writeln!(out, "| warmup_secs | {} |", meta.warmup_secs)?;
    writeln!(out, "| cooldown_secs | {} |", meta.cooldown_secs)?;
    writeln!(out, "| iterations | {} |", meta.iterations)?;
    writeln!(out, "| connections | {} |", meta.connections)?;
    writeln!(out, "| keyspace | {} |", meta.keyspace)?;
    writeln!(out, "| item_size_bytes | {} |", meta.item_size_bytes)?;
    writeln!(out)?;

    writeln!(out, "## Sweep")?;
    writeln!(out)?;
    writeln!(
        out,
        "| target_rps | iter | achieved_rps | p50_us | p90_us | p99_us | p99.9_us | r_p99 | w_p99 | errors | err_rate | LG_cpu_p99 | LG bottleneck |"
    )?;
    writeln!(out, "|---|---|---|---|---|---|---|---|---|---|---|---|---|")?;
    for r in records {
        let r_p99 = r.read_p99_us.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
        let w_p99 = r.write_p99_us.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
        writeln!(
            out,
            "| {} | {} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {:.3} | {:.1}% | {} |",
            r.step_target_rps,
            r.iteration,
            r.achieved_rps,
            r.p50_us,
            r.p90_us,
            r.p99_us,
            r.p999_us,
            r_p99,
            w_p99,
            r.errors_total,
            r.error_rate,
            r.lg_cpu_p99_pct,
            if r.lg_bottlenecked { "**LG**" } else { "ok" },
        )?;
    }
    writeln!(out)?;

    writeln!(out, "## Saturation")?;
    writeln!(out)?;
    if let Some(rps) = sat.max_sustained_rps {
        writeln!(out, "- max_sustained_rps: **{rps}**")?;
        if let Some(p) = sat.p99_at_max_us {
            writeln!(out, "- p99 at max: {p} us")?;
        }
        if let Some(s) = sat.relative_stddev_at_max_pct {
            writeln!(out, "- relative stddev at max: {:.2}%", s)?;
        }
    } else {
        writeln!(out, "- no step succeeded under saturation thresholds")?;
    }
    if let Some(rps) = sat.cliff_step_rps {
        writeln!(out, "- cliff at: **{rps} RPS**")?;
        if let Some(r) = &sat.cliff_reason {
            writeln!(out, "- cliff_reason: `{r}`")?;
        }
        if let Some(p) = sat.p99_at_cliff_us {
            writeln!(out, "- p99 at cliff: {p} us")?;
        }
        if let Some(e) = sat.error_rate_at_cliff {
            writeln!(out, "- error_rate at cliff: {:.4}", e)?;
        }
    }
    writeln!(out)?;

    let bottlenecked: Vec<_> = records.iter().filter(|r| r.lg_bottlenecked).collect();
    if !bottlenecked.is_empty() {
        writeln!(out, "## ⚠ LG-bottlenecked steps (results invalid)")?;
        writeln!(out)?;
        for r in bottlenecked {
            writeln!(
                out,
                "- step={} iter={} LG cpu_p99={:.1}%",
                r.step_target_rps, r.iteration, r.lg_cpu_p99_pct
            )?;
        }
        writeln!(out)?;
    }

    Ok(())
}

/// Re-render summary.md from an existing results dir (the `report` subcommand).
pub fn re_render_summary(dir: &Path) -> Result<()> {
    let meta_path = dir.join("meta.json");
    let sweep_path = dir.join("sweep.json");
    let sat_path = dir.join("saturation.json");
    let meta: Meta = serde_json::from_reader(std::fs::File::open(&meta_path)?)?;
    let records: Vec<StepRecord> =
        serde_json::from_reader(std::fs::File::open(&sweep_path)?)?;
    let sat: Saturation = if sat_path.exists() {
        serde_json::from_reader(std::fs::File::open(&sat_path)?)?
    } else {
        compute_saturation(&records)
    };
    write_summary(dir, &meta, &records, &sat)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(rps: u64, iter: u32, p99: u64, err_rate: f64) -> StepRecord {
        StepRecord {
            step_target_rps: rps,
            iteration: iter,
            achieved_rps: rps as f64 * 0.99,
            total_requests: 1000,
            errors_total: (1000.0 * err_rate) as u64,
            error_rate: err_rate,
            p50_us: p99 / 2,
            p90_us: p99 * 8 / 10,
            p99_us: p99,
            p999_us: p99 * 12 / 10,
            max_us: p99 * 2,
            lg_cpu_p99_pct: 50.0,
            lg_cpu_mean_pct: 30.0,
            lg_rss_max_bytes: 1_000_000,
            lg_bottlenecked: false,
            histogram_file: format!("step-{rps:06}-iter-{iter}.hgrm"),
            read_p50_us: None,
            read_p99_us: None,
            read_count: None,
            write_p50_us: None,
            write_p99_us: None,
            write_count: None,
        }
    }

    #[test]
    fn saturation_finds_p99_cliff() {
        let records = vec![
            make_record(1000, 1, 1000, 0.0),
            make_record(1000, 2, 1100, 0.0),
            make_record(5000, 1, 80_000, 0.0),
            make_record(25_000, 1, 200_000, 0.0),
        ];
        let s = compute_saturation(&records);
        assert_eq!(s.max_sustained_rps, Some(5000));
        assert_eq!(s.cliff_step_rps, Some(25_000));
        assert_eq!(s.cliff_reason.as_deref(), Some("p99_exceeded_100ms"));
    }

    #[test]
    fn saturation_finds_error_cliff() {
        let records = vec![
            make_record(1000, 1, 1000, 0.0),
            make_record(5000, 1, 80_000, 0.05),
        ];
        let s = compute_saturation(&records);
        assert_eq!(s.max_sustained_rps, Some(1000));
        assert_eq!(s.cliff_reason.as_deref(), Some("error_rate_exceeded_1pct"));
    }
}
