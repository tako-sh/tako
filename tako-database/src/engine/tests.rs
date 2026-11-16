use super::*;
use crate::proto::NamedArg;

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    create_database(&path).unwrap();
    Fixture { _dir: dir, path }
}

fn open(fixture: &Fixture) -> Connection {
    Connection::open(&fixture.path, &EngineConfig::default()).unwrap()
}

fn run(conn: &mut Connection, sql: &str) -> Result<StmtResult, Error> {
    conn.execute(
        sql,
        &Args::Positional(&[]),
        true,
        &mut Budget::new(usize::MAX),
    )
}

fn run_args(conn: &mut Connection, sql: &str, args: &[Value]) -> Result<StmtResult, Error> {
    conn.execute(
        sql,
        &Args::Positional(args),
        true,
        &mut Budget::new(usize::MAX),
    )
}

#[test]
fn created_databases_use_wal_and_refuse_to_overwrite() {
    let fixture = fixture();
    let conn = rusqlite::Connection::open(&fixture.path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert!(create_database(&fixture.path).is_err());
}

#[test]
fn executes_statements_and_reports_columns_rows_and_changes() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(
        &mut conn,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB)",
    )
    .unwrap();
    let insert = run_args(
        &mut conn,
        "INSERT INTO users (name, score, data) VALUES (?, ?, ?)",
        &[
            Value::Text("ada".into()),
            Value::Float(1.5),
            Value::Blob(vec![1, 2]),
        ],
    )
    .unwrap();
    assert_eq!(insert.affected_row_count, 1);
    assert_eq!(insert.last_insert_rowid, Some(1));

    let select = run(
        &mut conn,
        "SELECT id, name, score, data, NULL AS empty FROM users",
    )
    .unwrap();
    assert_eq!(
        select.cols,
        vec![
            Col {
                name: Some("id".into()),
                decltype: Some("INTEGER".into())
            },
            Col {
                name: Some("name".into()),
                decltype: Some("TEXT".into())
            },
            Col {
                name: Some("score".into()),
                decltype: Some("REAL".into())
            },
            Col {
                name: Some("data".into()),
                decltype: Some("BLOB".into())
            },
            Col {
                name: Some("empty".into()),
                decltype: None
            },
        ]
    );
    assert_eq!(
        select.rows,
        vec![vec![
            Value::Integer(1),
            Value::Text("ada".into()),
            Value::Float(1.5),
            Value::Blob(vec![1, 2]),
            Value::Null,
        ]]
    );
    // A read after a write reports no changes of its own.
    assert_eq!(select.affected_row_count, 0);
    assert_eq!(select.last_insert_rowid, None);

    let update = run(&mut conn, "UPDATE users SET score = 2").unwrap();
    assert_eq!(update.affected_row_count, 1);
    assert_eq!(update.last_insert_rowid, None);
}

#[test]
fn want_rows_false_executes_without_returning_rows() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    let result = conn
        .execute(
            "SELECT 1",
            &Args::Positional(&[]),
            false,
            &mut Budget::new(usize::MAX),
        )
        .unwrap();
    assert!(result.rows.is_empty());
    assert_eq!(result.rows_read, 1);
    assert!(!conn.take_wrote());
}

#[test]
fn binds_bare_names_to_any_prefix_and_prefixed_names_exactly() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    let arg = |name: &str, value| NamedArg {
        name: name.into(),
        value: Value::Integer(value),
    };
    let mut execute = |sql: &str, named: &[NamedArg]| {
        conn.execute(sql, &Args::Named(named), true, &mut Budget::new(usize::MAX))
    };
    let result = execute(
        "SELECT :a + @b + $c",
        &[arg("a", 1), arg("@b", 2), arg("$c", 3)],
    )
    .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Integer(6)]]);
    // `:x` and `$x` are different parameters.
    let result = execute("SELECT :x * 10 + $x", &[arg(":x", 1), arg("$x", 2)]).unwrap();
    assert_eq!(result.rows, vec![vec![Value::Integer(12)]]);
    let error = execute("SELECT $c", &[arg(":c", 3)]).unwrap_err();
    assert_eq!(error.code, "ARGS_INVALID");
    // Two arguments for `:x` would leave `:y` unbound (NULL).
    let error = execute("SELECT :x, :y", &[arg(":x", 1), arg("x", 2)]).unwrap_err();
    assert_eq!(error.code, "ARGS_INVALID");
    assert!(error.message.contains("already binds"), "{}", error.message);
}

