use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::management_http::{LogCursor, ManagementClient};

pub(super) type SharedLogSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

const LOG_FETCH_MAX_BYTES: usize = 24 * 1024 * 1024;
const LOG_TAIL_CHUNK_BYTES: usize = 256 * 1024;
const LOG_TAIL_BACKLOG_BYTES: usize = 64 * 1024;
const LOG_TAIL_BACKLOG_LINES: usize = 10;

pub(super) async fn stream_remote_logs(
    host: &str,
    app: &str,
    sink: SharedLogSink,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = ManagementClient::new(host).await?;
    let metadata = client
        .fetch_log_bytes(app, LogCursor::default(), None, 0)
        .await?;
    let mut cursor = LogCursor {
        previous: metadata.previous_len,
        current: metadata.current_len,
    };

    if metadata.current_len > 0 {
        let start = metadata
            .current_len
            .saturating_sub(LOG_TAIL_BACKLOG_BYTES as u64);
        let backlog = client
            .fetch_log_bytes(
                app,
                LogCursor {
                    previous: metadata.previous_len,
                    current: start,
                },
                None,
                LOG_TAIL_BACKLOG_BYTES,
            )
            .await?;
        let tail = last_complete_lines(&backlog.bytes, LOG_TAIL_BACKLOG_LINES, start > 0);
        if !tail.is_empty() {
            sink(&tail);
        }
        cursor = backlog.cursor;
    }

    loop {
        let fetch = client
            .fetch_log_bytes(app, cursor, None, LOG_TAIL_CHUNK_BYTES)
            .await?;
        if !fetch.bytes.is_empty() {
            sink(&fetch.bytes);
        }
        cursor = fetch.cursor;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub(super) async fn collect_remote_log_bytes(
    host: &str,
    app: &str,
    days: u32,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let since = cutoff_unix_secs_for_days(days);
    let mut client = ManagementClient::new(host).await?;
    let fetch = client
        .fetch_log_bytes(app, LogCursor::default(), Some(since), LOG_FETCH_MAX_BYTES)
        .await?;
    if fetch.truncated {
        return Err(format!(
            "log response exceeded {} bytes; retry with a smaller --days value",
            LOG_FETCH_MAX_BYTES
        )
        .into());
    }
    Ok(fetch.bytes.to_vec())
}

fn cutoff_unix_secs_for_days(days: u32) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let window = u64::from(days).saturating_mul(86_400);
    now.saturating_sub(window) as i64
}

fn last_complete_lines(bytes: &[u8], line_count: usize, drop_first_partial: bool) -> Vec<u8> {
    if bytes.is_empty() || line_count == 0 {
        return Vec::new();
    }

    let start = if drop_first_partial {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1)
    } else {
        0
    };
    let bytes = &bytes[start..];
    if bytes.is_empty() {
        return Vec::new();
    }

    let mut lines_seen = 0;
    let mut start_index = 0;
    for (index, byte) in bytes.iter().enumerate().rev() {
        if *byte == b'\n' && index + 1 < bytes.len() {
            lines_seen += 1;
            if lines_seen == line_count {
                start_index = index + 1;
                break;
            }
        }
    }

    bytes[start_index..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_complete_lines_returns_last_n_lines() {
        let lines = last_complete_lines(b"one\ntwo\nthree\n", 2, false);

        assert_eq!(String::from_utf8(lines).unwrap(), "two\nthree\n");
    }

    #[test]
    fn last_complete_lines_drops_partial_first_line_when_reading_from_middle() {
        let lines = last_complete_lines(b"rtial\none\ntwo\n", 2, true);

        assert_eq!(String::from_utf8(lines).unwrap(), "one\ntwo\n");
    }
}
