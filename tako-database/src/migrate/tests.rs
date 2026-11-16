use super::*;

fn migration(name: &str, sql: &str) -> Migration {
    Migration {
        name: name.into(),
        sql: sql.into(),
    }
}

fn database() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    (dir, path)
}

fn tables(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut statement = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn names_need_an_order_number() {
    assert_eq!(migration_number("0001_initial.sql"), Some(1));
    assert_eq!(
        migration_number("20260101120000_add-users.v2.sql"),
        Some(20260101120000)
    );
    assert_eq!(migration_number("0007.sql"), Some(7));
    assert_eq!(migration_number("initial.sql"), None);
    assert_eq!(migration_number("0001_.sql"), None);
    assert_eq!(migration_number("0001_a b.sql"), None);
    assert_eq!(migration_number("0001_initial.txt"), None);
    let duplicate = order(&[migration("1_a.sql", ""), migration("01_b.sql", "")]).unwrap_err();
    assert!(duplicate.contains("share the number 1"), "{duplicate}");
}

#[test]
fn applies_pending_migrations_in_order_once() {
    let (_dir, path) = database();
    let migrations = [
        migration("0002_items.sql", "INSERT INTO users (name) VALUES ('ada');"),
        migration(
            "0001_users.sql",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
        ),
    ];
    let preview = migrate_file(&path, "shop", &migrations, true).unwrap();
    assert_eq!(preview.pending, ["0001_users.sql", "0002_items.sql"]);
    assert!(preview.applied.is_empty());

    let report = migrate_file(&path, "shop", &migrations, false).unwrap();
    assert_eq!(report.applied, ["0001_users.sql", "0002_items.sql"]);
    let again = migrate_file(&path, "shop", &migrations, false).unwrap();
    assert!(again.applied.is_empty(), "each migration runs once");
    let conn = rusqlite::Connection::open(&path).unwrap();
    let users: i64 = conn
        .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
        .unwrap();
    assert_eq!(users, 1);
}

#[test]
fn a_failing_migration_leaves_no_trace_and_keeps_earlier_ones() {
    let (_dir, path) = database();
    let migrations = [
        migration(
            "0001_users.sql",
            "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        ),
        migration(
            "0002_broken.sql",
            "CREATE TABLE half (x); INSERT INTO missing VALUES (1);",
        ),
        migration("0003_later.sql", "CREATE TABLE later (x);"),
    ];
    let error = migrate_file(&path, "shop", &migrations, false).unwrap_err();
    assert!(
        error.contains("Migration `0002_broken.sql` failed"),
        "{error}"
    );
    assert!(
        error.contains("Applied before it: 0001_users.sql"),
        "{error}"
    );
    let tables = tables(&path);
    assert!(tables.contains(&"users".to_owned()));
    assert!(
        !tables.contains(&"half".to_owned()),
        "the failed file rolled back"
    );
    assert!(
        !tables.contains(&"later".to_owned()),
        "later files did not run"
    );
}

#[test]
fn edited_applied_migrations_fail_and_missing_ones_warn() {
    let (_dir, path) = database();
    migrate_file(
        &path,
        "shop",
        &[
            migration("0001_users.sql", "CREATE TABLE users (id INTEGER);"),
            migration("0002_posts.sql", "CREATE TABLE posts (id INTEGER);"),
        ],
        false,
    )
    .unwrap();
    let error = migrate_file(
        &path,
        "shop",
        &[migration(
            "0001_users.sql",
            "CREATE TABLE users (id INTEGER, name TEXT);",
        )],
        false,
    )
    .unwrap_err();
    assert!(error.contains("changed after it was applied"), "{error}");

    let report = migrate_file(
        &path,
        "shop",
        &[migration(
            "0001_users.sql",
            "CREATE TABLE users (id INTEGER);",
        )],
        false,
    )
    .unwrap();
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("0002_posts.sql"));
}

#[test]
fn late_migrations_run_with_a_warning() {
    let (_dir, path) = database();
    migrate_file(
        &path,
        "shop",
        &[migration("0005_a.sql", "CREATE TABLE a (x);")],
        false,
    )
    .unwrap();
    let report = migrate_file(
        &path,
        "shop",
        &[
            migration("0005_a.sql", "CREATE TABLE a (x);"),
            migration("0003_b.sql", "CREATE TABLE b (x);"),
        ],
        false,
    )
    .unwrap();
    assert_eq!(report.applied, ["0003_b.sql"]);
    assert!(report.warnings[0].contains("numbered before"));
}

#[test]
fn apps_sharing_a_database_keep_separate_histories() {
    let (_dir, path) = database();
    migrate_file(
        &path,
        "shop",
        &[migration(
            "0001_initial.sql",
            "CREATE TABLE orders (id INTEGER);",
        )],
        false,
    )
    .unwrap();
    let report = migrate_file(
        &path,
        "admin",
        &[migration(
            "0001_initial.sql",
            "CREATE TABLE audit (id INTEGER);",
        )],
        false,
    )
    .unwrap();
    assert_eq!(report.applied, ["0001_initial.sql"]);
    let tables = tables(&path);
    assert!(tables.contains(&"orders".to_owned()) && tables.contains(&"audit".to_owned()));
}

#[test]
fn migrations_may_not_end_their_transaction() {
    let (_dir, path) = database();
    let error = migrate_file(
        &path,
        "shop",
        &[migration("0001_commit.sql", "CREATE TABLE a (x); COMMIT;")],
        false,
    )
    .unwrap_err();
    assert!(error.contains("ended the transaction"), "{error}");
}

#[test]
fn migrations_can_rebuild_referenced_tables_but_not_break_references() {
    let (_dir, path) = database();
    let initial = migration(
        "0001_initial.sql",
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));
         INSERT INTO users VALUES (1); INSERT INTO posts VALUES (1, 1);",
    );
    // The 12-step rebuild from https://sqlite.org/lang_altertable.html.
    let rebuild = migration(
        "0002_rebuild_users.sql",
        "CREATE TABLE users_new (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT '');
         INSERT INTO users_new (id) SELECT id FROM users;
         DROP TABLE users;
         ALTER TABLE users_new RENAME TO users;",
    );
    let report = migrate_file(&path, "shop", &[initial.clone(), rebuild.clone()], false).unwrap();
    assert_eq!(report.applied.len(), 2);

    let broken = migration("0003_orphans.sql", "DELETE FROM users;");
    let error = migrate_file(&path, "shop", &[initial, rebuild, broken], false).unwrap_err();
    assert!(error.contains("break foreign keys"), "{error}");
    assert!(error.contains("`posts`"), "{error}");
    let conn = rusqlite::Connection::open(&path).unwrap();
    let users: i64 = conn
        .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
        .unwrap();
    assert_eq!(users, 1, "the breaking migration rolled back");
}
