//! `putitem-1kb` workload: PutItem with a 1 KB item, uniform random key over [0, keyspace).

use std::time::{Duration, Instant};

use anyhow::Result;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client as DdbClient;

use super::Workload;

pub struct PutItem1Kb {
    table_name: String,
    keyspace: u64,
    payload_size: usize,
}

impl PutItem1Kb {
    pub fn new(table_name: String, keyspace: u64, payload_size: usize) -> Self {
        Self { table_name, keyspace, payload_size }
    }

    /// Deterministic payload-from-key. Lets the SUT cache compress the
    /// payload distribution stably across iterations.
    fn payload(&self, key: u64) -> String {
        let mut s = String::with_capacity(self.payload_size);
        let chunk = format!("{key:016x}");
        while s.len() + chunk.len() <= self.payload_size {
            s.push_str(&chunk);
        }
        while s.len() < self.payload_size {
            s.push('=');
        }
        s
    }
}

#[async_trait::async_trait]
impl Workload for PutItem1Kb {
    fn name(&self) -> &'static str {
        "putitem-1kb"
    }

    async fn execute(&self, client: &DdbClient, rng_seed: u64) -> Result<Duration> {
        let mut rng = fastrand::Rng::with_seed(rng_seed);
        let key = rng.u64(0..self.keyspace);
        let val = self.payload(key);
        let started = Instant::now();
        client
            .put_item()
            .table_name(&self.table_name)
            .item("pk", AttributeValue::S(format!("{key:012}")))
            .item("val", AttributeValue::S(val))
            .send()
            .await?;
        Ok(started.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_exact_size() {
        let w = PutItem1Kb::new("bench".into(), 100, 1024);
        for k in 0..10 {
            assert_eq!(w.payload(k).len(), 1024);
        }
    }

    #[test]
    fn payload_is_deterministic_per_key() {
        let w = PutItem1Kb::new("bench".into(), 100, 256);
        assert_eq!(w.payload(42), w.payload(42));
        assert_ne!(w.payload(42), w.payload(43));
    }
}
