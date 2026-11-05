use super::*;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn command(script: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    command
}

struct GroupCleanup(i32);
impl Drop for GroupCleanup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

fn assert_reaped(pid: u32) {
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[tokio::test]
async fn model_stdin_closes_before_owned_capture_reads_the_result() {
    let child = command("cat").spawn().unwrap();
    let child = write_stdin(child, b"{\"services\":{}}", Duration::from_secs(1))
        .await
        .unwrap();
    assert!(child.stdin.is_none());
    let output = capture(child, Some(Duration::from_secs(1)), StdoutMode::Complete)
        .await
        .unwrap();
    assert!(output.status.unwrap().success());
    assert_eq!(output.stdout, b"{\"services\":{}}");
}

#[tokio::test]
async fn model_stdin_timeout_kills_and_reaps_before_returning() {
    let child = command("exec sleep 30").spawn().unwrap();
    let pid = child.id().unwrap();
    let _cleanup = GroupCleanup(pid as i32);
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    let result = write_stdin(child, &bytes, Duration::from_millis(30)).await;
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::TimedOut);
    assert_reaped(pid);
}

#[tokio::test]
async fn model_stdin_write_failure_kills_and_reaps_before_returning() {
    let child = command("exec 0<&-; exec sleep 30").spawn().unwrap();
    let pid = child.id().unwrap();
    let _cleanup = GroupCleanup(pid as i32);
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    let result = write_stdin(child, &bytes, Duration::from_secs(2)).await;
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::BrokenPipe);
    assert_reaped(pid);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn blocked_model_stdin_observes_drain_and_cleans_group_before_unlocking() {
    let directory = tempfile::tempdir().unwrap();
    let child = command("sh -c 'echo $$ > descendant; exec sleep 30' & wait")
        .current_dir(directory.path())
        .spawn()
        .unwrap();
    let leader = child.id().unwrap();
    let _cleanup = GroupCleanup(leader as i32);
    let descendant = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(directory.path().join("descendant"))
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(unsafe { libc::getpgid(descendant) }, leader as i32);
    let flag = Arc::new(AtomicBool::new(false));
    let lock = tokio::sync::Mutex::new(());
    let guard = lock.lock().await;
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    let mut writing = Box::pin(with_cancellation(
        flag.clone(),
        write_stdin(child, &bytes, Duration::from_secs(30)),
    ));
    tokio::select! {
        _ = &mut writing => panic!("large input should block while child ignores stdin"),
        _ = tokio::time::sleep(Duration::from_millis(30)) => (),
    }
    flag.store(true, Ordering::Release);
    let result = tokio::time::timeout(Duration::from_secs(2), writing)
        .await
        .unwrap();
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
    assert!(lock.try_lock().is_err());
    assert_reaped(leader);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match std::fs::read_to_string(format!("/proc/{descendant}/stat")) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Ok(stat) if stat.rsplit_once(") ").unwrap().1.starts_with('Z') => break,
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap();
    drop(guard);
    assert!(lock.try_lock().is_ok());
}
