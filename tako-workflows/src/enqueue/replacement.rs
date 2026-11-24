//! Preparing a replacement copy of a workflow database, written by a
//! restore, an import or a reset, before the server swaps it in under
//! workflow runtimes that keep the database open.

use rusqlite::{Connection, params};

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
pub fn keep_live_schedules(live: &Connection, replacement: &Connection) -> rusqlite::Result<()> {
    let complete: bool = live.query_row(
        "SELECT count(*) = 3 FROM main.sqlite_schema WHERE type = 'table' AND name IN
             ('tako_workflow_meta', 'tako_workflow_schedule_releases', 'tako_workflow_schedules')",
        [],
        |row| row.get(0),
    )?;
    if !complete {
        return Ok(());
    }
    let revision: i64 = live
        .query_row(
            "SELECT MAX(
                 COALESCE((SELECT value FROM tako_workflow_meta WHERE name = 'schedule_revision'), 0),
                 COALESCE((SELECT MAX(revision) FROM tako_workflow_schedule_releases), 0))",
            [],
            |row| row.get(0),
        )?;
    let releases: Vec<(String, String, i64)> = live
        .prepare("SELECT app, build, revision FROM tako_workflow_schedule_releases")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    type Schedule = (String, String, String, Option<i64>, Option<i64>);
    let schedules: Vec<Schedule> = live
        .prepare("SELECT app, name, cron, last_run_at, max_attempts FROM tako_workflow_schedules")?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let tx = replacement.unchecked_transaction()?;
    tx.execute(
        "UPDATE tako_workflow_meta
         SET value = MAX(value, ?1,
             COALESCE((SELECT MAX(revision) FROM tako_workflow_schedule_releases), 0))
         WHERE name = 'schedule_revision'",
        params![revision],
    )?;
    tx.execute("DELETE FROM tako_workflow_schedule_releases", [])?;
    tx.execute("DELETE FROM tako_workflow_schedules", [])?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO tako_workflow_schedule_releases (app, build, revision) VALUES (?1, ?2, ?3)",
        )?;
        for (app, build, revision) in &releases {
            insert.execute(params![app, build, revision])?;
        }
        let mut insert = tx.prepare(
            "INSERT INTO tako_workflow_schedules (app, name, cron, last_run_at, max_attempts)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (app, name, cron, last_run_at, max_attempts) in &schedules {
            insert.execute(params![app, name, cron, last_run_at, max_attempts])?;
        }
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&schema_sql()).unwrap();
        conn
    }

    fn revision(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT value FROM tako_workflow_meta WHERE name = 'schedule_revision'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_replacement_keeps_the_running_releases_schedules_and_revision() {
        let live = store();
        live.execute_batch(
            "UPDATE tako_workflow_meta SET value = 12 WHERE name = 'schedule_revision';
             INSERT INTO tako_workflow_schedule_releases VALUES ('shop', 'b2', 12);
             INSERT INTO tako_workflow_schedules VALUES ('shop', 'nightly', '0 3 * * *', 900, 2);",
        )
        .unwrap();
        // An older copy: a lower counter, an earlier build's schedules.
        let old = store();
        old.execute_batch(
            "UPDATE tako_workflow_meta SET value = 4 WHERE name = 'schedule_revision';
             INSERT INTO tako_workflow_schedule_releases VALUES ('shop', 'b1', 4);
             INSERT INTO tako_workflow_schedules VALUES ('shop', 'hourly', '0 * * * *', 100, NULL);",
        )
        .unwrap();

        keep_live_schedules(&live, &old).unwrap();

        assert_eq!(revision(&old), 12);
        let release: (String, i64) = old
            .query_row(
                "SELECT build, revision FROM tako_workflow_schedule_releases WHERE app = 'shop'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(release, ("b2".to_string(), 12));
        let names: Vec<String> = old
            .prepare("SELECT name FROM tako_workflow_schedules WHERE app = 'shop'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(names, ["nightly"]);
    }

    #[test]
    fn a_replacement_never_lowers_the_revision_counter() {
        let live = store();
        let newer = store();
        newer
            .execute_batch(
                "UPDATE tako_workflow_meta SET value = 30 WHERE name = 'schedule_revision';",
            )
            .unwrap();

        keep_live_schedules(&live, &newer).unwrap();

        assert_eq!(revision(&newer), 30);
    }
}
