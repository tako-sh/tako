//! Exercise the restore barrier with real primary and standby processes.
mod support;

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Standby(Child);
impl Drop for Standby {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn primary_refuses_restore_until_the_standby_process_exits() {
    let primary = support::TestServer::start();
    let data = primary.data_dir();
    let sentinel = data.join("untouched");
    std::fs::write(&sentinel, b"original data").unwrap();
    let mut standby = Standby(
        Command::new(env!("CARGO_BIN_EXE_tako-server"))
            .arg("--standby")
            .arg("--data-dir")
            .arg(data)
            .arg("--socket")
            .arg(data.join("tako.sock"))
            .args(["--http-port", "0", "--https-port", "0", "--no-acme"])
            .env("TAKO_TEST_SKIP_CLOUDFLARE_IP_REFRESH", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    assert!(
        support::wait_for(Duration::from_secs(15), || {
            assert!(
                standby.0.try_wait().unwrap().is_none(),
                "standby exited before readiness"
            );
            let Ok(lock) = OpenOptions::new()
                .read(true)
                .write(true)
                .open(data.join("standby.lock"))
            else {
                return false;
            };
            let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
        }),
        "standby never acquired its shared lease"
    );
    let command = serde_json::json!({"command":"restore_backup", "app":"demo/production", "backup_id":"test"});
    let refused = primary.send_command(&command);
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("Stop the standby first"),
        "{refused}"
    );
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"original data");
    drop(standby);
    let result = primary.send_command(&command);
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("Backups are not configured"),
        "{result}"
    );
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"original data");
}
