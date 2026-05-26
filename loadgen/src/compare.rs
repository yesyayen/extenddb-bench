//! Compare driver: bootstrap-CI on `achieved_rps` and `p99_us` per step.
//!
//! Inputs: two single-leg results dirs (each containing meta.json + sweep.json).
//! Outputs: compare-summary.json (schema_version 2) + compare-summary.md.
//!
//! Stat test: percentile-method bootstrap, 1000 resamples, 95% CI on the
//! median across iterations within a step. Verdict per metric:
//!   regression  - candidate CI's lower bound > baseline CI's upper bound (worse)
//!   improvement - candidate CI's upper bound < baseline CI's lower bound (better)
//!   within_noise - CIs overlap.
//! For higher-is-better metrics (achieved_rps), the inequalities flip.
//! Step verdict is the worse of the two metrics' verdicts.
//! Headline verdict is the worst step's verdict.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::ReportCompareArgs;
use crate::output::{Meta, StepRecord};

pub const COMPARE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Improvement,
    WithinNoise,
    Regression,
}

impl Verdict {
    fn worse_of(a: Verdict, b: Verdict) -> Verdict {
        use Verdict::*;
        // Regression > WithinNoise > Improvement.
        match (a, b) {
            (Regression, _) | (_, Regression) => Regression,
            (WithinNoise, _) | (_, WithinNoise) => WithinNoise,
            _ => Improvement,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricComparison {
    pub baseline_median: f64,
    pub candidate_median: f64,
    pub baseline_ci_95: [f64; 2],
    pub candidate_ci_95: [f64; 2],
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepComparison {
    pub step_target_rps: u64,
    pub achieved_rps: MetricComparison,
    pub p99_us: MetricComparison,
    pub step_verdict: Verdict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareSummary {
    pub schema_version: u32,
    pub compare_id: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub workload: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rw_ratio: Option<String>,
    pub rps_sweep: Vec<u64>,
    pub iterations_per_step: u32,
    pub baseline: LegRef,
    pub candidate: LegRef,
    pub stat_test: StatTest,
    pub steps: Vec<StepComparison>,
    pub headline_verdict: Verdict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extenddb_sha: Option<String>,
    pub results_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatTest {
    pub method: String,
    pub resamples: u32,
    pub ci: f64,
}

pub fn report_compare(args: &ReportCompareArgs) -> Result<()> {
    fs::create_dir_all(&args.output)
        .with_context(|| format!("create {}", args.output.display()))?;

    let baseline_meta: Meta = read_json(&args.baseline.join("meta.json"))?;
    let baseline_records: Vec<StepRecord> = read_json(&args.baseline.join("sweep.json"))?;
    let candidate_meta: Meta = read_json(&args.candidate.join("meta.json"))?;
    let candidate_records: Vec<StepRecord> = read_json(&args.candidate.join("sweep.json"))?;

    if baseline_meta.workload != candidate_meta.workload {
        anyhow::bail!(
            "workload mismatch: baseline={:?}, candidate={:?}",
            baseline_meta.workload,
            candidate_meta.workload
        );
    }

    let summary = build_summary(
        &baseline_meta,
        &baseline_records,
        &candidate_meta,
        &candidate_records,
        args,
    );

    let json_path = args.output.join("compare-summary.json");
    let md_path = args.output.join("compare-summary.md");
    let json_file =
        fs::File::create(&json_path).with_context(|| format!("create {}", json_path.display()))?;
    serde_json::to_writer_pretty(json_file, &summary).context("serialize compare-summary.json")?;
    fs::write(&md_path, render_markdown(&summary))
        .with_context(|| format!("write {}", md_path.display()))?;

    println!("compare-summary: {}", md_path.display());
    println!("headline verdict: {:?}", summary.headline_verdict);

    // CI-friendly exit: regression -> non-zero.
    if matches!(summary.headline_verdict, Verdict::Regression) {
        std::process::exit(1);
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    serde_json::from_reader(f).with_context(|| format!("parse {}", path.display()))
}

pub fn build_summary(
    baseline_meta: &Meta,
    baseline: &[StepRecord],
    candidate_meta: &Meta,
    candidate: &[StepRecord],
    args: &ReportCompareArgs,
) -> CompareSummary {
    use std::collections::BTreeMap;
    let mut bsteps: BTreeMap<u64, Vec<&StepRecord>> = Default::default();
    let mut csteps: BTreeMap<u64, Vec<&StepRecord>> = Default::default();
    for r in baseline {
        bsteps.entry(r.step_target_rps).or_default().push(r);
    }
    for r in candidate {
        csteps.entry(r.step_target_rps).or_default().push(r);
    }

    let common_steps: Vec<u64> = bsteps
        .keys()
        .filter(|k| csteps.contains_key(k))
        .copied()
        .collect();

    let resamples = args.resamples;
    let mut steps_out = Vec::with_capacity(common_steps.len());
    let mut headline = Verdict::Improvement;

    for rps in &common_steps {
        let bvec = &bsteps[rps];
        let cvec = &csteps[rps];

        let b_ach: Vec<f64> = bvec.iter().map(|r| r.achieved_rps).collect();
        let c_ach: Vec<f64> = cvec.iter().map(|r| r.achieved_rps).collect();
        let b_p99: Vec<f64> = bvec.iter().map(|r| r.p99_us as f64).collect();
        let c_p99: Vec<f64> = cvec.iter().map(|r| r.p99_us as f64).collect();

        let achieved = compare_metric(&b_ach, &c_ach, resamples, /*higher_is_better=*/ true);
        let p99 = compare_metric(&b_p99, &c_p99, resamples, /*higher_is_better=*/ false);
        let step_v = Verdict::worse_of(achieved.verdict, p99.verdict);
        headline = Verdict::worse_of(headline, step_v);

        steps_out.push(StepComparison {
            step_target_rps: *rps,
            achieved_rps: achieved,
            p99_us: p99,
            step_verdict: step_v,
        });
    }

    let rw_ratio = if baseline_meta.workload == "mixed-rw" {
        Some("80:20".to_string())
    } else {
        None
    };

    CompareSummary {
        schema_version: COMPARE_SCHEMA_VERSION,
        compare_id: args.compare_id.clone(),
        started_at: Some(baseline_meta.started_at.to_rfc3339()),
        ended_at: candidate_meta.ended_at.map(|t| t.to_rfc3339()),
        workload: baseline_meta.workload.clone(),
        rw_ratio,
        rps_sweep: common_steps,
        iterations_per_step: baseline_meta.iterations,
        baseline: LegRef {
            extenddb_sha: baseline_meta.extenddb_sha.clone(),
            results_dir: format!("{}", args.baseline.display()),
        },
        candidate: LegRef {
            extenddb_sha: candidate_meta.extenddb_sha.clone(),
            results_dir: format!("{}", args.candidate.display()),
        },
        stat_test: StatTest {
            method: "bootstrap_percentile".to_string(),
            resamples,
            ci: 0.95,
        },
        steps: steps_out,
        headline_verdict: headline,
    }
}

fn compare_metric(b: &[f64], c: &[f64], resamples: u32, higher_is_better: bool) -> MetricComparison {
    let b_med = median(b);
    let c_med = median(c);
    let b_ci = bootstrap_ci_median(b, resamples, /*seed=*/ 0xB0_0B_5);
    let c_ci = bootstrap_ci_median(c, resamples, /*seed=*/ 0xCAFE);
    let verdict = decide_verdict(b_ci, c_ci, higher_is_better);
    MetricComparison {
        baseline_median: b_med,
        candidate_median: c_med,
        baseline_ci_95: b_ci,
        candidate_ci_95: c_ci,
        verdict,
    }
}

fn decide_verdict(b: [f64; 2], c: [f64; 2], higher_is_better: bool) -> Verdict {
    let (b_lo, b_hi) = (b[0], b[1]);
    let (c_lo, c_hi) = (c[0], c[1]);
    if higher_is_better {
        // higher is better: candidate higher than baseline -> improvement.
        if c_lo > b_hi {
            Verdict::Improvement
        } else if c_hi < b_lo {
            Verdict::Regression
        } else {
            Verdict::WithinNoise
        }
    } else {
        // lower is better (latency): candidate lower -> improvement.
        if c_hi < b_lo {
            Verdict::Improvement
        } else if c_lo > b_hi {
            Verdict::Regression
        } else {
            Verdict::WithinNoise
        }
    }
}

pub fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = xs.iter().copied().collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Percentile-method bootstrap 95% CI on the median.
/// Uses a deterministic per-call seed so self-compares are reproducible.
pub fn bootstrap_ci_median(xs: &[f64], resamples: u32, seed: u64) -> [f64; 2] {
    if xs.is_empty() {
        return [0.0, 0.0];
    }
    if xs.len() == 1 {
        return [xs[0], xs[0]];
    }
    let mut rng = fastrand::Rng::with_seed(seed);
    let n = xs.len();
    let mut medians: Vec<f64> = Vec::with_capacity(resamples as usize);
    let mut buf: Vec<f64> = Vec::with_capacity(n);
    for _ in 0..resamples {
        buf.clear();
        for _ in 0..n {
            let idx = rng.usize(0..n);
            buf.push(xs[idx]);
        }
        medians.push(median(&buf));
    }
    medians.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lo_idx = (medians.len() as f64 * 0.025).floor() as usize;
    let hi_idx = ((medians.len() as f64 * 0.975).ceil() as usize).min(medians.len() - 1);
    [medians[lo_idx], medians[hi_idx]]
}

fn render_markdown(s: &CompareSummary) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "# extenddb-bench compare: {}", s.workload);
    let _ = writeln!(out);
    if let Some(id) = &s.compare_id {
        let _ = writeln!(out, "- compare_id: `{id}`");
    }
    let _ = writeln!(out, "- baseline_sha: `{}`", s.baseline.extenddb_sha.as_deref().unwrap_or("?"));
    let _ = writeln!(out, "- candidate_sha: `{}`", s.candidate.extenddb_sha.as_deref().unwrap_or("?"));
    let _ = writeln!(out, "- iterations_per_step: {}", s.iterations_per_step);
    if let Some(rw) = &s.rw_ratio {
        let _ = writeln!(out, "- rw_ratio: {rw}");
    }
    let _ = writeln!(out, "- headline verdict: **{:?}**", s.headline_verdict);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Per-step verdict");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| target_rps | baseline_rps | candidate_rps | RPS verdict | baseline_p99_us | candidate_p99_us | p99 verdict | step verdict |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
    for st in &s.steps {
        let _ = writeln!(
            out,
            "| {} | {:.1} | {:.1} | {:?} | {:.0} | {:.0} | {:?} | {:?} |",
            st.step_target_rps,
            st.achieved_rps.baseline_median,
            st.achieved_rps.candidate_median,
            st.achieved_rps.verdict,
            st.p99_us.baseline_median,
            st.p99_us.candidate_median,
            st.p99_us.verdict,
            st.step_verdict,
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Stat test");
    let _ = writeln!(
        out,
        "- method: `{}` ({} resamples, {:.0}% CI)",
        s.stat_test.method,
        s.stat_test.resamples,
        s.stat_test.ci * 100.0
    );
    let _ = writeln!(out, "- baseline results dir: `{}`", s.baseline.results_dir);
    let _ = writeln!(out, "- candidate results dir: `{}`", s.candidate.results_dir);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_basic() {
        assert_eq!(median(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }

    #[test]
    fn bootstrap_ci_collapses_for_constant() {
        let ci = bootstrap_ci_median(&[5.0, 5.0, 5.0, 5.0, 5.0], 1000, 1);
        assert_eq!(ci, [5.0, 5.0]);
    }

    #[test]
    fn self_compare_is_within_noise() {
        // Same iteration values for both legs -> identical bootstrap distributions
        // -> CIs identical -> within_noise on both metrics.
        let b = vec![100.0, 102.0, 99.0];
        let c = b.clone();
        let comp = compare_metric(&b, &c, 1000, true);
        assert_eq!(comp.verdict, Verdict::WithinNoise);
        let comp_low = compare_metric(&b, &c, 1000, false);
        assert_eq!(comp_low.verdict, Verdict::WithinNoise);
    }

    #[test]
    fn obvious_improvement_higher_better() {
        // Candidate clearly higher RPS, non-overlapping.
        let b = vec![100.0, 101.0, 102.0];
        let c = vec![200.0, 201.0, 202.0];
        let comp = compare_metric(&b, &c, 1000, true);
        assert_eq!(comp.verdict, Verdict::Improvement);
    }

    #[test]
    fn obvious_regression_lower_better() {
        // Candidate p99 is way higher than baseline -> regression.
        let b = vec![1000.0, 1001.0, 1002.0];
        let c = vec![5000.0, 5001.0, 5002.0];
        let comp = compare_metric(&b, &c, 1000, false);
        assert_eq!(comp.verdict, Verdict::Regression);
    }

    #[test]
    fn worse_of_ordering() {
        use Verdict::*;
        assert_eq!(Verdict::worse_of(Improvement, WithinNoise), WithinNoise);
        assert_eq!(Verdict::worse_of(WithinNoise, Regression), Regression);
        assert_eq!(Verdict::worse_of(Regression, Improvement), Regression);
    }
}
