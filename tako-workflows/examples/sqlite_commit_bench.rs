//! Local enqueue/claim/checkpoint/complete latency, including database commits.
//! cargo run -p tako-workflows --example sqlite_commit_bench

use std::time::Instant;
use tako_core::EnqueueOpts;
use tako_workflows::RunsDb;

fn main() {
    let temp = tempfile::tempdir().unwrap();
    let db = RunsDb::open(&temp.path().join("workflows.sqlite")).unwrap();
    let mut samples = Vec::new();
    for index in 0..256 {
        let start = Instant::now();
        let queued = db
            .enqueue(
                "work",
                &serde_json::json!({"index": index}),
                &EnqueueOpts::default(),
            )
            .unwrap();
        let claimed = db
            .claim("worker", &["work".into()], 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, queued.id);
        db.save_step(&claimed.id, "worker", "result", &serde_json::json!(index))
            .unwrap();
        db.complete(&claimed.id, "worker", &serde_json::Value::Null)
            .unwrap();
        samples.push(start.elapsed().as_micros());
    }
    samples.sort_unstable();
    println!(
        "{}",
        serde_json::json!({
            "samples": samples.len(), "workflow_cycle": {
                "p50_us": samples[samples.len() / 2],
                "p95_us": samples[samples.len() * 95 / 100],
                "max_us": samples.last(),
            }
        })
    );
}
