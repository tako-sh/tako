//! Sustained queue churn, with and without a pinned MVCC horizon.
//! See ../POSTGRES_BENCHMARK.md. Only run against a disposable database.

use postgres::{Client, NoTls};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tako_core::EnqueueOpts;
use tako_workflows::{RunsDb, WorkflowStoreConfig};

const APP: &str = "queue-benchmark";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn setting(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .map(|s| s.parse().expect("invalid benchmark setting"))
        .unwrap_or(default)
}

#[derive(Clone, Default)]
struct Timings {
    claim: Vec<u64>,
    heartbeat: Vec<u64>,
    reclaim: Vec<u64>,
    enqueued: u64,
    completed: u64,
}

impl Timings {
    fn merge(&mut self, mut other: Self) {
        self.claim.append(&mut other.claim);
        self.heartbeat.append(&mut other.heartbeat);
        self.reclaim.append(&mut other.reclaim);
        self.enqueued += other.enqueued;
        self.completed += other.completed;
    }
}

fn latency(samples: &[u64]) -> serde_json::Value {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    if sorted.is_empty() {
        return json!({"count": 0});
    }
    json!({"count": sorted.len(), "p50_us": sorted[(sorted.len()-1)/2],
        "p95_us": sorted[(sorted.len()-1)*95/100], "max_us": sorted.last().unwrap()})
}

struct SchemaGuard<'a> {
    url: &'a str,
    name: String,
}

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Drop for SchemaGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut client) = Client::connect(self.url, NoTls) {
            let _ = client.batch_execute(&format!("DROP SCHEMA {} CASCADE", self.name));
        }
    }
}

