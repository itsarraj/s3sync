use std::sync::Arc;

use anyhow::{Context, Result};
use object_store::aws::AmazonS3Builder;
use object_store::ObjectStore;

#[derive(Debug, Clone, Default)]
pub struct S3Config {
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    /// Needed for non-TLS local/self-hosted endpoints (MinIO, RustFS in a
    /// dev compose file without a cert) — real S3 itself is always HTTPS.
    pub allow_http: bool,
}

/// Builds a real `AmazonS3` client — works against AWS S3 itself and
/// anything speaking the same API (MinIO, RustFS, R2, Backblaze B2, ...)
/// via `endpoint`. Starts from `AmazonS3Builder::from_env()` so the usual
/// `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_ENDPOINT`/`AWS_REGION`
/// env vars work with zero flags, same convention every other AWS-adjacent
/// tool already uses; explicit fields here override the env var if set.
pub fn build_store(cfg: &S3Config) -> Result<Arc<dyn ObjectStore>> {
    let mut builder = AmazonS3Builder::from_env().with_bucket_name(&cfg.bucket);
    if let Some(endpoint) = &cfg.endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    if let Some(region) = &cfg.region {
        builder = builder.with_region(region);
    }
    if let Some(key) = &cfg.access_key_id {
        builder = builder.with_access_key_id(key);
    }
    if let Some(secret) = &cfg.secret_access_key {
        builder = builder.with_secret_access_key(secret);
    }
    if cfg.allow_http {
        builder = builder.with_allow_http(true);
    }
    let store = builder
        .build()
        .context("building S3 client (check bucket/endpoint/credentials)")?;
    Ok(Arc::new(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_without_a_network_call_given_explicit_config() {
        // `.build()` only validates configuration locally — no network
        // call happens until a request is actually issued, so this is a
        // legitimate offline test of the config-wiring path, not a live
        // connectivity check.
        let cfg = S3Config {
            bucket: "test-bucket".to_string(),
            endpoint: Some("http://localhost:9000".to_string()),
            region: Some("us-east-1".to_string()),
            access_key_id: Some("test-key".to_string()),
            secret_access_key: Some("test-secret".to_string()),
            allow_http: true,
        };
        assert!(build_store(&cfg).is_ok());
    }
}