#[test]
fn rejects_argument_mismatches() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    let error = run_args(&mut conn, "SELECT ?", &[]).unwrap_err();
    assert_eq!(error.code, "ARGS_INVALID");
    let error = run_args(&mut conn, "SELECT 1", &[Value::Integer(1)]).unwrap_err();
    assert_eq!(error.code, "ARGS_INVALID");
    let named = [NamedArg {
        name: "a".into(),
        value: Value::Null,
    }];
    let error = conn
        .execute(
            "SELECT ?",
            &Args::Named(&named),
            true,
            &mut Budget::new(usize::MAX),
        )
        .unwrap_err();
    assert_eq!(error.code, "ARGS_INVALID");
    let error = conn
        .execute(
            "SELECT :b",
            &Args::Named(&named),
            true,
            &mut Budget::new(usize::MAX),
        )
        .unwrap_err();
    assert_eq!(error.code, "ARGS_INVALID");
}

#[test]
fn maps_sqlite_errors_to_protocol_codes() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(&mut conn, "CREATE TABLE t (email TEXT UNIQUE NOT NULL)").unwrap();
    run(&mut conn, "INSERT INTO t VALUES ('a')").unwrap();
    let unique = run(&mut conn, "INSERT INTO t VALUES ('a')").unwrap_err();
    assert_eq!(unique.code, "SQLITE_CONSTRAINT");
    assert_eq!(
        unique.extended_code.as_deref(),
        Some("SQLITE_CONSTRAINT_UNIQUE")
    );
    assert!(
        unique.message.contains("UNIQUE constraint failed"),
        "{}",
        unique.message
    );
    let not_null = run(&mut conn, "INSERT INTO t VALUES (NULL)").unwrap_err();
    assert_eq!(
        not_null.extended_code.as_deref(),
        Some("SQLITE_CONSTRAINT_NOTNULL")
    );
    assert_eq!(
        run(&mut conn, "SELEC 1").unwrap_err().code,
        "SQL_PARSE_ERROR"
    );
    assert_eq!(
        run(&mut conn, "SELECT * FROM missing").unwrap_err().code,
        "SQLITE_ERROR"
    );
    assert_eq!(
        run(&mut conn, "SELECT 1; SELECT 2").unwrap_err().code,
        "SQL_MANY_STATEMENTS"
    );
    assert_eq!(run(&mut conn, "  ;; ").unwrap_err().code, "SQL_PARSE_ERROR");
}

#[test]
fn enforces_foreign_keys_by_default() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(&mut conn, "CREATE TABLE a (id INTEGER PRIMARY KEY)").unwrap();
    run(&mut conn, "CREATE TABLE b (a_id INTEGER REFERENCES a(id))").unwrap();
    let error = run(&mut conn, "INSERT INTO b VALUES (7)").unwrap_err();
    assert_eq!(
        error.extended_code.as_deref(),
        Some("SQLITE_CONSTRAINT_FOREIGNKEY")
    );
}

#[test]
fn refuses_to_reach_files_outside_the_database() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    let outside = fixture.path.with_file_name("stolen.sqlite");
    let into = run(&mut conn, &format!("VACUUM INTO '{}'", outside.display())).unwrap_err();
    assert_eq!(into.code, "SQLITE_AUTH", "{into:?}");
    assert!(!outside.exists());
    let attach = run(
        &mut conn,
        &format!("ATTACH '{}' AS other", outside.display()),
    )
    .unwrap_err();
    assert_eq!(attach.code, "SQLITE_AUTH");
    assert!(!outside.exists());
    // SQLite reports denied functions as a plain error.
    let extension = run(&mut conn, "SELECT load_extension('x')").unwrap_err();
    assert!(
        extension.message.contains("not authorized"),
        "{extension:?}"
    );
    // A plain VACUUM attaches only an anonymous temporary database.
    run(&mut conn, "VACUUM").unwrap();
}

