"""Measure transaction amortization, excluding queueing and application overhead."""

import argparse
import json
import sqlite3
import tempfile
import time
from pathlib import Path


def run(records: int, synchronous: str, batch: int) -> dict:
    with tempfile.TemporaryDirectory() as directory:
        connection = sqlite3.connect(Path(directory) / "log.sqlite")
        connection.execute("PRAGMA journal_mode=WAL")
        connection.execute(f"PRAGMA synchronous={synchronous}")
        connection.execute("CREATE TABLE messages (id INTEGER PRIMARY KEY, payload TEXT)")
        payload = "x" * 256
        samples = []
        start = time.perf_counter_ns()
        for offset in range(0, records, batch):
            before = time.perf_counter_ns()
            with connection:
                connection.executemany(
                    "INSERT INTO messages VALUES (?, ?)",
                    ((i, payload) for i in range(offset, min(offset + batch, records))),
                )
            samples.append((time.perf_counter_ns() - before) / 1000)
        elapsed = (time.perf_counter_ns() - start) / 1e9
        assert connection.execute("SELECT COUNT(*) FROM messages").fetchone()[0] == records
        connection.close()
        samples.sort()
        return dict(
            sqlite=sqlite3.sqlite_version,
            synchronous=synchronous,
            batch=batch,
            records=records,
            records_per_second=round(records / elapsed),
            commit_p50_us=round(samples[len(samples) // 2], 1),
            commit_p95_us=round(samples[len(samples) * 95 // 100], 1),
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--records", type=int, default=4096)
    args = parser.parse_args()
    if args.records <= 0:
        parser.error("--records must be positive")
    for mode in ["NORMAL", "FULL"]:
        for size in [1, 8, 32]:
            print(json.dumps(run(args.records, mode, size)))
