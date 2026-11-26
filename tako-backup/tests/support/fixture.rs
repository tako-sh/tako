//! Subprocess fixture: Turso writer or committed-page capture owner, driven over stdin.

use std::{
    io::{self, BufRead, Write},
    path::Path,
};

use super::capture::Capture;
use anyhow::{Context, Result, bail};
use tako_db::PortableDatabase;

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
            std::fs::File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(database)?;
            let connection = PortableDatabase::open(database)?;
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
            )?;
            connection.checkpoint()?;
            reply("ready")?;
            for line in stdin.lock().lines() {
                let line = line?;
                if let Some(sql) = line.strip_prefix("uncommitted ") {
                    connection.execute_batch(sql)?;
                } else {
                    connection.execute_batch(&line)?;
                    connection.checkpoint()?;
                }
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
