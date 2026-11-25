//! Subprocess fixture: stock SQLite writer or embedded capture owner, driven over stdin.

use std::{
    io::{self, BufRead, Write},
    path::Path,
    time::Duration,
};

use super::capture::Capture;
use anyhow::{Context, Result, bail};
use rusqlite::Connection;

#[test]
fn subprocess() -> Result<()> {
    let Ok(mode) = std::env::var("TAKO_LTX_FIXTURE_MODE") else {
        return Ok(());
    };
    let db = std::env::var_os("TAKO_LTX_FIXTURE_DB").context("missing database")?;
    let database = Path::new(&db);
    let stdin = io::stdin();
    match mode.as_str() {
        "writer" => {
            let connection = Connection::open(database)?;
            connection.busy_timeout(Duration::from_secs(5))?;
            connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=1; CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, value TEXT NOT NULL)")?;
            reply("ready")?;
            for line in stdin.lock().lines() {
                connection.execute_batch(&line?)?;
                reply("ok")?;
            }
        }
        "capture" => {
            let spool_dir = std::env::var_os("TAKO_LTX_FIXTURE_SPOOL").context("missing spool")?;
            let spool = Path::new(&spool_dir);
            let mut capture = Capture::open(database, spool, 0)?;
            reply("ready")?;
            for line in stdin.lock().lines() {
                match line?.as_str() {
                    "sync" => reply(&capture.sync()?.to_string())?,
                    _ => bail!("unknown capture command"),
                }
            }
        }
        _ => bail!("unknown mode"),
    }
    Ok(())
}

fn reply(message: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "LTX-FIXTURE>{message}")?;
    stdout.flush()?;
    Ok(())
}
