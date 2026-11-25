//! Preparing a replacement copy of a workflow database, written by a
//! restore, an import or a reset, before the server swaps it in under
//! workflow runtimes that keep the database open.

use std::io;
use tako_db::PortableDatabase;

/// SQL that creates the workflow store's tables in a database file.
/// Idempotent.
pub fn schema_sql() -> String {
    super::hosted::schema_batch()
}

/// Keep the schedule state of `live`, the database in use now, in
/// `replacement`, which already has the workflow tables.
///
/// Schedules belong to the releases running now, which register them only
/// when they start, so an older copy's set (or a reset's empty one) would
/// stay until each app's next deploy. The revision counter must not go back
/// either: a later deploy could then take a revision an app's running
/// release already holds and lose to it.
pub fn keep_live_schedules(
    live: &PortableDatabase,
    replacement: &PortableDatabase,
) -> io::Result<()> {
    let complete = live.query(
        "SELECT count(*) FROM main.sqlite_schema WHERE type = 'table' AND rootpage > 0 AND name IN
        ('tako_workflow_meta', 'tako_workflow_schedule_releases', 'tako_workflow_schedules')",
        &[],
    )?;
    if complete[0][0].as_int() != Some(3) {
        return Ok(());
    }
    let revision = live.query(
        "SELECT MAX(
        COALESCE((SELECT value FROM tako_workflow_meta WHERE name = 'schedule_revision'), 0),
        COALESCE((SELECT MAX(revision) FROM tako_workflow_schedule_releases), 0))",
        &[],
    )?;
    let releases = live.query(
        "SELECT app, build, revision FROM tako_workflow_schedule_releases",
        &[],
    )?;
    let schedules = live.query(
        "SELECT app, name, cron, last_run_at, max_attempts FROM tako_workflow_schedules",
        &[],
    )?;
    replacement.execute_batch("BEGIN IMMEDIATE")?;
    let update = || -> io::Result<()> {
        replacement.query(
            "UPDATE tako_workflow_meta SET value = MAX(value, ?1,
             COALESCE((SELECT MAX(revision) FROM tako_workflow_schedule_releases), 0))
             WHERE name = 'schedule_revision'",
            &revision[0],
        )?;
        replacement.execute_batch(
            "DELETE FROM tako_workflow_schedule_releases; DELETE FROM tako_workflow_schedules",
        )?;
        for row in releases {
            replacement.query("INSERT INTO tako_workflow_schedule_releases (app, build, revision) VALUES (?1, ?2, ?3)", &row)?;
        }
        for row in schedules {
            replacement.query(
                "INSERT INTO tako_workflow_schedules (app, name, cron, last_run_at, max_attempts)
                VALUES (?1, ?2, ?3, ?4, ?5)",
                &row,
            )?;
        }
        replacement.execute_batch("COMMIT")
    };
    if let Err(error) = update() {
        replacement.execute_batch("ROLLBACK")?;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PortableDatabase) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        tako_db::DatabaseHost::create_file(&path).unwrap();
        let conn = PortableDatabase::open(&path).unwrap();
        conn.execute_batch(&schema_sql()).unwrap();
        (dir, conn)
    }

    fn revision(conn: &PortableDatabase) -> i64 {
        conn.query(
            "SELECT value FROM tako_workflow_meta WHERE name = 'schedule_revision'",
            &[],
        )
        .unwrap()[0][0]
            .as_int()
            .unwrap()
    }
    #[test]
    fn a_replacement_keeps_the_running_releases_schedules_and_revision() {
        let (_live_dir, live) = store();
        live.execute_batch(
            "UPDATE tako_workflow_meta SET value = 12 WHERE name = 'schedule_revision';
             INSERT INTO tako_workflow_schedule_releases VALUES ('shop', 'b2', 12);
             INSERT INTO tako_workflow_schedules VALUES ('shop', 'nightly', '0 3 * * *', 900, 2);",
        )
        .unwrap();
        // An older copy: a lower counter, an earlier build's schedules.
        let (_old_dir, old) = store();
        old.execute_batch(
            "UPDATE tako_workflow_meta SET value = 4 WHERE name = 'schedule_revision';
             INSERT INTO tako_workflow_schedule_releases VALUES ('shop', 'b1', 4);
             INSERT INTO tako_workflow_schedules VALUES ('shop', 'hourly', '0 * * * *', 100, NULL);",
        )
        .unwrap();

        keep_live_schedules(&live, &old).unwrap();

        assert_eq!(revision(&old), 12);
        let release = old
            .query(
                "SELECT build, revision FROM tako_workflow_schedule_releases WHERE app = 'shop'",
                &[],
            )
            .unwrap();
        assert_eq!(release.len(), 1);
        assert_eq!(release[0][0].to_text(), Some("b2"));
        assert_eq!(release[0][1].as_int(), Some(12));
        let names = old
            .query(
                "SELECT name FROM tako_workflow_schedules WHERE app = 'shop'",
                &[],
            )
            .unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0][0].to_text(), Some("nightly"));
    }

    #[test]
    fn a_replacement_never_lowers_the_revision_counter() {
        let (_live_dir, live) = store();
        let (_new_dir, newer) = store();
        newer
            .execute_batch(
                "UPDATE tako_workflow_meta SET value = 30 WHERE name = 'schedule_revision';",
            )
            .unwrap();

        keep_live_schedules(&live, &newer).unwrap();

        assert_eq!(revision(&newer), 30);
    }
}
