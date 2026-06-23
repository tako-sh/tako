pub(crate) use std::fs;
use std::io::Write;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::{io::BufRead, thread};
pub(crate) use tempfile::TempDir;

fn workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manifest_dir.to_path_buf())
}

pub(crate) fn apply_coverage_env(cmd: &mut Command) {
    let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") else {
        return;
    };
    let profile = PathBuf::from(profile);
    if profile.is_absolute() {
        return;
    }
    let absolute = workspace_root().join(profile);
    if let Some(parent) = absolute.parent() {
        let _ = fs::create_dir_all(parent);
    }
    cmd.env("LLVM_PROFILE_FILE", absolute);
}

/// Helper to run tako CLI commands
pub(crate) fn run_tako(args: &[&str], cwd: &Path) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    cmd.output().expect("Failed to run tako command")
}

/// Helper to run tako CLI commands with stdin input
pub(crate) fn run_tako_with_stdin(args: &[&str], cwd: &Path, input: &str) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    let mut child = cmd.spawn().expect("Failed to spawn tako command");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes()).ok();
    }

    child
        .wait_with_output()
        .expect("Failed to wait for tako command")
}

pub(crate) fn run_tako_with_env(
    args: &[&str],
    cwd: &Path,
    home: &Path,
    tako_home: &Path,
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("TAKO_HOME", tako_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    cmd.output().expect("Failed to run tako command")
}

pub(crate) fn run_tako_with_extra_env(
    args: &[&str],
    cwd: &Path,
    envs: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(args)
        .current_dir(cwd)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    cmd.output().expect("Failed to run tako command")
}

pub(crate) fn run_tako_with_stdin_and_env(
    args: &[&str],
    cwd: &Path,
    input: &str,
    home: &Path,
    tako_home: &Path,
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("TAKO_HOME", tako_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    let mut child = cmd.spawn().expect("Failed to spawn tako command");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes()).ok();
    }

    child
        .wait_with_output()
        .expect("Failed to wait for tako command")
}

/// Helper to get stdout as string
pub(crate) fn stdout_str(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Helper to get stderr as string
pub(crate) fn stderr_str(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

pub(crate) fn setup_minimal_bun_project(project_dir: &Path) {
    fs::write(project_dir.join("bun.lockb"), "").unwrap();
    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "dev-test-app", "version": "1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        project_dir.join("index.ts"),
        r#"export default { fetch() { return new Response("ok"); } };"#,
    )
    .unwrap();
}

pub(crate) struct FakeDevServer {
    sock_path: PathBuf,
    running: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl FakeDevServer {
    pub(crate) fn start(tako_home: &Path) -> Option<Self> {
        fs::create_dir_all(tako_home).unwrap();
        let sock_path = tako_home.join("dev-server.sock");
        let _ = fs::remove_file(&sock_path);

        let running = Arc::new(AtomicBool::new(true));
        let running2 = running.clone();
        let sock_path2 = sock_path.clone();
        let listener = std::os::unix::net::UnixListener::bind(&sock_path2).ok()?;
        listener
            .set_nonblocking(true)
            .expect("set_nonblocking on fake dev-server sock");

        let join = thread::spawn(move || {
            while running2.load(Ordering::SeqCst) {
                let (stream, _) = match listener.accept() {
                    Ok(x) => x,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = stream.set_nonblocking(false);
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;

                let mut line = String::new();
                while reader
                    .read_line(&mut line)
                    .ok()
                    .filter(|n| *n > 0)
                    .is_some()
                {
                    let v: serde_json::Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => {
                            line.clear();
                            continue;
                        }
                    };
                    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let resp = match typ {
                        "Ping" => serde_json::json!({ "type": "Pong" }),
                        "ListApps" => serde_json::json!({
                            "type": "Apps",
                            "apps": [
                                { "app_name": "a", "hosts": ["a.test"], "upstream_port": 1234, "pid": 111 },
                                { "app_name": "b", "hosts": ["b.test"], "upstream_port": 2222 }
                            ]
                        }),
                        "Info" => serde_json::json!({
                                "type": "Info",
                                "info": {
                                "listen": "127.0.0.1:8443",
                                "port": 8443,
                                "advertised_ip": "127.0.0.1",
                                "local_dns_enabled": true,
                                "local_dns_port": 53535
                            }
                        }),
                        "UnregisterApp" => serde_json::json!({
                            "type": "AppUnregistered",
                            "project_dir": v.get("project_dir").and_then(|a| a.as_str()).unwrap_or(""),
                        }),
                        "RegisterApp" => serde_json::json!({
                            "type": "AppRegistered",
                            "app_name": v.get("app_name").and_then(|a| a.as_str()).unwrap_or(""),
                            "project_dir": v.get("project_dir").and_then(|a| a.as_str()).unwrap_or(""),
                            "url": format!("https://{}.test/", v.get("app_name").and_then(|a| a.as_str()).unwrap_or("app")),
                        }),
                        "SetAppStatus" => serde_json::json!({
                            "type": "AppStatusUpdated",
                            "project_dir": v.get("project_dir").and_then(|a| a.as_str()).unwrap_or(""),
                            "status": v.get("status").and_then(|a| a.as_str()).unwrap_or(""),
                        }),
                        "HandoffApp" => serde_json::json!({
                            "type": "AppHandedOff",
                            "project_dir": v.get("project_dir").and_then(|a| a.as_str()).unwrap_or(""),
                        }),
                        "ListRegisteredApps" => serde_json::json!({
                            "type": "RegisteredApps",
                            "apps": []
                        }),
                        "RestartApp" => serde_json::json!({
                            "type": "AppRestarting",
                            "project_dir": v.get("project_dir").and_then(|a| a.as_str()).unwrap_or(""),
                        }),
                        "StopServer" => {
                            running2.store(false, Ordering::SeqCst);
                            serde_json::json!({ "type": "Stopping" })
                        }
                        _ => serde_json::json!({ "type": "Error", "message": "unknown request" }),
                    };
                    let _ = writeln!(writer, "{}", resp);
                    line.clear();
                    if typ == "StopServer" {
                        break;
                    }
                }
            }
        });

        // Wait until the socket exists so callers can connect reliably.
        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        Some(Self {
            sock_path,
            running,
            join: Some(join),
        })
    }
}

impl Drop for FakeDevServer {
    fn drop(&mut self) {
        // Best effort: signal stop and join.
        self.running.store(false, Ordering::SeqCst);
        // Wake the accept loop if it's sleeping/polling.
        let _ = std::os::unix::net::UnixStream::connect(&self.sock_path);
        let _ = std::fs::remove_file(&self.sock_path);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub(crate) fn dev_daemon_test_lock() -> &'static StdMutex<()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
}
