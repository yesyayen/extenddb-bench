//! `mixed-rw` workload: dispatches Get or Update by an `R:W` ratio.
//!
//! Mixed-rw runs against the pre-seeded keyspace (reads hit, updates rewrite).
//! Per-op latencies are tagged read/write at the metrics layer; the runner
//! collects two HDR histograms when `op_kind` is `Some`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use aws_sdk_dynamodb::Client as DdbClient;

use super::Workload;
use crate::workload::getitem::GetItem1Kb;
use crate::workload::updateitem::UpdateItem1Kb;

pub struct MixedRw {
    read: GetItem1Kb,
    write: UpdateItem1Kb,
    /// 0..=100; reads percentage.
    read_pct: u8,
}

impl MixedRw {
    pub fn new(table_name: String, keyspace: u64, payload_size: usize, rw_ratio: &str) -> Result<Self> {
        let read_pct = parse_ratio(rw_ratio)?;
        Ok(Self {
            read: GetItem1Kb::new(table_name.clone(), keyspace),
            write: UpdateItem1Kb::new(table_name, keyspace, payload_size),
            read_pct,
        })
    }

    /// Pure-function decision: read on this seed?
    fn pick_read(&self, rng_seed: u64) -> bool {
        // Stable derivation from seed -> [0, 100).
        let bucket = (rng_seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32 % 100;
        bucket < self.read_pct as u32
    }
}

#[async_trait::async_trait]
impl Workload for MixedRw {
    fn name(&self) -> &'static str {
        "mixed-rw"
    }

    fn requires_preseed(&self) -> bool {
        true
    }

    fn op_kind(&self, rng_seed: u64) -> Option<&'static str> {
        Some(if self.pick_read(rng_seed) { "read" } else { "write" })
    }

    async fn execute(&self, client: &DdbClient, rng_seed: u64) -> Result<Duration> {
        let started = Instant::now();
        if self.pick_read(rng_seed) {
            self.read.execute(client, rng_seed).await?;
        } else {
            self.write.execute(client, rng_seed).await?;
        }
        Ok(started.elapsed())
    }
}

fn parse_ratio(s: &str) -> Result<u8> {
    let mut it = s.split(':');
    let r: u32 = it
        .next()
        .with_context(|| format!("rw_ratio missing R: {s:?}"))?
        .trim()
        .parse()
        .with_context(|| format!("rw_ratio R parse failed: {s:?}"))?;
    let w: u32 = it
        .next()
        .with_context(|| format!("rw_ratio missing W: {s:?}"))?
        .trim()
        .parse()
        .with_context(|| format!("rw_ratio W parse failed: {s:?}"))?;
    if it.next().is_some() {
        anyhow::bail!("rw_ratio must be R:W (got {s:?})");
    }
    if r + w != 100 {
        anyhow::bail!("rw_ratio R:W must sum to 100 (got {r}:{w})");
    }
    Ok(r as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ratio_ok() {
        assert_eq!(parse_ratio("80:20").unwrap(), 80);
        assert_eq!(parse_ratio("50:50").unwrap(), 50);
        assert_eq!(parse_ratio("100:0").unwrap(), 100);
    }

    #[test]
    fn parse_ratio_rejects_bad_sum() {
        assert!(parse_ratio("80:30").is_err());
        assert!(parse_ratio("80").is_err());
        assert!(parse_ratio("80:20:0").is_err());
    }

    #[test]
    fn pick_read_split_is_roughly_correct() {
        let m = MixedRw::new("bench".into(), 100, 256, "80:20").unwrap();
        let n = 10_000u64;
        let reads = (0..n).filter(|i| m.pick_read(*i)).count() as f64;
        let pct = reads * 100.0 / n as f64;
        assert!(
            (75.0..=85.0).contains(&pct),
            "read pct {pct:.1}% not within +-5 of 80%"
        );
    }
}
