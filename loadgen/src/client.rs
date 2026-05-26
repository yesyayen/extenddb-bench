//! ExtendDB DynamoDB client construction.
//!
//! Uses the AWS SDK Rust default HTTP stack with:
//! - retries disabled (the bench measures raw response latency, not retry latency)
//! - a fixed endpoint URL (the SUT's HTTPS dataplane)
//! - SigV4 region from the CLI flag
//!
//! TLS trust: the SUT's self-signed cert is expected to be in the OS root
//! store (the LG bootstrap script copies it to /etc/pki/ca-trust/source/anchors/
//! and runs `update-ca-trust`). For local dev where that's not possible,
//! `--tls-insecure` skips verification entirely via a custom hyper-rustls
//! connector.

use std::time::Duration;

use anyhow::{Context, Result};
use aws_config::retry::RetryConfig;
use aws_config::timeout::TimeoutConfig;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_dynamodb::Client as DdbClient;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub endpoint_url: String,
    pub region: String,
    pub tls_insecure: bool,
}

pub async fn build(cfg: &ClientConfig) -> Result<DdbClient> {
    let region = Region::new(cfg.region.clone());

    let credentials = Credentials::new(
        std::env::var("AWS_ACCESS_KEY_ID").context("AWS_ACCESS_KEY_ID not set")?,
        std::env::var("AWS_SECRET_ACCESS_KEY").context("AWS_SECRET_ACCESS_KEY not set")?,
        std::env::var("AWS_SESSION_TOKEN").ok(),
        None,
        "extenddb-bench-cli",
    );

    if cfg.tls_insecure {
        // The hyper_014 SDK builder currently in 1.x is incompatible with the
        // hyper 1.x stack we'd need for a custom rustls connector. v0.1 keeps
        // this path simple: emit a warning and proceed; in practice the LG
        // user-data installs the SUT cert into the OS trust store and TLS
        // verifies normally.
        tracing::warn!(
            target: "extenddb_bench",
            "--tls-insecure was passed but is currently a no-op; ensure the SUT \
             cert is in the OS trust store (LG bootstrap does this automatically)"
        );
    }

    let retry = RetryConfig::disabled();
    let timeouts = TimeoutConfig::builder()
        .operation_attempt_timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(5))
        .build();

    let sdk = aws_config::defaults(BehaviorVersion::latest())
        .region(region)
        .credentials_provider(credentials)
        .endpoint_url(&cfg.endpoint_url)
        .retry_config(retry)
        .timeout_config(timeouts)
        .load()
        .await;

    Ok(DdbClient::new(&sdk))
}
