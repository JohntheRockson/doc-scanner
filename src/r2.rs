//! Cloudflare R2 access via the S3-compatible API.
//!
//! R2 is plain HTTPS + the S3 REST protocol - no WASM, no Cloudflare Worker, and no
//! browser involved. We use the official `aws-sdk-s3` crate (Cloudflare's own docs
//! recommend it for Rust: <https://developers.cloudflare.com/r2/examples/aws/aws-sdk-rust/>)
//! pointed at R2's endpoint instead of AWS, with static credentials from `.env`. The rest
//! of the program stays fully synchronous; only these calls run on a small Tokio runtime.

use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use std::time::SystemTime;

/// R2 credentials/config, read from the process environment (populated from `.env` at
/// startup). Kept separate from `Client` construction so we can give a clear error
/// naming exactly which variable is missing, instead of a generic SDK failure.
pub struct R2Config {
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_key: String,
    pub bucket: String,
}

impl R2Config {
    pub fn from_env() -> Result<Self> {
        let get = |name: &str| {
            std::env::var(name).with_context(|| {
                format!(
                    "missing {name} (set it in .env, or pass --local to scan local files instead)"
                )
            })
        };
        Ok(Self {
            endpoint: get("R2_ENDPOINT_URL")?,
            access_key_id: get("R2_ACCESS_KEY_ID")?,
            secret_key: get("R2_SECRET_KEY")?,
            bucket: get("R2_BUCKET_NAME")?,
        })
    }
}

pub fn build_client(cfg: &R2Config) -> Client {
    let credentials =
        Credentials::new(&cfg.access_key_id, &cfg.secret_key, None, None, "r2-static");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(&cfg.endpoint)
        .region(Region::new("auto"))
        .credentials_provider(credentials)
        .build();
    Client::from_conf(config)
}

#[derive(Debug, Clone)]
pub struct RemoteObject {
    pub key: String,
    pub size: i64,
    pub last_modified: Option<SystemTime>,
}

/// Lists every object in the bucket (optionally under `prefix`), following pagination.
pub async fn list_objects(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
) -> Result<Vec<RemoteObject>> {
    let mut out = Vec::new();
    let mut continuation_token: Option<String> = None;

    loop {
        let mut req = client.list_objects_v2().bucket(bucket);
        if let Some(p) = prefix {
            req = req.prefix(p);
        }
        if let Some(token) = &continuation_token {
            req = req.continuation_token(token);
        }

        let resp = req
            .send()
            .await
            .context("listing objects in the R2 bucket")?;

        for obj in resp.contents() {
            let Some(key) = obj.key() else { continue };
            out.push(RemoteObject {
                key: key.to_string(),
                size: obj.size().unwrap_or(0),
                last_modified: obj.last_modified().and_then(|dt| (*dt).try_into().ok()),
            });
        }

        if resp.is_truncated().unwrap_or(false) {
            continuation_token = resp.next_continuation_token().map(str::to_string);
            if continuation_token.is_none() {
                break;
            }
        } else {
            break;
        }
    }

    Ok(out)
}

/// Downloads one object's full contents into memory.
pub async fn download_object(client: &Client, bucket: &str, key: &str) -> Result<Vec<u8>> {
    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .with_context(|| format!("downloading '{key}' from R2"))?;

    let bytes = resp
        .body
        .collect()
        .await
        .with_context(|| format!("reading the body of '{key}' from R2"))?
        .into_bytes();

    Ok(bytes.to_vec())
}

/// Deletes one object. Only ever called after that object's photo has been
/// successfully folded into the output PDF.
pub async fn delete_object(client: &Client, bucket: &str, key: &str) -> Result<()> {
    client
        .delete_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .with_context(|| format!("deleting '{key}' from R2"))?;
    Ok(())
}
