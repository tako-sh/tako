use super::framing::{read_worker_frame_sync, write_worker_frame_sync};
use super::protocol::handle_request_bytes;

pub(crate) fn run_stdio() -> Result<(), String> {
    apply_worker_resource_policy();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    while let Some(input) = read_worker_frame_sync(&mut reader)? {
        let response = handle_request_bytes(&input);
        let output = serde_json::to_vec(&response)
            .map_err(|error| format!("encode image worker response: {error}"))?;
        write_worker_frame_sync(&mut writer, &output)?;
    }

    Ok(())
}

#[cfg(unix)]
fn apply_worker_resource_policy() {
    // Keep image CPU below the proxy and app processes under contention.
    unsafe {
        let _ = libc::nice(10);
    }
}

#[cfg(not(unix))]
fn apply_worker_resource_policy() {}
