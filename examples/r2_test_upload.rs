//! Dev-only helper (not part of the shipped tool): uploads local test files to the R2
//! bucket under `TEST_`-prefixed keys, purely so we can validate the real
//! download -> process -> delete round trip against the actual bucket without touching
//! any real photos. Safe to delete; not part of the product.
//!
//! Usage: cargo run --release --example r2_test_upload -- <file1> [file2 ...]

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use std::env;

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

    for path in env::args().skip(1) {
        let bytes = std::fs::read(&path).expect("read local file");
        let filename = std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy();
        let key = format!("TEST_{filename}");
        client
            .put_object()
            .bucket(&bucket)
            .key(&key)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .expect("upload");
        println!("Uploaded {path} -> r2://{bucket}/{key}");
    }
}