#[test]
fn allows_introspection_and_session_pragmas_but_not_server_settings() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(&mut conn, "CREATE TABLE t (x)").unwrap();
    run(&mut conn, "PRAGMA table_info(t)").unwrap();
    run(&mut conn, "SELECT * FROM pragma_table_info('t')").unwrap();
    run(&mut conn, "PRAGMA journal_mode").unwrap();
    run(&mut conn, "PRAGMA user_version = 3").unwrap();
    assert!(!conn.is_dirty());
    for denied in [
        "PRAGMA journal_mode = DELETE",
        "PRAGMA synchronous = OFF",
        "PRAGMA locking_mode = EXCLUSIVE",
        "PRAGMA writable_schema = ON",
        "PRAGMA cache_size = -10000000",
        "PRAGMA mmap_size = 1000000000",
        "PRAGMA busy_timeout = 100000",
        "PRAGMA temp_store_directory = '/tmp'",
    ] {
        assert_eq!(
            run(&mut conn, denied).unwrap_err().code,
            "SQLITE_AUTH",
            "{denied}"
        );
    }
    run(&mut conn, "PRAGMA foreign_keys = OFF").unwrap();
    assert!(conn.is_dirty(), "session pragmas mark the connection");
}

#[test]
fn commit_counts_as_a_write() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    let wrote = |conn: &mut Connection, sql: &str| {
        conn.execute(
            sql,
            &Args::Positional(&[]),
            true,
            &mut Budget::new(usize::MAX),
        )
        .unwrap();
        conn.take_wrote()
    };
    wrote(&mut conn, "CREATE TABLE t (x)");
    assert!(!wrote(&mut conn, "BEGIN"));
    assert!(wrote(&mut conn, "INSERT INTO t VALUES (1)"));
    // A snapshot between the insert and the commit must not lose it.
    assert!(
        wrote(&mut conn, "COMMIT"),
        "COMMIT makes the writes durable"
    );
    assert!(!wrote(&mut conn, "SELECT * FROM t"));
}

#[test]
fn streaming_sinks_can_stop_early() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    let mut seen = 0;
    let outcome = conn.execute_streaming(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT i FROM n",
        &Args::Positional(&[]),
        |event| {
            if let Event::Row(_) = event {
                seen += 1;
                if seen == 10 {
                    return Err(Error::new(super::STOP, "enough"));
                }
            }
            Ok(())
        },
    );
    assert!(outcome.is_ok(), "stopping is not an error");
    assert_eq!(seen, 10, "an endless query stops at the sink's request");
}

#[test]
fn temporary_objects_and_attachments_mark_the_connection() {
    let fixture = fixture();
    for sql in [
        "CREATE TEMP TABLE scratch (x)",
        "CREATE TEMP VIEW recent AS SELECT 1",
        "ATTACH DATABASE '' AS extra",
    ] {
        let mut conn = open(&fixture);
        assert!(!conn.is_dirty());
        run(&mut conn, sql).unwrap();
        assert!(conn.is_dirty(), "{sql} leaves state on the connection");
    }
}

#[test]
fn statements_stop_at_the_time_limit() {
    let fixture = fixture();
    let config = EngineConfig {
        statement_timeout: Duration::from_millis(200),
        ..EngineConfig::default()
    };
    let mut conn = Connection::open(&fixture.path, &config).unwrap();
    let started = Instant::now();
    let error = run(
        &mut conn,
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT count(*) FROM n",
    )
    .unwrap_err();
    assert_eq!(error.code, "SQLITE_INTERRUPT");
    assert!(started.elapsed() < Duration::from_secs(5));
    // The connection remains usable.
    assert_eq!(run(&mut conn, "SELECT 1").unwrap().rows.len(), 1);
}

#[test]
fn response_budget_stops_large_results() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    let error = conn
        .execute(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n LIMIT 10000) SELECT x FROM n",
            &Args::Positional(&[]),
            true,
            &mut Budget::new(10_000),
        )
        .unwrap_err();
    assert_eq!(error.code, "RESPONSE_TOO_LARGE");
}

#[test]
fn sequence_runs_statements_and_stops_at_the_first_failure() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    conn.sequence("CREATE TABLE t (x); INSERT INTO t VALUES (1); SELECT * FROM t;")
        .unwrap();
    assert!(conn.take_wrote());
    let error = conn
        .sequence(
            "INSERT INTO t VALUES (2); INSERT INTO missing VALUES (3); INSERT INTO t VALUES (4)",
        )
        .unwrap_err();
    assert_eq!(error.code, "SQLITE_ERROR");
    assert!(
        conn.take_wrote(),
        "the first insert wrote before the failure"
    );
    let count = run(&mut conn, "SELECT count(*) FROM t").unwrap();
    assert_eq!(count.rows, vec![vec![Value::Integer(2)]]);
    conn.take_wrote();
    conn.sequence("SELECT 1; SELECT 2").unwrap();
    assert!(!conn.take_wrote());
}

