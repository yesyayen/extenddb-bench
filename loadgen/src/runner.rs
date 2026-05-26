//! Open-loop sweep runner.
//!
//! For each (RPS step × iteration) we:
//!   1. spawn the LG self-monitoring sampler
//!   2. issue requests at exactly `target_rps` for `warmup + duration`
//!      seconds; warmup samples are discarded
//!   3. record durations into a per-iteration HDR histogram
//!   4. write `step-<rps>-iter-<n>.hgrm` to disk
//!   5. cool down for `cooldown` seconds before the next iteration/step

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use hdrhistogram::Histogram;
use std::sync::Mutex;

use crate::cli::RunArgs;
use crate::client::{self, ClientConfig};
use crate::histogram::{new_latency_histogram, write_hgrm, Percentiles};
use crate::lg_health::LgHealth;
use crate::metrics;
use crate::output::{self, Meta};
use crate::sweep::Sweep;
use crate::workload;

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

pub async fn run(args: RunArgs) -> Result<()> {
    metrics::install_recorder(args.metrics_port)?;

    let sweep = if let Some(path) = &args.rps_sweep_file {
        Sweep::from_file(path)?
    } else {
        Sweep::from_csv(&args.rps_sweep)?
    };
    tracing::info!(target: "extenddb_bench", steps = ?sweep.steps, "sweep parsed");

    let workload = workload::build(&args.workload, &args.table_name, args.keyspace, args.item_size_bytes)?;

    let client_cfg = ClientConfig {
        endpoint_url: args.target.clone(),
        region: args.aws_region.clone(),
        tls_ca_bundle: args.tls_ca_bundle.clone(),
        tls_insecure: args.tls_insecure,
    };
    let client = client::build(&client_cfg).await?;

    let run_id = Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let output_dir = args
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("results").join(&run_id));
    std::fs::create_dir_all(&output_dir).with_context(|| format!("create {}", output_dir.display()))?;

    let mut meta = Meta {
        schema_version: output::SCHEMA_VERSION,
        run_id: run_id.clone(),
        started_at: Utc::now(),
        ended_at: None,
        extenddb_sha: args.extenddb_sha.clone(),
        bench_sha: option_env!("BENCH_GIT_SHA").map(str::to_string),
        workload: args.workload.clone(),
        rps_sweep: sweep.steps.clone(),
        duration_secs: args.duration.as_secs(),
        warmup_secs: args.warmup.as_secs(),
        cooldown_secs: args.cooldown.as_secs(),
        iterations: args.iterations,
        connections: args.connections,
        keyspace: args.keyspace,
        item_size_bytes: args.item_size_bytes,
        target: args.target.clone(),
        aws_region: args.aws_region.clone(),
        table_name: args.table_name.clone(),
    };
    output::write_meta(&output_dir, &meta)?;
    tracing::info!(target: "extenddb_bench", out = %output_dir.display(), "results dir created");

    let mut all_records: Vec<output::StepRecord> = Vec::new();

    'sweep: for (step_index, &target_rps) in sweep.steps.iter().enumerate() {
        ::metrics::gauge!(metrics::names::STEP_INDEX).set(step_index as f64);
        ::metrics::gauge!(metrics::names::TARGET_RPS).set(target_rps as f64);

        for iteration in 1..=args.iterations {
            ::metrics::gauge!(metrics::names::ITERATION_INDEX).set(iteration as f64);
            tracing::info!(
                target: "extenddb_bench",
                target_rps, iteration, "starting step"
            );

            let outcome = run_step(
                &client,
                workload.clone(),
                target_rps,
                iteration,
                args.warmup,
                args.duration,
                args.connections,
            )
            .await?;

            let pct = Percentiles::from_histogram(&outcome.histogram);
            let hgrm_name = format!("step-{target_rps:06}-iter-{iteration}.hgrm");
            write_hgrm(&outcome.histogram, &output_dir.join(&hgrm_name))
                .with_context(|| format!("write {hgrm_name}"))?;

            let record = output::build_step_record(
                target_rps,
                iteration,
                outcome.achieved_rps,
                outcome.successes,
                outcome.errors,
                pct,
                outcome.lg,
                hgrm_name,
            );
            tracing::info!(
                target: "extenddb_bench",
                target_rps, iteration,
                achieved_rps = outcome.achieved_rps,
                successes = outcome.successes,
                errors = outcome.errors,
                p50_us = pct.p50_us, p99_us = pct.p99_us, p999_us = pct.p999_us,
                lg_bottlenecked = outcome.lg.bottlenecked,
                "step done"
            );
            all_records.push(record);

            // Persist after every iteration so a crash at step 4/5 doesn't lose
            // the prior steps' data.
            output::write_sweep(&output_dir, &all_records)?;

            tokio::time::sleep(args.cooldown).await;
        }

        // Saturation check after iterations of the same step.
        if args.stop_at_saturation {
            let trip = all_records
                .iter()
                .rev()
                .take(args.iterations as usize)
                .all(|r| r.p99_us > 100_000 || r.error_rate > 0.01);
            if trip {
                tracing::warn!(
                    target: "extenddb_bench",
                    target_rps, "saturation tripped; stopping sweep"
                );
                break 'sweep;
            }
        }
    }

    let saturation = output::compute_saturation(&all_records);
    output::write_saturation(&output_dir, &saturation)?;
    meta.ended_at = Some(Utc::now());
    output::write_meta(&output_dir, &meta)?;
    output::write_summary(&output_dir, &meta, &all_records, &saturation)?;

    println!("results: {}", output_dir.display());
    println!("summary: {}", output_dir.join("summary.md").display());
    Ok(())
}

