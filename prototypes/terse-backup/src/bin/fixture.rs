//! Subprocess fixture: stock SQLite writer or embedded capture owner, driven over stdin.

use std::{
    io::{self, BufRead, Write},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use tako_terse_prototype::Capture;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).context("missing mode")?;
    let database = Path::new(args.get(2).context("missing database")?);
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
            let spool = Path::new(args.get(3).context("missing spool")?);
            let mut capture = Capture::open(database, spool)?;
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
    writeln!(stdout, "{message}")?;
    stdout.flush()?;
    Ok(())
}
