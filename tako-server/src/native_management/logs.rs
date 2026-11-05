use super::*;
use crate::operations::logs::LogRead;

const SCAN_CHUNK: usize = 64 * 1024;
const MAX_SCAN_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn read(state: &ServerState, metadata: &Logs) -> Result<LogRead, ()> {
    let Some(since) = metadata.since else {
        return state
            .read_logs(
                &metadata.app,
                metadata.previous,
                metadata.current,
                None,
                metadata.max_bytes,
            )
            .map_err(|_| ());
    };
    let time = time::OffsetDateTime::from_unix_timestamp(since.max(0)).map_err(|_| ())?;
    let cutoff = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        time.year(),
        u8::from(time.month()),
        time.day(),
        time.hour(),
        time.minute(),
        time.second()
    );
    let sizes = state
        .read_logs(&metadata.app, u64::MAX, u64::MAX, None, 1)
        .map_err(|_| ())?;
    let mut result = LogRead {
        bytes: Vec::new(),
        previous_offset: sizes.previous_len,
        current_offset: sizes.current_len,
        previous_len: sizes.previous_len,
        current_len: sizes.current_len,
        truncated: false,
    };
    let mut scanned = 0;
    for previous in [true, false] {
        let len = if previous {
            sizes.previous_len
        } else {
            sizes.current_len
        };
        let mut offset = 0;
        let mut line = Vec::new();
        let mut line_too_long = false;
        while offset < len {
            let count = SCAN_CHUNK.min((len - offset).min(usize::MAX as u64) as usize);
            scanned += count;
            if scanned > MAX_SCAN_BYTES {
                return Err(());
            }
            let chunk = state
                .read_logs(
                    &metadata.app,
                    if previous { offset } else { sizes.previous_len },
                    if previous { sizes.current_len } else { offset },
                    None,
                    count,
                )
                .map_err(|_| ())?;
            if chunk.bytes.is_empty() {
                break;
            }
            offset += chunk.bytes.len() as u64;
            for byte in chunk.bytes {
                if line.len() <= metadata.max_bytes.max(19) {
                    line.push(byte);
                } else {
                    line_too_long = true;
                }
                if byte == b'\n' {
                    if append_line(
                        &mut result,
                        &line,
                        line_too_long,
                        &cutoff,
                        metadata.max_bytes,
                    ) {
                        return Ok(result);
                    }
                    line.clear();
                    line_too_long = false;
                }
            }
        }
        if !line.is_empty()
            && append_line(
                &mut result,
                &line,
                line_too_long,
                &cutoff,
                metadata.max_bytes,
            )
        {
            return Ok(result);
        }
    }
    Ok(result)
}

fn append_line(
    result: &mut LogRead,
    line: &[u8],
    too_long: bool,
    cutoff: &str,
    max_bytes: usize,
) -> bool {
    let old = line
        .get(..19)
        .and_then(|stamp| std::str::from_utf8(stamp).ok())
        .is_some_and(|stamp| {
            stamp.as_bytes()[4] == b'-' && stamp.as_bytes()[10] == b'T' && stamp < cutoff
        });
    if old {
        return false;
    }
    if too_long || result.bytes.len().saturating_add(line.len()) > max_bytes {
        result.truncated = true;
        return true;
    }
    result.bytes.extend_from_slice(line);
    false
}