struct StepOutcome {
    histogram: Histogram<u64>,
    successes: u64,
    errors: u64,
    achieved_rps: f64,
    lg: crate::lg_health::LgHealthReport,
}

async fn run_step(
    client: &aws_sdk_dynamodb::Client,
    workload: Arc<dyn workload::Workload>,
    target_rps: u64,
    iteration: u32,
    warmup: Duration,
    duration: Duration,
    connections: u32,
) -> Result<StepOutcome> {
    let lg = LgHealth::new();
    let lg_handle = lg.spawn_sampler();

    // governor's RateLimiter::until_ready holds at most `burst` tokens.
    // We use the default burst (== rate per second) which gives the open-loop
    // pacing the design wants: the long-term rate is target_rps; transient
    // overshoot is allowed for at most one second's worth of work.
    let target_u32 = u32::try_from(target_rps).context("target_rps must fit in u32")?;
    let quota = Quota::per_second(NonZeroU32::new(target_u32).context("target_rps must be > 0")?);
    let limiter: Arc<Limiter> = Arc::new(RateLimiter::direct(quota));

    // warmup histogram is discarded; measure histogram is what we keep.
    let measure_hist: Arc<Mutex<Histogram<u64>>> = Arc::new(Mutex::new(new_latency_histogram()));
    let warmup_hist: Arc<Mutex<Histogram<u64>>> = Arc::new(Mutex::new(new_latency_histogram()));

    let successes = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let warmup_successes = Arc::new(AtomicU64::new(0));
    let warmup_errors = Arc::new(AtomicU64::new(0));
    let inflight = Arc::new(AtomicU64::new(0));

    let warmup_until = Instant::now() + warmup;
    let measure_start = warmup_until;
    let measure_until = measure_start + duration;

    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(connections as usize * 4);
    let mut next_seed: u64 = (iteration as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);

    loop {
        let now = Instant::now();
        if now >= measure_until {
            break;
        }
        // Open-loop pacing: wait for the next token to be available.
        limiter.until_ready().await;

        // LG safety belt: if too many tasks are in flight (e.g. SUT can't
        // keep up at high target RPS), drop new dispatches and count them
        // as overflow. This prevents the LG from spawning millions of
        // tokio tasks when the SUT is saturated. The achieved_rps will
        // drop below target, which is the correct signal.
        let max_inflight: u64 = (connections as u64).saturating_mul(8).max(256);
        if inflight.load(Ordering::Relaxed) >= max_inflight {
            errors.fetch_add(1, Ordering::Relaxed);
            ::metrics::counter!(metrics::names::ERRORS_TOTAL, "op" => "putitem", "reason" => "lg_overflow").increment(1);
            continue;
        }

        let in_warmup = now < warmup_until;
        let target_hist = if in_warmup {
            warmup_hist.clone()
        } else {
            measure_hist.clone()
        };
        let workload = workload.clone();
        let client = client.clone();
        let successes = if in_warmup { warmup_successes.clone() } else { successes.clone() };
        let errors = if in_warmup { warmup_errors.clone() } else { errors.clone() };
        let inflight = inflight.clone();
        next_seed = next_seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let seed = next_seed;
        let task = tokio::spawn(async move {
            inflight.fetch_add(1, Ordering::Relaxed);
            ::metrics::gauge!(metrics::names::INFLIGHT).set(inflight.load(Ordering::Relaxed) as f64);
            let outcome = workload.execute(&client, seed).await;
            inflight.fetch_sub(1, Ordering::Relaxed);
            match outcome {
                Ok(latency) => {
                    let micros = latency.as_micros() as u64;
                    let bucket = micros.clamp(1, 60_000_000);
                    if let Ok(mut h) = target_hist.lock() {
                        let _ = h.record(bucket);
                    }
                    successes.fetch_add(1, Ordering::Relaxed);
                    ::metrics::histogram!(metrics::names::REQUEST_DURATION, "op" => "putitem", "status" => "ok")
                        .record(latency.as_secs_f64());
                }
                Err(e) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    ::metrics::counter!(metrics::names::ERRORS_TOTAL, "op" => "putitem").increment(1);
                    tracing::debug!(target: "extenddb_bench", error = ?e, "request failed");
                }
            }
        });
        tasks.push(task);
    }

    // Cooldown: wait for outstanding requests up to a small grace window.
    let drain_deadline = Instant::now() + Duration::from_secs(30);
    while inflight.load(Ordering::Relaxed) > 0 && Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for t in tasks {
        let _ = t.await;
    }

    lg_handle.abort();
    let lg_report = lg.report();

    let final_hist: Histogram<u64> = std::mem::replace(
        &mut *measure_hist.lock().expect("measure hist poisoned"),
        new_latency_histogram(),
    );
    let successes_n = successes.load(Ordering::Relaxed);
    let errors_n = errors.load(Ordering::Relaxed);
    let achieved = (successes_n + errors_n) as f64 / duration.as_secs_f64();
    ::metrics::gauge!(metrics::names::ACHIEVED_RPS).set(achieved);

    Ok(StepOutcome {
        histogram: final_hist,
        successes: successes_n,
        errors: errors_n,
        achieved_rps: achieved,
        lg: lg_report,
    })
}
