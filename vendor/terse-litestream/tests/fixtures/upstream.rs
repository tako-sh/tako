use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

pub struct Upstream {
    process: Child,
    _root: tempfile::TempDir,
    pub replica: PathBuf,
    socket: PathBuf,
    database: PathBuf,
}

impl Upstream {
    pub fn start(database: &Path) -> Result<Self> {
        let root = tempfile::tempdir_in("/tmp")?;
        let replica = root.path().join("replica");
        let socket = root.path().join("control.sock");
        let config = root.path().join("config.json");
        let log = root.path().join("daemon.log");
        fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "socket": {"enabled":true,"path":socket}, "levels": [],
                "snapshot":{"interval":"24h"}, "retention":{"enabled":false},
                "dbs":[{"path":database,"monitor-interval":"24h","checkpoint-interval":"0s","min-checkpoint-page-count":1000,"replica":{"type":"file","path":replica,"sync-interval":"24h"}}]
            }))?,
        )?;
        let process = command()?
            .args(["replicate", "-config"])
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&log)?))
            .stderr(Stdio::from(fs::File::options().append(true).open(&log)?))
            .spawn()?;
        let mut daemon = Self {
            process,
            _root: root,
            replica,
            socket,
            database: database.into(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            ensure!(
                daemon.process.try_wait()?.is_none(),
                "upstream exited: {}",
                fs::read_to_string(&log)?
            );
            if daemon.socket.exists() {
                let output = command()?
                    .args(["info", "-json", "-socket"])
                    .arg(&daemon.socket)
                    .output()?;
                if output.status.success() {
                    return Ok(daemon);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        anyhow::bail!(
            "upstream did not become ready: {}",
            fs::read_to_string(log)?
        )
    }

    pub fn sync(&self) -> Result<u64> {
        let output = command()?
            .args(["sync", "-wait", "-timeout", "10", "-json", "-socket"])
            .arg(&self.socket)
            .arg(&self.database)
            .output()?;
        let value: serde_json::Value = serde_json::from_slice(&success(output)?)?;
        value["txid"]
            .as_u64()
            .context("upstream sync returned no position")
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

pub fn restore(replica: &Path, output: &Path, target: Option<u64>) -> Result<()> {
    let mut process = command()?;
    process.arg("restore");
    if let Some(target) = target {
        process.args(["-txid", &format!("{target:016x}")]);
    }
    success(
        process
            .arg("-o")
            .arg(output)
            .arg(format!("file://{}", replica.display()))
            .output()?,
    )?;
    Ok(())
}

pub fn command() -> Result<Command> {
    let binary = std::env::var_os("LITESTREAM_BINARY").context(
        "LITESTREAM_BINARY must name the pinned upstream 0.5.17 binary; run scripts/verify.sh",
    )?;
    Ok(Command::new(binary))
}

pub fn verify_version() -> Result<()> {
    let version = success(command()?.arg("version").output()?)?;
    ensure!(
        String::from_utf8(version)?.trim() == "0.5.17",
        "compatibility tests require Litestream 0.5.17"
    );
    Ok(())
}

fn success(output: Output) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "upstream failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}
