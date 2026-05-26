//! ExtendDB DynamoDB client construction.
//!
//! Wraps `aws-sdk-dynamodb` with:
//! - retries disabled (the bench measures raw response latency, not retry latency)
//! - a fixed endpoint URL (the SUT's HTTPS dataplane)
//! - SigV4 region from the CLI flag
//! - a custom hyper-rustls HTTP client that either:
//!     - trusts an operator-supplied PEM CA bundle via `--tls-ca-bundle`, or
//!     - skips verification entirely via `--tls-insecure`, or
//!     - falls back to OS root certs (rustls-native-certs) merged with the
//!       webpki-roots bundle.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use aws_config::retry::RetryConfig;
use aws_config::timeout::TimeoutConfig;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_dynamodb::Client as DdbClient;
use aws_smithy_runtime::client::http::hyper_014::HyperClientBuilder;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub endpoint_url: String,
    pub region: String,
    pub tls_ca_bundle: Option<std::path::PathBuf>,
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

    let connector = build_https_connector(cfg)?;
    let http_client = HyperClientBuilder::new().build(connector);

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
        .http_client(http_client)
        .load()
        .await;

    Ok(DdbClient::new(&sdk))
}

fn build_https_connector(
    cfg: &ClientConfig,
) -> Result<hyper_rustls::HttpsConnector<hyper::client::HttpConnector>> {
    let tls_config = if cfg.tls_insecure {
        tracing::warn!(target: "extenddb_bench", "TLS verification is DISABLED");
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoopVerifier::new()))
            .with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        if let Some(path) = &cfg.tls_ca_bundle {
            let added = load_ca_bundle(path, &mut roots)?;
            tracing::info!(target: "extenddb_bench", added, path = %path.display(), "loaded CA bundle");
        } else {
            // Fall back to OS native roots + webpki-roots (covers self-signed
            // certs added via update-ca-trust on AL2023, and public CAs).
            match rustls_native_certs::load_native_certs() {
                Ok(native) => {
                    for cert in native {
                        let _ = roots.add(cert);
                    }
                }
                Err(e) => tracing::warn!(target: "extenddb_bench", error = %e, "failed to load native certs; using webpki-roots only"),
            }
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };

    Ok(hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_all_versions()
        .build())
}

fn load_ca_bundle(path: &Path, roots: &mut RootCertStore) -> Result<usize> {
    let pem = std::fs::read(path)
        .with_context(|| format!("read CA bundle {}", path.display()))?;
    let mut reader = std::io::BufReader::new(pem.as_slice());
    let mut added = 0usize;
    for cert in rustls_pemfile::certs(&mut reader) {
        let cert = cert.with_context(|| format!("parse PEM cert in {}", path.display()))?;
        roots.add(cert).with_context(|| format!("add cert from {}", path.display()))?;
        added += 1;
    }
    anyhow::ensure!(added > 0, "no certificates found in {}", path.display());
    Ok(added)
}

#[derive(Debug)]
struct NoopVerifier {
    schemes: Vec<SignatureScheme>,
}

impl NoopVerifier {
    fn new() -> Self {
        Self {
            schemes: vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ED25519,
            ],
        }
    }
}

impl ServerCertVerifier for NoopVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}
