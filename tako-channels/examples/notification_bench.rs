//! Local commit-to-replay measurements against an in-process database host;
//! no TLS or browser transport.
//! cargo run -p tako-channels --example notification_bench

use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tako_channels::{ChannelPublishPayload, ChannelStore, ChannelStoreConfig};
use tako_db::testing::TestServer;

fn summary(mut samples: Vec<u128>) -> serde_json::Value {
    samples.sort_unstable();
    serde_json::json!({
        "samples": samples.len(),
        "p50_us": samples[samples.len() / 2],
        "p95_us": samples[samples.len() * 95 / 100],
        "max_us": samples.last(),
    })
}

/// The hosted client blocks, so stores are opened off the async executor.
async fn open(config: ChannelStoreConfig) -> Arc<ChannelStore> {
    tokio::task::spawn_blocking(move || Arc::new(ChannelStore::open_config(config).unwrap()))
        .await
        .unwrap()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let server = TestServer::start();
    let database = server.database("bench#channels");
    let config = ChannelStoreConfig::hosted(&server.url, &database.token, "bench/production");
    let store = open(config.clone()).await;
    // A local writer wakes subscribers at commit; another server's writer is
    // seen through the store's head polling.
    for external in [false, true] {
        let writer = if external {
            open(config.clone()).await
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
