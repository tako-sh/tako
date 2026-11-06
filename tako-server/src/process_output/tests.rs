use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::Command;

fn waiting_child() -> Child {
    let mut command = Command::new("sh");
    command
        .args(["-c", "exec sleep 30"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    command.spawn().unwrap()
}

#[cfg(unix)]
fn assert_reaped(pid: u32) {
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[tokio::test]
async fn timeout_reaps_the_child_before_returning() {
    let child = waiting_child();
    let pid = child.id().unwrap();
    let output = capture(child, Some(Duration::from_millis(30)))
        .await
        .unwrap();
    assert!(output.status.is_none());
    #[cfg(unix)]
    assert_reaped(pid);
}

#[tokio::test]
async fn drain_reaps_preparation_before_releasing_a_detached_mutation_guard() {
    let flag = Arc::new(AtomicBool::new(false));
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let (started, ready) = tokio::sync::oneshot::channel();
    let (completed, result) = tokio::sync::oneshot::channel();
    let task_flag = flag.clone();
    let task_lock = lock.clone();
    let worker = tokio::spawn(async move {
        let _guard = task_lock.lock().await;
        let child = waiting_child();
        let pid = child.id().unwrap();
        started.send(pid).unwrap();
        let output = during_preparation(task_flag, capture(child, Some(PREPARATION_TIMEOUT))).await;
        #[cfg(unix)]
        assert_reaped(pid);
        assert!(task_lock.try_lock().is_err());
        completed.send(output.err().unwrap().kind()).unwrap();
    });
    let waiter = tokio::spawn(async move { worker.await });
    let pid = ready.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(lock.try_lock().is_err());
    flag.store(true, Ordering::Release);
    let error = tokio::time::timeout(Duration::from_secs(2), result)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(error, io::ErrorKind::Interrupted);
    let _guard = tokio::time::timeout(Duration::from_secs(2), lock.lock())
        .await
        .unwrap();
    #[cfg(unix)]
    assert_reaped(pid);
}

#[tokio::test]
async fn cancellation_is_scoped_to_the_preparation_future() {
    let flag = Arc::new(AtomicBool::new(true));
    assert!(
        during_preparation(flag, async { check_cancellation() })
            .await
            .is_err()
    );
    assert!(check_cancellation().is_ok());
    let child = Command::new("sh")
        .args(["-c", "printf success"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let output = capture(child, Some(Duration::from_secs(1))).await.unwrap();
    assert!(output.status.unwrap().success());
    assert_eq!(output.stdout, b"success");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn drain_kills_nested_package_manager_processes_in_the_owned_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new("sh");
    command
        .args(["-c", "sh -c 'echo $$ > descendant; exec sleep 30' & wait"])
        .current_dir(dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let child = command.spawn().unwrap();
    let leader = child.id().unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let task_flag = flag.clone();
    let task = tokio::spawn(async move {
        during_preparation(task_flag, capture(child, Some(PREPARATION_TIMEOUT))).await
    });
    let descendant = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(dir.path().join("descendant"))
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
    flag.store(true, Ordering::Release);
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
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
}
