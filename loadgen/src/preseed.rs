//! Pre-seed phase: idempotent PutItem fill of the bench keyspace.
//!
//! Deterministic by construction:
//!   - Key sweep is `[0, keyspace)`, partitioned across `connections` workers.
//!   - Payload is `payload_for(key, item_size_bytes)` -- same bytes per key forever.
//!   - An S3 stamp at `s3://<bucket>/preseed/<sha>/<keyspace>-<size>.done`
//!     records completion; re-runs return `Skipped` without firing a request.
//!
//! Open-loop pacing reuses governor with a rate cap of `preseed_rps`.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_s3::Client as S3Client;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use serde::{Deserialize, Serialize};

use crate::cli::PreseedArgs;
use crate::client::{self, ClientConfig};

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StampMeta {
    pub keyspace: u64,
    pub item_size_bytes: usize,
    pub extenddb_sha: Option<String>,
    pub started_at: String,
    pub ended_at: String,
    pub items_written: u64,
    pub achieved_rps: f64,
}

#[derive(Debug)]
pub enum Outcome {
    Seeded(StampMeta),
    Skipped(String),
}

/// Deterministic payload identical to the `putitem-1kb` workload's `payload`.
pub fn payload_for(key: u64, payload_size: usize) -> String {
    let mut s = String::with_capacity(payload_size);
    let chunk = format!("{key:016x}");
    while s.len() + chunk.len() <= payload_size {
        s.push_str(&chunk);
    }
    while s.len() < payload_size {
        s.push('=');
    }
    s
}

pub fn stamp_key(sha: Option<&str>, keyspace: u64, item_size_bytes: usize) -> String {
    let sha_part = sha.unwrap_or("unspecified");
    format!("preseed/{sha_part}/{keyspace}-{item_size_bytes}.done")
}

pub async fn run(args: PreseedArgs) -> Result<Outcome> {
    let s3_client = build_s3_client(&args.aws_region).await?;
    let key = stamp_key(args.extenddb_sha.as_deref(), args.keyspace, args.item_size_bytes);

    if !args.force {
        if let Some(bucket) = &args.stamp_bucket {
            if let Some(meta) = read_stamp(&s3_client, bucket, &key).await? {
                if meta.keyspace == args.keyspace && meta.item_size_bytes == args.item_size_bytes {
                    tracing::info!(
                        target: "extenddb_bench",
                        bucket = %bucket, key = %key,
                        items = meta.items_written, rps = meta.achieved_rps,
                        "preseed: skipped (stamp present)"
                    );
                    return Ok(Outcome::Skipped(format!("stamp s3://{bucket}/{key} present")));
                }
            }
        } else {
            tracing::warn!(target: "extenddb_bench", "no --stamp-bucket; preseed cannot be made idempotent");
        }
    }

    let started_at = chrono::Utc::now();
    let ddb_cfg = ClientConfig {
        endpoint_url: args.target.clone(),
        region: args.aws_region.clone(),
        tls_ca_bundle: args.tls_ca_bundle.clone(),
        tls_insecure: args.tls_insecure,
    };
    let ddb = client::build(&ddb_cfg).await?;

    let target_u32 = u32::try_from(args.preseed_rps).context("preseed_rps must fit in u32")?;
    let quota = Quota::per_second(NonZeroU32::new(target_u32).context("preseed_rps must be > 0")?);
    let limiter: Arc<Limiter> = Arc::new(RateLimiter::direct(quota));

    let written = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let inflight = Arc::new(AtomicU64::new(0));
    let max_inflight: u64 = (args.connections as u64).saturating_mul(8).max(256);

    let begin = Instant::now();
    let mut next_key: u64 = 0;
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(args.connections as usize * 4);

    while next_key < args.keyspace {
        // LG safety belt: same overflow protection as the main runner.
        if inflight.load(Ordering::Relaxed) >= max_inflight {
            tokio::task::yield_now().await;
            continue;
        }
        limiter.until_ready().await;

        let key = next_key;
        next_key += 1;
        let table = args.table_name.clone();
        let size = args.item_size_bytes;
        let written_c = written.clone();
        let errors_c = errors.clone();
        let inflight_c = inflight.clone();
        let ddb_c = ddb.clone();

        let task = tokio::spawn(async move {
            inflight_c.fetch_add(1, Ordering::Relaxed);
            let val = payload_for(key, size);
            let res = ddb_c
                .put_item()
                .table_name(&table)
                .item("pk", AttributeValue::S(format!("{key:012}")))
                .item("val", AttributeValue::S(val))
                .send()
                .await;
            inflight_c.fetch_sub(1, Ordering::Relaxed);
            match res {
                Ok(_) => {
                    written_c.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    errors_c.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(target: "extenddb_bench", key, error = ?e, "preseed put failed");
                }
            }
        });
        tasks.push(task);

        // Periodic progress.
        if next_key.is_multiple_of(50_000) {
            tracing::info!(
                target: "extenddb_bench",
                queued = next_key, keyspace = args.keyspace,
                written = written.load(Ordering::Relaxed),
                errors = errors.load(Ordering::Relaxed),
                "preseed progress"
            );
        }
    }

    // Drain.
    let drain_deadline = Instant::now() + Duration::from_secs(120);
    while inflight.load(Ordering::Relaxed) > 0 && Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for t in tasks {
        let _ = t.await;
    }

    let elapsed = begin.elapsed().as_secs_f64().max(0.001);
    let written_n = written.load(Ordering::Relaxed);
    let errors_n = errors.load(Ordering::Relaxed);
    let achieved = written_n as f64 / elapsed;
    let ended_at = chrono::Utc::now();

    if errors_n > 0 {
        anyhow::bail!(
            "preseed completed with {errors_n} errors out of {} attempts; not stamping",
            args.keyspace
        );
    }

    let meta = StampMeta {
        keyspace: args.keyspace,
        item_size_bytes: args.item_size_bytes,
        extenddb_sha: args.extenddb_sha.clone(),
        started_at: started_at.to_rfc3339(),
        ended_at: ended_at.to_rfc3339(),
        items_written: written_n,
        achieved_rps: achieved,
    };

    if let Some(bucket) = &args.stamp_bucket {
        write_stamp(&s3_client, bucket, &key, &meta).await?;
        tracing::info!(
            target: "extenddb_bench",
            bucket = %bucket, key = %key,
            items = meta.items_written, rps = meta.achieved_rps,
            "preseed: stamp written"
        );
    } else {
        tracing::warn!(
            target: "extenddb_bench",
            "preseed completed but no --stamp-bucket; next run will re-seed"
        );
    }

    Ok(Outcome::Seeded(meta))
}

