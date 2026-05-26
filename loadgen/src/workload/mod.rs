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
}

pub fn build(name: &str, table_name: &str, keyspace: u64, item_size_bytes: usize) -> Result<Arc<dyn Workload>> {
    match name {
        "putitem-1kb" => Ok(Arc::new(putitem::PutItem1Kb::new(
            table_name.to_string(),
            keyspace,
            item_size_bytes,
        ))),
        other => anyhow::bail!("unknown workload: {other:?} (only `putitem-1kb` is supported in v0.1)"),
    }
}
