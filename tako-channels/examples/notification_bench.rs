//! Local commit-to-replay measurements; no HTTP, TLS, or browser transport.
//! cargo run -p tako-channels --example notification_bench

use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tako_channels::{ChannelPublishPayload, ChannelStore};

fn summary(mut samples: Vec<u128>) -> serde_json::Value {
    samples.sort_unstable();
    serde_json::json!({
        "samples": samples.len(),
        "p50_us": samples[samples.len() / 2],
        "p95_us": samples[samples.len() * 95 / 100],
        "max_us": samples.last(),
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("channels.sqlite");
    let store = Arc::new(ChannelStore::open(&path).unwrap());
    for external in [false, true] {
        let writer = if external {
            Arc::new(ChannelStore::open(&path).unwrap())
        } else {
            store.clone()
        };
        let channel = if external { "external" } else { "local" };
        let mut changes = store.changes(channel);
        let mut after = None;
        let mut latency = Vec::new();
        for index in 0..32 {
            changes.borrow_and_update();
            let start = Instant::now();
            let message = writer
                .append_async(
                    channel,
                    &ChannelPublishPayload {
                        r#type: "event".into(),
                        data: index.into(),
                    },
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), changes.changed())
                .await
                .unwrap()
                .unwrap();
            let records = store.read_after_async(channel, after, 100).await.unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].id, message.id);
            latency.push(start.elapsed().as_micros());
            after = message.id.parse().ok();
        }
        println!(
            "{}",
            serde_json::json!({"external_writer": external, "commit_to_replay": summary(latency)})
        );
    }
}
