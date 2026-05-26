//! LG self-monitoring: CPU and RSS sampling at fixed intervals during a step.
//!
//! Used to flag steps where the load generator was the bottleneck. A step is
//! "LG-bottlenecked" if `cpu_user_pct >= 90` for >= 50% of the measure window.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tokio::time::interval;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const BOTTLENECK_THRESHOLD_PCT: f64 = 90.0;

#[derive(Debug, Default)]
struct Samples {
    cpu_pcts: Vec<f64>,
    rss_bytes_max: u64,
}

#[derive(Debug, Clone)]
pub struct LgHealth {
    inner: Arc<Mutex<Samples>>,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct LgHealthReport {
    pub cpu_p99_pct: f64,
    pub cpu_mean_pct: f64,
    pub rss_max_bytes: u64,
    pub bottlenecked: bool,
}

impl LgHealth {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(Samples::default())) }
    }

    /// Spawn a sampler task. Returns a `JoinHandle` that the caller aborts at end-of-step.
    pub fn spawn_sampler(&self) -> tokio::task::JoinHandle<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut sys = System::new_with_specifics(
                RefreshKind::new().with_processes(ProcessRefreshKind::new().with_cpu().with_memory()),
            );
            let pid = sysinfo::get_current_pid().ok();
            let cpu_count = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1) as f64;
            let mut tick = interval(SAMPLE_INTERVAL);
            loop {
                tick.tick().await;
                let pids = pid.map(|p| [p]).unwrap_or([sysinfo::Pid::from(0)]);
                let processes_to_update = if pid.is_some() {
                    ProcessesToUpdate::Some(&pids)
                } else {
                    ProcessesToUpdate::All
                };
                sys.refresh_processes_specifics(
                    processes_to_update,
                    ProcessRefreshKind::new().with_cpu().with_memory(),
                );
                let (cpu, rss) = if let Some(pid) = pid {
                    if let Some(p) = sys.process(pid) {
                        // sysinfo cpu_usage is per-core (e.g. 800% on 8 cores fully used).
                        // Normalize to a 0..100 single-CPU-equivalent percentage.
                        let cpu = (p.cpu_usage() as f64 / cpu_count).min(100.0 * cpu_count);
                        (cpu, p.memory())
                    } else {
                        (0.0, 0)
                    }
                } else {
                    (0.0, 0)
                };
                let mut s = inner.lock().expect("lg_health mutex poisoned");
                s.cpu_pcts.push(cpu);
                s.rss_bytes_max = s.rss_bytes_max.max(rss);
            }
        })
    }

    /// Stop sampling and produce a report.
    pub fn report(&self) -> LgHealthReport {
        let s = self.inner.lock().expect("lg_health mutex poisoned");
        if s.cpu_pcts.is_empty() {
            return LgHealthReport {
                cpu_p99_pct: 0.0,
                cpu_mean_pct: 0.0,
                rss_max_bytes: s.rss_bytes_max,
                bottlenecked: false,
            };
        }
        let mut sorted = s.cpu_pcts.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let p99_idx = ((n as f64) * 0.99).floor() as usize;
        let p99 = sorted[p99_idx.min(n - 1)];
        let mean = sorted.iter().sum::<f64>() / (n as f64);
        let above = sorted.iter().filter(|&&v| v >= BOTTLENECK_THRESHOLD_PCT).count();
        let bottlenecked = above as f64 / n as f64 >= 0.5;
        LgHealthReport {
            cpu_p99_pct: p99,
            cpu_mean_pct: mean,
            rss_max_bytes: s.rss_bytes_max,
            bottlenecked,
        }
    }
}
