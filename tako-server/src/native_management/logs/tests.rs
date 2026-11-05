use super::*;
use crate::tls::{CertManager, CertManagerConfig};
use std::collections::HashMap;
use std::io::Write;

fn fixture() -> (tempfile::TempDir, Arc<ServerState>) {
    std::thread::spawn(|| {
        let dir = tempfile::tempdir().unwrap();
        let certs = Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: dir.path().join("certs"),
            ..Default::default()
        }));
        let state = ServerState::new_with_runtime(
            dir.path().into(),
            certs,
            None,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            crate::ServerRuntimeConfig::for_defaults(dir.path().into()),
        )
        .unwrap();
        (dir, Arc::new(state))
    })
    .join()
    .unwrap()
}

fn old_log(path: &std::path::Path, bytes: usize) -> std::fs::File {
    let mut file = std::fs::File::create(path).unwrap();
    let mut line = vec![b'x'; 1024];
    line[..19 + 1].copy_from_slice(b"2020-01-01T00:00:00 ");
    *line.last_mut().unwrap() = b'\n';
    let chunk = line.repeat(64);
    for _ in 0..bytes / chunk.len() {
        file.write_all(&chunk).unwrap();
    }
    file
}

fn metadata() -> Logs {
    Logs {
        app: "demo/production".into(),
        previous: 0,
        current: 0,
        since: Some(1_735_689_600),
        max_bytes: 128,
    }
}

#[test]
fn since_scans_normal_retention_and_returns_only_recent_tail_or_empty() {
    let (dir, state) = fixture();
    let logs = dir.path().join("apps/demo/production/logs");
    std::fs::create_dir_all(&logs).unwrap();
    old_log(&logs.join("previous.log"), 10 * 1024 * 1024);
    let mut current = old_log(&logs.join("current.log"), 8 * 1024 * 1024);
    let result = read(&state, &metadata()).unwrap();
    assert!(result.bytes.is_empty());
    assert!(!result.truncated);
    current.write_all(b"2026-01-01T00:00:00 recent\n").unwrap();
    let result = read(&state, &metadata()).unwrap();
    assert_eq!(result.bytes, b"2026-01-01T00:00:00 recent\n");
    assert!(!result.truncated);
    assert_eq!(result.current_offset, result.current_len);
}

#[test]
fn oversized_history_is_bounded_and_reported_as_truncated_not_invalid() {
    let (dir, state) = fixture();
    let logs = dir.path().join("apps/demo/production/logs");
    std::fs::create_dir_all(&logs).unwrap();
    old_log(&logs.join("previous.log"), MAX_SCAN_BYTES);
    old_log(&logs.join("current.log"), SCAN_CHUNK);
    let result = read(&state, &metadata()).unwrap();
    assert!(result.truncated);
    assert!(result.bytes.is_empty());
}