#[test]
fn a_write_that_fails_afterwards_still_counts() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(&mut conn, "CREATE TABLE t (x)").unwrap();
    conn.take_wrote();
    // RETURNING applies every change before the first row; the budget then
    // fails the statement, but the rows are committed.
    let error = conn
        .execute(
            "INSERT INTO t SELECT value FROM json_each('[1,2,3,4]') RETURNING x",
            &Args::Positional(&[]),
            true,
            &mut Budget::new(1),
        )
        .unwrap_err();
    assert_eq!(error.code, "RESPONSE_TOO_LARGE");
    assert!(conn.take_wrote());
    let count = run(&mut conn, "SELECT count(*) FROM t").unwrap();
    assert_eq!(count.rows, vec![vec![Value::Integer(4)]]);
}

#[test]
fn describe_reports_parameters_and_columns() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(&mut conn, "CREATE TABLE users (id INTEGER, name TEXT)").unwrap();
    let described = conn
        .describe("SELECT id, name FROM users WHERE id = :id AND name = ?")
        .unwrap();
    assert_eq!(described.params.len(), 2);
    assert_eq!(described.params[0].name.as_deref(), Some(":id"));
    assert_eq!(described.params[1].name, None);
    assert_eq!(described.cols.len(), 2);
    assert!(described.is_readonly);
    assert!(!described.is_explain);
    assert!(conn.describe("EXPLAIN SELECT 1").unwrap().is_explain);
    assert!(!conn.describe("DELETE FROM users").unwrap().is_readonly);
}

#[test]
fn transactions_span_statements_and_roll_back() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(&mut conn, "CREATE TABLE t (x)").unwrap();
    run(&mut conn, "BEGIN").unwrap();
    assert!(!conn.is_autocommit());
    run(&mut conn, "INSERT INTO t VALUES (1)").unwrap();
    conn.rollback();
    assert!(conn.is_autocommit());
    let count = run(&mut conn, "SELECT count(*) FROM t").unwrap();
    assert_eq!(count.rows, vec![vec![Value::Integer(0)]]);
}

#[test]
fn snapshots_are_consistent_verified_copies() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(&mut conn, "CREATE TABLE t (x)").unwrap();
    run(&mut conn, "INSERT INTO t VALUES (1), (2)").unwrap();
    run(&mut conn, "BEGIN").unwrap();
    run(&mut conn, "INSERT INTO t VALUES (3)").unwrap();
    let dest = fixture.path.with_file_name("copy.sqlite");
    snapshot_database(&fixture.path, &dest, Duration::from_secs(1)).unwrap();
    conn.rollback();
    let copy = rusqlite::Connection::open(&dest).unwrap();
    let count: i64 = copy
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2, "the open transaction is not part of the snapshot");
    assert!(snapshot_database(&fixture.path, &dest, Duration::from_secs(1)).is_err());
    verify_database(&dest).unwrap();
}

#[test]
fn verify_rejects_files_that_are_not_databases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("junk.sqlite");
    std::fs::write(
        &path,
        b"definitely not sqlite, just a long enough header to look at",
    )
    .unwrap();
    assert!(verify_database(&path).is_err());
}