fn run(url: &str, index: &str, held_xmin: bool) {
    let seconds = setting("BENCH_SECONDS", 30);
    let recovery_seconds = setting("BENCH_RECOVERY_SECONDS", 30);
    let rate = setting("BENCH_JOBS_PER_SECOND", 200);
    let long_jobs = setting("BENCH_LONG_JOBS", 128);
    let heartbeat_ms = setting("BENCH_HEARTBEAT_MS", 200);
    let workers = setting("BENCH_WORKERS", 4);
    let history = setting("BENCH_HISTORY", 10_000) as i64;
    assert!(seconds > 0 && recovery_seconds > 0 && rate > 0 && heartbeat_ms > 0 && workers > 0);
    let schema = SchemaGuard {
        url,
        name: format!("queue_bench_{}_{}", std::process::id(), now_ms()),
    };
    let open = || {
        RunsDb::open_config(WorkflowStoreConfig::Postgres {
            url: url.to_string(),
            schema: schema.name.clone(),
            app_id: APP.to_string(),
        })
        .unwrap()
    };
    // Open all stores before selecting the index variant: initialization may migrate indexes.
    let producer = open();
    let consumers: Vec<_> = (0..workers).map(|_| open()).collect();
    let heartbeater = open();
    let maintenance = open();
    let mut client = Client::connect(url, NoTls).unwrap();
    client.batch_execute(&format!(
        "DROP INDEX IF EXISTS {s}.idx_runs_app_lease;
         DROP INDEX IF EXISTS {s}.idx_runs_app_running_lease;
         ALTER TABLE {s}.runs SET (autovacuum_vacuum_threshold=50, autovacuum_vacuum_scale_factor=0.01);",
        s = schema.name,
    )).unwrap();
    if index != "unindexed" {
        client
            .batch_execute(&format!(
                "CREATE INDEX idx_runs_app_lease ON {}.runs(app_id, lease_until) {}",
                schema.name,
                if index == "partial" {
                    "WHERE status='running'"
                } else {
                    ""
                },
            ))
            .unwrap();
    }
    client
        .execute(
            &format!(
                "INSERT INTO {}.runs
         (app_id,id,name,payload,status,attempts,max_attempts,run_at,created_at,finished_at)
         SELECT $1,'history-' || n,'short','{{}}','succeeded',1,3,0,0,$2
         FROM generate_series(1,$3::bigint) n",
                schema.name,
            ),
            &[&APP, &(now_ms() + 86_400_000), &history],
        )
        .unwrap();
    let mut long_ids = Vec::new();
    for _ in 0..long_jobs {
        heartbeater
            .enqueue("long", &json!({}), &EnqueueOpts::default())
            .unwrap();
        long_ids.push(
            heartbeater
                .claim("heartbeat-worker", &["long".into()], 60_000)
                .unwrap()
                .unwrap()
                .id,
        );
    }
    client
        .batch_execute(&format!("VACUUM ANALYZE {}.runs", schema.name))
        .unwrap();
    let mut holder = Client::connect(url, NoTls).unwrap();
    if held_xmin {
        holder
            .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .unwrap();
        // Establish a snapshot; BEGIN alone does not pin xmin.
        holder
            .query_one(&format!("SELECT count(*) FROM {}.runs", schema.name), &[])
            .unwrap();
    }
    let mut wal_start: String = client
        .query_one("SELECT pg_current_wal_insert_lsn()::text", &[])
        .unwrap()
        .get(0);
    let stop = AtomicBool::new(false);
    let timings = Mutex::new(Timings::default());
    let start = Instant::now();
    let mut phase_totals = Timings::default();
    let recovery_phase = if held_xmin {
        "recovery"
    } else {
        "recovery_control"
    };
    println!(
        "{}",
        json!({"event":"start", "lease_index":index, "held_xmin":held_xmin,
        "seconds":seconds, "recovery_seconds":recovery_seconds, "jobs_per_second":rate, "long_jobs":long_jobs,
        "heartbeat_ms":heartbeat_ms, "workers":workers, "history":history})
    );
    thread::scope(|scope| {
        // Stop workers even if sampling or maintenance fails, so scope can join them.
        let _stop_on_drop = StopOnDrop(&stop);
        let stop_ref = &stop;
        let timings_ref = &timings;
        scope.spawn(move || {
            let interval = Duration::from_secs_f64(1.0 / rate as f64);
            let mut next = Instant::now();
            while !stop_ref.load(Ordering::Relaxed) {
                producer
                    .enqueue(
                        "short",
                        &json!({"payload":"x".repeat(256)}),
                        &EnqueueOpts::default(),
                    )
                    .unwrap();
                timings_ref.lock().unwrap().enqueued += 1;
                next += interval;
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        });
        for (i, db) in consumers.into_iter().enumerate() {
            scope.spawn(move || {
                let worker = format!("worker-{i}");
                let names = vec!["short".into()];
                while !stop_ref.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    if let Some(run) = db.claim(&worker, &names, 60_000).unwrap() {
                        timings_ref
                            .lock()
                            .unwrap()
                            .claim
                            .push(t.elapsed().as_micros() as u64);
                        db.complete(&run.id, &worker).unwrap();
                        timings_ref.lock().unwrap().completed += 1;
                    } else {
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            });
        }
        scope.spawn(move || {
            while !stop_ref.load(Ordering::Relaxed) {
                let round = Instant::now();
                for id in &long_ids {
                    if stop_ref.load(Ordering::Relaxed) {
                        break;
                    }
                    let t = Instant::now();
                    heartbeater
                        .heartbeat(id, "heartbeat-worker", 60_000)
                        .unwrap();
                    timings_ref
                        .lock()
                        .unwrap()
                        .heartbeat
                        .push(t.elapsed().as_micros() as u64);
                }
                thread::sleep(Duration::from_millis(heartbeat_ms).saturating_sub(round.elapsed()));
            }
        });
        for (phase, duration) in [
            (if held_xmin { "held" } else { "normal" }, seconds),
            (recovery_phase, recovery_seconds),
        ] {
            if phase == recovery_phase {
                // Keep producer/consumers/heartbeats alive while autovacuum catches up.
                // No manual VACUUM here: this measures operational recovery.
                if held_xmin {
                    holder.batch_execute("ROLLBACK").unwrap();
                }
                println!(
                    "{}",
                    json!({"event":"recovery_started", "lease_index":index,
                    "snapshot_released":held_xmin, "elapsed_seconds":start.elapsed().as_secs_f64()})
                );
                wal_start = client
                    .query_one("SELECT pg_current_wal_insert_lsn()::text", &[])
                    .unwrap()
                    .get(0);
                phase_totals = Timings::default();
            }
            let phase_start = Instant::now();
            for second in 1..=duration {
                thread::sleep(
                    (phase_start + Duration::from_secs(second))
                        .saturating_duration_since(Instant::now()),
                );
                let t = Instant::now();
                maintenance.reclaim_expired().unwrap();
                timings
                    .lock()
                    .unwrap()
                    .reclaim
                    .push(t.elapsed().as_micros() as u64);
                // Accelerated retention: remove short jobs after two seconds, retain seeded history.
                maintenance
                    .prune_finished_before(now_ms() - 2_000, rate as u32 * 2)
                    .unwrap();
                let window = std::mem::take(&mut *timings.lock().unwrap());
                report(
                    &mut client,
                    &schema.name,
                    index,
                    phase,
                    start.elapsed().as_secs_f64(),
                    second as f64,
                    &window,
                    &wal_start,
                    "sample",
                );
                phase_totals.merge(window);
            }
            if phase != recovery_phase {
                report(
                    &mut client,
                    &schema.name,
                    index,
                    phase,
                    start.elapsed().as_secs_f64(),
                    phase_start.elapsed().as_secs_f64(),
                    &phase_totals,
                    &wal_start,
                    "summary",
                );
            }
        }
        stop.store(true, Ordering::Relaxed);
    });
    drop(maintenance);
    phase_totals.merge(std::mem::take(&mut *timings.lock().unwrap()));
    // Closed worker connections flush their stats; refresh this connection's snapshot.
    client
        .batch_execute("SELECT pg_stat_clear_snapshot()")
        .unwrap();
    report(
        &mut client,
        &schema.name,
        index,
        recovery_phase,
        start.elapsed().as_secs_f64(),
        recovery_seconds as f64,
        &phase_totals,
        &wal_start,
        "summary",
    );
}

#[allow(clippy::too_many_arguments)]
fn report(
    client: &mut Client,
    schema: &str,
    index: &str,
    phase: &str,
    elapsed: f64,
    phase_elapsed: f64,
    t: &Timings,
    wal_start: &str,
    event: &str,
) {
    let row = client
        .query_one(
            "SELECT n_dead_tup,n_tup_upd,n_tup_hot_upd,autovacuum_count,
                pg_table_size(relid),pg_indexes_size(relid),
                pg_wal_lsn_diff(pg_current_wal_insert_lsn(),$2::text::pg_lsn)::float8
         FROM pg_stat_user_tables WHERE schemaname=$1 AND relname='runs'",
            &[&schema, &wal_start],
        )
        .unwrap();
    println!(
        "{}",
        json!({"event":event, "phase":phase, "phase_elapsed_seconds":phase_elapsed,
        "lease_index":index,"held_xmin":phase=="held","elapsed_seconds":elapsed,
        "enqueued":t.enqueued,"completed":t.completed,"claim":latency(&t.claim),
        "heartbeat":latency(&t.heartbeat),"reclaim":latency(&t.reclaim),
        "dead_tuples":row.get::<_,i64>(0),"updates":row.get::<_,i64>(1),
        "hot_updates":row.get::<_,i64>(2),"autovacuums":row.get::<_,i64>(3),
        "table_bytes":row.get::<_,i64>(4),"index_bytes":row.get::<_,i64>(5),
        "wal_bytes":row.get::<_,f64>(6)})
    );
}

fn main() {
    let url = std::env::var("TAKO_TEST_POSTGRES_URL")
        .expect("set TAKO_TEST_POSTGRES_URL to a disposable database");
    let indexes = match std::env::var("BENCH_INDEX").as_deref().unwrap_or("all") {
        "indexed" => vec!["indexed"],
        "partial" => vec!["partial"],
        "unindexed" => vec!["unindexed"],
        "all" => vec!["indexed", "partial", "unindexed"],
        _ => panic!("BENCH_INDEX must be indexed, partial, unindexed, or all"),
    };
    let horizons = match std::env::var("BENCH_HORIZON").as_deref().unwrap_or("both") {
        "normal" => vec![false],
        "held" => vec![true],
        "both" => vec![false, true],
        _ => panic!("BENCH_HORIZON must be normal, held, or both"),
    };
    for index in indexes {
        for &held_xmin in &horizons {
            run(&url, index, held_xmin);
        }
    }
}
