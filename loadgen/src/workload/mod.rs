//! Workload trait + factory for v0.1.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use aws_sdk_dynamodb::Client as DdbClient;

pub mod putitem;

/// A workload knows how to issue exactly one operation given a worker-local RNG.
#[async_trait::async_trait]
pub trait Workload: Send + Sync + 'static {
    /// Friendly name (used in metric labels and meta.json).
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
    /// Execute one operation; return the wire latency on success.
    async fn execute(&self, client: &DdbClient, rng_seed: u64) -> Result<Duration>;
    /// Whether this workload requires the bench keyspace to be pre-seeded
    /// (PutItem fill of `[0, keyspace)`) before the sweep starts.
    fn requires_preseed(&self) -> bool {
        false
    }
    /// If the workload exposes a per-op kind ("read"/"write"), call this to
    /// translate the seed into the kind. Default: a single op kind.
    /// Used by the runner to split HDR histograms in mixed workloads.
    #[allow(dead_code)]
    fn op_kind(&self, _rng_seed: u64) -> Option<&'static str> {
        None
    }
}

pub fn build(name: &str, table_name: &str, keyspace: u64, item_size_bytes: usize, rw_ratio: &str) -> Result<Arc<dyn Workload>> {
    let _ = rw_ratio; // unused for non-mixed workloads at this milestone
    match name {
        "putitem-1kb" => Ok(Arc::new(putitem::PutItem1Kb::new(
            table_name.to_string(),
            keyspace,
            item_size_bytes,
        ))),
        other => anyhow::bail!("unknown workload: {other:?} (only `putitem-1kb` is wired in M1; reads/updates land in M2)"),
    }
}