#[test]
fn replacing_keeps_the_previous_file_and_drops_stale_sidecars() {
    let fixture = fixture();
    {
        let mut conn = open(&fixture);
        run(&mut conn, "CREATE TABLE t (x)").unwrap();
        run(&mut conn, "INSERT INTO t VALUES ('old')").unwrap();
    }
    let replacement = fixture.path.with_file_name("new.sqlite");
    create_database(&replacement).unwrap();
    {
        let conn = rusqlite::Connection::open(&replacement).unwrap();
        conn.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES ('new');")
            .unwrap();
    }
    let previous = fixture.path.with_file_name("previous.sqlite");
    replace_database(&fixture.path, &replacement, &previous).unwrap();
    let read = |path: &std::path::Path| -> String {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("SELECT x FROM t", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(read(&fixture.path), "new");
    assert_eq!(read(&previous), "old");
    assert!(!replacement.exists());
}

#[test]
fn response_budget_counts_text_as_json_encodes_it() {
    for text in [
        "plain",
        "quote \" and backslash \\",
        "new\nline\ttab\r",
        "\u{0}\u{1}\u{1f}\u{7f}",
        "ünïcödé ✓",
    ] {
        let encoded = serde_json::to_string(text).unwrap();
        assert_eq!(super::escaped_len(text), encoded.len() - 2, "{text:?}");
    }
    let controls = "\u{1}".repeat(1000);
    let row = PendingRow(vec![ValueRef::Text(controls.as_bytes())]);
    assert!(row.encoded_size() >= 6000);
    // Text that is not UTF-8 is sized without being copied, at most as large
    // as its replacement characters or escapes.
    let invalid = [0xffu8; 100];
    let row = PendingRow(vec![ValueRef::Text(&invalid)]);
    let copied = serde_json::to_string(&String::from_utf8_lossy(&invalid)).unwrap();
    assert!(row.encoded_size() >= copied.len());
}

#[test]
fn reset_empties_a_database_that_others_hold_open() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(&mut conn, "CREATE TABLE t (x)").unwrap();
    run(&mut conn, "INSERT INTO t VALUES (1)").unwrap();
    run(
        &mut conn,
        "CREATE TABLE seq (id INTEGER PRIMARY KEY AUTOINCREMENT)",
    )
    .unwrap();
    run(&mut conn, "CREATE VIEW v AS SELECT x FROM t").unwrap();
    run(&mut conn, "CREATE VIRTUAL TABLE docs USING fts5(body)").unwrap();
    run(&mut conn, "PRAGMA user_version = 7").unwrap();
    super::reset_database_file(&fixture.path).unwrap();
    let error = run(&mut conn, "SELECT * FROM t").unwrap_err();
    assert!(error.message.contains("no such table"), "{}", error.message);
    run(&mut conn, "CREATE TABLE t (x)").unwrap();
    let raw = rusqlite::Connection::open(&fixture.path).unwrap();
    let left: i64 = raw
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' AND name <> 't'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(left, 0);
    let version: i64 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 0);
    let mode: String = rusqlite::Connection::open(&fixture.path)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn computed_attach_targets_are_refused() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    let other = fixture.path.with_file_name("other.sqlite");
    let error = run_args(
        &mut conn,
        "ATTACH DATABASE ? AS other",
        &[Value::Text(other.display().to_string())],
    )
    .unwrap_err();
    assert_eq!(error.code, "SQLITE_AUTH", "{}", error.message);
    let error = run(&mut conn, "ATTACH DATABASE 'x' || '.sqlite' AS other").unwrap_err();
    assert_eq!(error.code, "SQLITE_AUTH", "{}", error.message);
    assert!(!other.exists());
    // Plain VACUUM attaches an empty-named database and stays allowed.
    run(&mut conn, "VACUUM").unwrap();
}

#[test]
fn values_larger_than_a_response_are_refused_before_they_are_copied() {
    let fixture = fixture();
    let mut conn = Connection::open(
        &fixture.path,
        &EngineConfig {
            max_value_bytes: 1024,
            ..EngineConfig::default()
        },
    )
    .unwrap();
    let error = run(&mut conn, "SELECT randomblob(4096)").unwrap_err();
    assert_eq!(error.code, "SQLITE_TOOBIG", "{}", error.message);
    run(&mut conn, "SELECT randomblob(512)").unwrap();
}

#[test]
fn inserts_after_comments_report_their_rowid() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(&mut conn, "CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    run(&mut conn, "INSERT INTO t VALUES (5)").unwrap();
    run(&mut conn, "DELETE FROM t").unwrap();
    // The same rowid again leaves last_insert_rowid() unchanged.
    let result = run(
        &mut conn,
        "-- comment\n/* block */ INSERT INTO t VALUES (5)",
    )
    .unwrap();
    assert_eq!(result.last_insert_rowid, Some(5));
}

