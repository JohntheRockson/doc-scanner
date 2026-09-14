//! Dev-only helper: downloads objects from the R2 bucket by key, WITHOUT deleting them,
//! purely for inspection. Not part of the shipped tool.
//!
//! Usage: cargo run --release --example r2_download -- <key1> [key2 ...]

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use std::env;
use std::path::PathBuf;

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run());
}

async fn run() {
    let _ = dotenvy::dotenv();
    let endpoint = env::var("R2_ENDPOINT_URL").expect("R2_ENDPOINT_URL");
    let access_key_id = env::var("R2_ACCESS_KEY_ID").expect("R2_ACCESS_KEY_ID");
    let secret_key = env::var("R2_SECRET_KEY").expect("R2_SECRET_KEY");
    let bucket = env::var("R2_BUCKET_NAME").expect("R2_BUCKET_NAME");

    let credentials = Credentials::new(&access_key_id, &secret_key, None, None, "r2-static");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(&endpoint)
        .region(Region::new("auto"))
        .credentials_provider(credentials)
        .build();
    let client = aws_sdk_s3::Client::from_conf(config);

    std::fs::create_dir_all("test_assets/bugcheck").expect("create dir");

    for key in env::args().skip(1) {
        let resp = client
            .get_object()
            .bucket(&bucket)
            .key(&key)
            .send()
            .await
            .expect("download");
        let bytes = resp.body.collect().await.expect("collect body").into_bytes();
        let out = PathBuf::from("test_assets/bugcheck").join(&key);
        std::fs::write(&out, &bytes).expect("write file");
        println!("Downloaded {key} ({} bytes) -> {}", bytes.len(), out.display());
    }
}
