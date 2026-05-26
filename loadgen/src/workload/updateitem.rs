//! `updateitem-1kb` workload: UpdateItem with `SET val = :v` over the pre-seeded keyspace.
//!
//! The new value is `salt + payload(key)` where the salt rotates per iteration.
//! This guarantees every UPDATE actually mutates bytes, so the SUT can't
//! short-circuit a write to a no-op (would invalidate the perf signal).

use std::time::{Duration, Instant};

use anyhow::Result;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client as DdbClient;

use super::Workload;
use crate::workload::putitem::PutItem1Kb;

pub struct UpdateItem1Kb {
    table_name: String,
    keyspace: u64,
    payload_size: usize,
}

impl UpdateItem1Kb {
    pub fn new(table_name: String, keyspace: u64, payload_size: usize) -> Self {
        Self { table_name, keyspace, payload_size }
    }

    fn payload_with_salt(&self, key: u64, salt: u64) -> String {
        // Reuse the deterministic payload generator and prefix with a 16-char
        // salt so consecutive updates write different bytes.
        let base = PutItem1Kb::payload(key, self.payload_size.saturating_sub(16));
        format!("{salt:016x}{base}")
    }
}

#[async_trait::async_trait]
impl Workload for UpdateItem1Kb {
    fn name(&self) -> &'static str {
        "updateitem-1kb"
    }

    fn requires_preseed(&self) -> bool {
        true
    }

    async fn execute(&self, client: &DdbClient, rng_seed: u64) -> Result<Duration> {
        let mut rng = fastrand::Rng::with_seed(rng_seed);
        let key = rng.u64(0..self.keyspace);
        // Salt is per-iteration: derive from the seed itself so each call is unique.
        let salt = rng_seed;
        let val = self.payload_with_salt(key, salt);
        let started = Instant::now();
        client
            .update_item()
            .table_name(&self.table_name)
            .key("pk", AttributeValue::S(format!("{key:012}")))
            .update_expression("SET #v = :v")
            .expression_attribute_names("#v", "val")
            .expression_attribute_values(":v", AttributeValue::S(val))
            .send()
            .await?;
        Ok(started.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salted_payloads_change() {
        let w = UpdateItem1Kb::new("bench".into(), 100, 256);
        assert_ne!(w.payload_with_salt(42, 1), w.payload_with_salt(42, 2));
        assert_eq!(w.payload_with_salt(42, 1).len(), 256);
    }
}