#[test]
fn with_updates_and_deletes_report_no_insert_rowid() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    run(
        &mut conn,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    )
    .unwrap();
    run(&mut conn, "INSERT INTO t VALUES (7, 0)").unwrap();
    for sql in [
        "WITH c AS (SELECT 7 AS id) UPDATE t SET n = n + 1 WHERE id IN (SELECT id FROM c)",
        "WITH c AS (SELECT 7 AS id) DELETE FROM t WHERE id IN (SELECT id FROM c)",
    ] {
        let result = run(&mut conn, sql).unwrap();
        assert_eq!(result.affected_row_count, 1, "{sql}");
        assert_eq!(result.last_insert_rowid, None, "{sql}");
    }
    // A WITH ... INSERT that moves the rowid still reports it.
    let result = run(
        &mut conn,
        "WITH c AS (SELECT 9 AS id) INSERT INTO t SELECT id, 0 FROM c",
    )
    .unwrap();
    assert_eq!(result.last_insert_rowid, Some(9));
    // So does one whose rowid equals the previous insert's.
    run(&mut conn, "CREATE TABLE u (id INTEGER PRIMARY KEY)").unwrap();
    let result = run(
        &mut conn,
        "WITH c AS (SELECT 9 AS id) INSERT INTO u SELECT id FROM c",
    )
    .unwrap();
    assert_eq!(result.last_insert_rowid, Some(9));
}

#[test]
fn statement_verbs_skip_with_clauses() {
    for (sql, verb) in [
        ("INSERT INTO t VALUES (1)", Some("INSERT")),
        (
            "  /* c */ -- c\n replace into t values (1)",
            Some("replace"),
        ),
        (
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
            Some("INSERT"),
        ),
        (
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n LIMIT 3) INSERT INTO t SELECT i FROM n",
            Some("INSERT"),
        ),
        (
            "with a as materialized (select ')' ), \"b\"\"c\" as not materialized (select (1)) insert into t select 1",
            Some("insert"),
        ),
        (
            "WITH replace AS (SELECT 1) SELECT * FROM replace",
            Some("SELECT"),
        ),
        ("WITH x AS (SELECT 1) UPDATE t SET n = 1", Some("UPDATE")),
        ("WITH [odd]] name] AS (SELECT 1) DELETE FROM t", None),
        ("WITH x AS (SELECT 1", None),
        ("SELECT 1", Some("SELECT")),
    ] {
        assert_eq!(statement_verb(sql), verb, "{sql}");
    }
}

#[test]
fn long_statements_run_without_the_statement_cache() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    // Longer than the cache takes; runs the same, twice.
    let sql = format!("/* {} */ SELECT 41 + 1", "x".repeat(8 * 1024));
    for _ in 0..2 {
        let result = run(&mut conn, &sql).unwrap();
        assert_eq!(result.rows, vec![vec![Value::Integer(42)]]);
    }
    let described = conn.describe(&sql).unwrap();
    assert!(described.is_readonly);
}

#[test]
fn shared_budgets_reserve_what_they_charge_from_the_host() {
    let host = Arc::new(tokio::sync::Semaphore::new(100_000));
    let mut budget = Budget::shared(usize::MAX, host.clone());
    assert!(budget.charge(10));
    assert!(budget.charge(70_000));
    assert!(!budget.charge(40_000), "the host has no room left");
    assert!(budget.host_exhausted());
    // What was charged stays reserved; nothing more.
    let reservation = budget.into_reservation().unwrap();
    assert_eq!(reservation.num_permits(), 70_010);
    assert_eq!(host.available_permits(), 100_000 - 70_010);
    drop(reservation);
    assert_eq!(host.available_permits(), 100_000);
    // A response's own limit applies first, and is not the host's fault.
    let mut budget = Budget::shared(100, host.clone());
    assert!(!budget.charge(101));
    assert!(!budget.host_exhausted());
    assert!(budget.charge(100));
    drop(budget);
    assert_eq!(host.available_permits(), 100_000, "dropping releases");
}

#[test]
fn connections_keep_a_small_page_cache() {
    let fixture = fixture();
    let mut conn = open(&fixture);
    let result = run(&mut conn, "PRAGMA cache_size").unwrap();
    assert_eq!(result.rows, vec![vec![Value::Integer(-1024)]]);
    // Apps cannot raise it.
    assert!(run(&mut conn, "PRAGMA cache_size = -1000000").is_err());
}

#[test]
fn the_soft_heap_limit_is_set_for_the_process() {
    set_soft_heap_limit(256 * 1024 * 1024);
    // SAFETY: a negative argument only reads the current limit.
    let current = unsafe { ffi::sqlite3_soft_heap_limit64(-1) };
    assert_eq!(current, 256 * 1024 * 1024);
}