async fn build_s3_client(region: &str) -> Result<S3Client> {
    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_owned()))
        .load()
        .await;
    Ok(S3Client::new(&cfg))
}

async fn read_stamp(s3: &S3Client, bucket: &str, key: &str) -> Result<Option<StampMeta>> {
    match s3.get_object().bucket(bucket).key(key).send().await {
        Ok(out) => {
            let bytes = out
                .body
                .collect()
                .await
                .context("read stamp body")?
                .into_bytes();
            let meta: StampMeta = serde_json::from_slice(&bytes).context("parse stamp body")?;
            Ok(Some(meta))
        }
        Err(e) => {
            // NoSuchKey is a normal not-present case.
            let svc = e.into_service_error();
            if svc.is_no_such_key() {
                Ok(None)
            } else {
                Err(anyhow::anyhow!("read stamp s3://{bucket}/{key}: {svc}"))
            }
        }
    }
}

async fn write_stamp(s3: &S3Client, bucket: &str, key: &str, meta: &StampMeta) -> Result<()> {
    let body = serde_json::to_vec_pretty(meta).context("serialize stamp")?;
    s3.put_object()
        .bucket(bucket)
        .key(key)
        .body(body.into())
        .content_type("application/json")
        .send()
        .await
        .with_context(|| format!("write stamp s3://{bucket}/{key}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_size_exact() {
        assert_eq!(payload_for(0, 1024).len(), 1024);
        assert_eq!(payload_for(123_456, 256).len(), 256);
    }

    #[test]
    fn payload_deterministic_per_key() {
        assert_eq!(payload_for(42, 512), payload_for(42, 512));
        assert_ne!(payload_for(42, 512), payload_for(43, 512));
    }

    #[test]
    fn stamp_key_is_sha_scoped() {
        let a = stamp_key(Some("aaaaaa"), 1_000_000, 1024);
        let b = stamp_key(Some("bbbbbb"), 1_000_000, 1024);
        assert_ne!(a, b);
        assert!(a.starts_with("preseed/aaaaaa/"));
    }

    #[test]
    fn stamp_key_handles_no_sha() {
        let k = stamp_key(None, 100, 8);
        assert_eq!(k, "preseed/unspecified/100-8.done");
    }
}
