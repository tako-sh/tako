#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export LITESTREAM_BINARY
LITESTREAM_BINARY="$(python3 scripts/upstream.py binary)"
upstream_source="$(python3 scripts/upstream.py source)"
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --release --all-features
(
  cd "$upstream_source"
  for attempt in 1 2 3; do
    if go list -deps -test . >/dev/null; then break; fi
    if [ "$attempt" = 3 ]; then exit 1; fi
    sleep 2
  done
  go test -race -count=1 -run '^(TestWALReader.*|TestDB_(Sync.*|NoLTXFilesOnIdleSync|DelayedCheckpointAfterWrite|Snapshot.*|CRC64|Checkpoint.*|MultipleCheckpointsWithWrites|IdleCheckpointSnapshotLoop|Issue994_RunawayDiskUsage|WALPageCoverage.*|WriteLTXFromWAL.*)|TestReplica_(Restore.*|CalcRestorePlan|CalcRestoreTarget)|TestSyncRestoreIntegrity.*)$' .
)
