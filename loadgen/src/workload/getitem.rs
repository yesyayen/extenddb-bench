//! `getitem-1kb` workload: GetItem against the pre-seeded keyspace.
//!
//! 100% hit rate by design: every key in `[0, keyspace)` was filled during
//! the pre-seed phase. A miss flags a pre-seed bug.

use std::time::{Duration, Instant};

use anyhow::Result;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client as DdbClient;

use super::Workload;

pub struct GetItem1Kb {
    table_name: String,
    keyspace: u64,
}

impl GetItem1Kb {
    pub fn new(table_name: String, keyspace: u64) -> Self {
        Self { table_name, keyspace }
    }
}

#[async_trait::async_trait]
impl Workload for GetItem1Kb {
    fn name(&self) -> &'static str {
        "getitem-1kb"
    }

    fn requires_preseed(&self) -> bool {
        true
    }

    async fn execute(&self, client: &DdbClient, rng_seed: u64) -> Result<Duration> {
        let mut rng = fastrand::Rng::with_seed(rng_seed);
        let key = rng.u64(0..self.keyspace);
        let started = Instant::now();
        let out = client
            .get_item()
            .table_name(&self.table_name)
            .key("pk", AttributeValue::S(format!("{key:012}")))
            .send()
            .await?;
        if out.item.is_none() || out.item.as_ref().is_some_and(|i| i.is_empty()) {
            // 100% hit invariant violated. Bump the loadgen miss counter.
            ::metrics::counter!(crate::metrics::names::READ_MISS_TOTAL).increment(1);
        }
        Ok(started.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_stable() {
        let w = GetItem1Kb::new("bench".into(), 100);
        assert_eq!(w.name(), "getitem-1kb");
        assert!(w.requires_preseed());
    }
}
