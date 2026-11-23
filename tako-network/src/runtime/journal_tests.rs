/// The SQLite schema of `runtime.db` as the runtime last created it.
const LEGACY_SCHEMA: &str = "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
    CREATE TABLE IF NOT EXISTS runtime_identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), network TEXT NOT NULL, server TEXT NOT NULL, last_time INTEGER NOT NULL, auth_window INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS consumed_nonces (signer TEXT NOT NULL, nonce TEXT NOT NULL, expires INTEGER NOT NULL, PRIMARY KEY(signer, nonce));
    CREATE INDEX IF NOT EXISTS nonce_expiry ON consumed_nonces(expires);
    CREATE TABLE IF NOT EXISTS observed_records (kind TEXT NOT NULL, id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(kind, id));
    CREATE TABLE IF NOT EXISTS registry_failure (singleton INTEGER PRIMARY KEY CHECK(singleton=1));
    CREATE TABLE IF NOT EXISTS revocation_nonces (signer TEXT NOT NULL, nonce TEXT NOT NULL, expires INTEGER NOT NULL, PRIMARY KEY(signer, nonce));
    CREATE INDEX IF NOT EXISTS revocation_nonce_expiry ON revocation_nonces(expires);
    CREATE TABLE IF NOT EXISTS pending_entitlements (id TEXT PRIMARY KEY, expires INTEGER NOT NULL, payload TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS invitations (id TEXT PRIMARY KEY, expires INTEGER NOT NULL, payload TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS shared_observed (kind TEXT NOT NULL, id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(kind, id));
    CREATE TABLE IF NOT EXISTS shared_publications (id TEXT PRIMARY KEY, expires INTEGER NOT NULL, payload TEXT NOT NULL);
    CREATE INDEX IF NOT EXISTS shared_publication_expiry ON shared_publications(expires);";

/// A legacy journal holding only the fixture's identity.
fn legacy_journal(fixture: &Fixture, last_time: i64) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(fixture.dir.path().join("runtime.db")).unwrap();
    db.execute_batch(LEGACY_SCHEMA).unwrap();
    db.execute(
        "INSERT INTO runtime_identity VALUES (1, ?1, ?2, ?3, ?4)",
        rusqlite::params![
            serde_json::to_string(&fixture.config.network).unwrap(),
            fixture.config.server_id.to_string(),
            last_time,
            fixture.config.limits.auth_window_seconds as i64
        ],
    )
    .unwrap();
    db
}

impl Fixture {
    fn try_open(&self, config: RuntimeConfig) -> RuntimeResult<ServerRuntime<MemoryStore>> {
        ServerRuntime::open(
            self.store.clone(),
            LocalDeviceKeys::load(&self.dir.path().join("keys")).unwrap(),
            config,
            &self.journal(),
        )
    }
}

/// Every nonce of a pool, checking each has its expiry index entry.
fn nonces(
    runtime: &ServerRuntime<MemoryStore>,
    pool: journal::NoncePool,
) -> Vec<(String, String, u64)> {
    let txn = runtime.db.begin_read().unwrap();
    let expiry = txn.open_table(pool.1).unwrap();
    let nonces: Vec<_> = txn
        .open_table(pool.0)
        .unwrap()
        .iter()
        .unwrap()
        .map(|row| {
            let (key, expires) = row.unwrap();
            let (signer, nonce) = key.value();
            (signer.to_owned(), nonce.to_owned(), expires.value())
        })
        .collect();
    for (signer, nonce, expires) in &nonces {
        assert!(
            expiry
                .get((*expires, signer.as_str(), nonce.as_str()))
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(rows(runtime, pool.1), nonces.len() as u64);
    nonces
}

#[test]
fn legacy_sqlite_journal_is_imported_once_with_every_row() {
    let fixture = Fixture::new();
    let keys = LocalDeviceKeys::generate().unwrap();
    let grant = fixture.grant(&keys);
    let entitlement = serde_json::to_string(&Entitlement {
        grant: grant.clone(),
        approver: fixture.config.network.initial_admin.clone(),
        independent_root: false,
        candidate: None,
    })
    .unwrap();
    let db = legacy_journal(&fixture, 20);
    // A crashed agent: nothing below was checkpointed out of the WAL.
    db.execute_batch("PRAGMA wal_autocheckpoint=0").unwrap();
    for member in &fixture.store.0.borrow().snapshot.members {
        db.execute(
            "INSERT INTO observed_records VALUES ('member', ?1, ?2)",
            rusqlite::params![
                member.device_id.to_string(),
                serde_json::to_string(member).unwrap()
            ],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO pending_entitlements VALUES (?1, 1000, ?2)",
        rusqlite::params![grant.pending_id.to_string(), entitlement],
    )
    .unwrap();
    db.execute_batch(
        r#"INSERT INTO consumed_nonces VALUES ('signer-a', 'n1', 140), ('signer-a', 'n2', 141), ('signer-b', 'n1', 142);
        INSERT INTO revocation_nonces VALUES ('signer-a', 'n3', 143);
        INSERT INTO invitations VALUES ('invitation', 900, '{"invitation":1}');
        INSERT INTO shared_observed VALUES ('secret', 'address', '{"record":1}'), ('grant', 'legacy', '{"grant":1}');
        INSERT INTO shared_publications VALUES ('publication', 800, '{"publication":1}');
        CREATE TABLE shared_commands (id TEXT PRIMARY KEY);"#,
    )
    .unwrap();
    std::mem::forget(db);
    let dir = fixture.dir.path();
    assert!(dir.join("runtime.db-wal").exists());
    assert_eq!(
        verify_journal(&fixture.journal()).unwrap(),
        JournalSummary {
            consumed_nonces: 3,
            revocation_nonces: 1
        }
    );

    let mut runtime = fixture.open();
    for (name, present) in [
        ("runtime.redb", true),
        ("runtime.db.migrated", true),
        ("runtime.db.migrated-wal", true),
        ("runtime.db", false),
        ("runtime.db-wal", false),
        ("runtime.db-shm", false),
        ("runtime.redb-import", false),
    ] {
        assert_eq!(dir.join(name).exists(), present, "{name}");
    }
    let owned = |rows: &[(&str, &str, u64)]| -> Vec<(String, String, u64)> {
        rows.iter()
            .map(|(a, b, c)| (a.to_string(), b.to_string(), *c))
            .collect()
    };
    assert_eq!(
        nonces(&runtime, CONSUMED_NONCES),
        owned(&[
            ("signer-a", "n1", 140),
            ("signer-a", "n2", 141),
            ("signer-b", "n1", 142)
        ])
    );
    assert_eq!(
        nonces(&runtime, REVOCATION_NONCES),
        owned(&[("signer-a", "n3", 143)])
    );
    assert_eq!(rows(&runtime, OBSERVED_RECORDS), 2);
    assert_eq!(rows(&runtime, REGISTRY_FAILURE), 0);
    assert_eq!(
        runtime.pending(&grant.pending_id, 999).unwrap().grant,
        grant
    );
    assert_eq!(payloads(&runtime, PENDING_ENTITLEMENTS), vec![entitlement]);
    assert_eq!(payloads(&runtime, INVITATIONS), vec![r#"{"invitation":1}"#]);
    assert_eq!(
        payloads(&runtime, SHARED_PUBLICATIONS),
        vec![r#"{"publication":1}"#]
    );
    let shared: Vec<(String, String, String)> = runtime
        .db
        .begin_read()
        .unwrap()
        .open_table(SHARED_OBSERVED)
        .unwrap()
        .iter()
        .unwrap()
        .map(|row| {
            let (key, payload) = row.unwrap();
            let (kind, id) = key.value();
            (kind.into(), id.into(), payload.value().into())
        })
        .collect();
    assert_eq!(
        shared,
        [
            ("grant", "legacy", r#"{"grant":1}"#),
            ("secret", "address", r#"{"record":1}"#)
        ]
        .map(|(a, b, c)| (a.to_owned(), b.to_owned(), c.to_owned()))
    );
    // The imported clock still refuses to run backwards.
    assert!(matches!(
        runtime.desired_host_state(19),
        Err(RuntimeError::Invalid("server clock moved backwards"))
    ));
    runtime.desired_host_state(20).unwrap();
    drop(runtime);
    // Imported once: reopening uses the redb journal alone.
    fixture.open();
    assert!(dir.join("runtime.db.migrated").exists());
}

#[test]
fn nonce_consumed_before_import_is_still_a_replay() {
    let fixture = Fixture::new();
    let signed = fixture.signed(&fixture.admin, RpcRequest::Snapshot, 10);
    let path = signed.request.path();
    let db = legacy_journal(&fixture, 10);
    db.execute(
        "INSERT INTO consumed_nonces VALUES (?1, ?2, ?3)",
        rusqlite::params![
            signed.signer.to_string(),
            signed.nonce.to_string(),
            (10 + fixture.config.limits.auth_window_seconds) as i64
        ],
    )
    .unwrap();
    drop(db);
    // The controller's first-bootstrap placeholder never hides a legacy journal.
    std::fs::write(fixture.journal(), b"").unwrap();
    let mut runtime = fixture.open();
    assert!(matches!(
        runtime.handle(signed.clone(), "POST", path, 11),
        Err(RuntimeError::Replay)
    ));
    fixture
        .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 11)
        .unwrap();
    drop(runtime);
    assert_eq!(
        verify_journal(&fixture.journal()).unwrap(),
        JournalSummary {
            consumed_nonces: 2,
            revocation_nonces: 0
        }
    );
    let mut runtime = fixture.open();
    assert!(matches!(
        runtime.handle(signed, "POST", path, 12),
        Err(RuntimeError::Replay)
    ));
}

#[test]
fn existing_redb_journal_is_used_and_legacy_file_left_alone() {
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    fixture
        .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 10)
        .unwrap();
    drop(runtime);
    let db = legacy_journal(&fixture, 50);
    db.execute(
        "INSERT INTO consumed_nonces VALUES ('stale', 'stale', 170)",
        [],
    )
    .unwrap();
    drop(db);
    let mut runtime = fixture.open();
    let dir = fixture.dir.path();
    assert!(dir.join("runtime.db").exists());
    assert!(!dir.join("runtime.db.migrated").exists());
    assert_eq!(rows(&runtime, CONSUMED_NONCES.0), 1);
    // The clock is the redb journal's (10), not the legacy file's (50).
    fixture
        .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 11)
        .unwrap();
}

#[test]
fn legacy_import_checks_identity_first_and_keeps_the_registry_latch() {
    let fixture = Fixture::new();
    drop(legacy_journal(&fixture, 0));
    let dir = fixture.dir.path();
    let mut wider = fixture.config.clone();
    wider.limits.auth_window_seconds = 300;
    assert!(matches!(
        fixture.try_open(wider),
        Err(RuntimeError::Conflict("local runtime identity"))
    ));
    // A refused import changes nothing.
    assert!(dir.join("runtime.db").exists());
    assert!(!dir.join("runtime.redb").exists());
    assert!(!dir.join("runtime.redb-import").exists());
    rusqlite::Connection::open(dir.join("runtime.db"))
        .unwrap()
        .execute("INSERT INTO registry_failure VALUES (1)", [])
        .unwrap();
    assert!(matches!(
        fixture.try_open(fixture.config.clone()),
        Err(RuntimeError::Capacity("registry requires local recovery"))
    ));
    assert!(dir.join("runtime.redb").exists());
    assert!(dir.join("runtime.db.migrated").exists());
    assert!(!dir.join("runtime.db").exists());
}

#[test]
fn blank_legacy_journal_from_an_unfinished_bootstrap_is_retired() {
    let fixture = Fixture::new();
    let dir = fixture.dir.path();
    // An old agent created the file, or its schema, but never its identity.
    std::fs::write(dir.join("runtime.db"), b"").unwrap();
    fixture.open();
    assert!(dir.join("runtime.db.migrated").exists());
    assert!(!dir.join("runtime.db").exists());
    std::fs::remove_file(dir.join("runtime.db.migrated")).unwrap();
    std::fs::remove_file(fixture.journal()).unwrap();
    let db = legacy_journal(&fixture, 0);
    db.execute_batch("DELETE FROM runtime_identity").unwrap();
    drop(db);
    fixture.open();
    assert!(!dir.join("runtime.db").exists());
    // Rows without the identity that anchors them are never discarded.
    std::fs::remove_file(fixture.journal()).unwrap();
    let db = legacy_journal(&fixture, 0);
    db.execute_batch(
        "DELETE FROM runtime_identity; INSERT INTO consumed_nonces VALUES ('s', 'n', 1)",
    )
    .unwrap();
    drop(db);
    assert!(matches!(
        fixture.try_open(fixture.config.clone()),
        Err(RuntimeError::Invalid("legacy runtime identity missing"))
    ));
    assert!(dir.join("runtime.db").exists());
}

#[test]
fn journal_verification_never_creates_a_journal() {
    let fixture = Fixture::new();
    assert!(verify_journal(&fixture.journal()).is_err());
    std::fs::write(fixture.journal(), b"").unwrap();
    assert!(verify_journal(&fixture.journal()).is_err());
    assert_eq!(std::fs::metadata(fixture.journal()).unwrap().len(), 0);
    std::fs::write(fixture.journal(), b"not a journal").unwrap();
    assert!(verify_journal(&fixture.journal()).is_err());
    std::fs::remove_file(fixture.journal()).unwrap();
    let db = legacy_journal(&fixture, 0);
    db.execute_batch("DELETE FROM runtime_identity").unwrap();
    assert!(verify_journal(&fixture.journal()).is_err());
}
