use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::ServerState;
use crate::release::{app_root, validate_app_name};

pub(crate) struct LogRead {
    pub(crate) bytes: Vec<u8>,
    pub(crate) previous_offset: u64,
    pub(crate) current_offset: u64,
    pub(crate) previous_len: u64,
    pub(crate) current_len: u64,
    pub(crate) truncated: bool,
}

impl ServerState {
    pub(crate) fn read_logs(
        &self,
        app: &str,
        previous_offset: u64,
        current_offset: u64,
        since_unix_secs: Option<i64>,
        max_bytes: usize,
    ) -> Result<LogRead, String> {
        read_logs(
            &self.runtime.data_dir,
            app,
            previous_offset,
            current_offset,
            since_unix_secs,
            max_bytes,
        )
    }
}

fn read_logs(
    data_dir: &Path,
    app: &str,
    previous_offset: u64,
    current_offset: u64,
    since_unix_secs: Option<i64>,
    max_bytes: usize,
) -> Result<LogRead, String> {
    validate_app_name(app)?;
    let log_dir = app_root(data_dir, app).join("logs");
    let previous_path = log_dir.join("previous.log");
    let current_path = log_dir.join("current.log");
    let previous_len = file_len(&previous_path)?;
    let current_len = file_len(&current_path)?;

    if let Some(since_unix_secs) = since_unix_secs {
        return read_logs_since(
            &[previous_path, current_path],
            previous_len,
            current_len,
            since_unix_secs,
            max_bytes,
        );
    }

    read_logs_from_offsets(
        &previous_path,
        &current_path,
        previous_len,
        current_len,
        previous_offset,
        current_offset,
        max_bytes,
    )
}

fn read_logs_since(
    paths: &[PathBuf; 2],
    previous_len: u64,
    current_len: u64,
    since_unix_secs: i64,
    max_bytes: usize,
) -> Result<LogRead, String> {
    let cutoff = format_unix_secs_utc(since_unix_secs);
    let mut bytes = Vec::new();
    let mut truncated = false;

    for path in paths {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("read log file {}: {error}", path.display())),
        };
        append_lines_since(path, file, &cutoff, max_bytes, &mut bytes, &mut truncated)?;

        if truncated {
            break;
        }
    }

    Ok(LogRead {
        bytes,
        previous_offset: previous_len,
        current_offset: current_len,
        previous_len,
        current_len,
        truncated,
    })
}

fn read_logs_from_offsets(
    previous_path: &Path,
    current_path: &Path,
    previous_len: u64,
    current_len: u64,
    previous_offset: u64,
    current_offset: u64,
    max_bytes: usize,
) -> Result<LogRead, String> {
    let current_rotated = current_len < current_offset;
    let previous_start = if current_rotated {
        current_offset.min(previous_len)
    } else {
        previous_offset.min(previous_len)
    };
    let current_start = if current_rotated {
        0
    } else {
        current_offset.min(current_len)
    };

    let mut bytes = Vec::new();
    let mut previous_next = previous_start;
    let mut current_next = current_start;
    let mut truncated = false;

    append_file_range(
        previous_path,
        previous_start,
        previous_len,
        max_bytes,
        &mut bytes,
        &mut previous_next,
        &mut truncated,
    )?;
    if !truncated {
        append_file_range(
            current_path,
            current_start,
            current_len,
            max_bytes,
            &mut bytes,
            &mut current_next,
            &mut truncated,
        )?;
    }

    Ok(LogRead {
        bytes,
        previous_offset: previous_next,
        current_offset: current_next,
        previous_len,
        current_len,
        truncated,
    })
}

fn append_lines_since(
    path: &Path,
    file: std::fs::File,
    cutoff: &str,
    max_bytes: usize,
    out: &mut Vec<u8>,
    truncated: &mut bool,
) -> Result<(), String> {
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();

    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("read log file {}: {error}", path.display()))?;
        if read == 0 {
            return Ok(());
        }

        if !line_matches_cutoff(&String::from_utf8_lossy(&line), cutoff) {
            continue;
        }
        if out.len().saturating_add(line.len()) > max_bytes {
            *truncated = true;
            return Ok(());
        }
        out.extend_from_slice(&line);
    }
}

fn append_file_range(
    path: &Path,
    start: u64,
    len: u64,
    max_bytes: usize,
    out: &mut Vec<u8>,
    next_offset: &mut u64,
    truncated: &mut bool,
) -> Result<(), String> {
    if start >= len || out.len() >= max_bytes {
        *next_offset = start.min(len);
        return Ok(());
    }

    let remaining_capacity = max_bytes - out.len();
    let available = (len - start).min(remaining_capacity as u64);
    if available < len - start {
        *truncated = true;
    }
    if available == 0 {
        *next_offset = start;
        return Ok(());
    }

    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            *next_offset = start;
            return Ok(());
        }
        Err(error) => return Err(format!("open log file {}: {error}", path.display())),
    };
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("seek log file {}: {error}", path.display()))?;
    let mut limited = file.take(available);
    let before = out.len();
    limited
        .read_to_end(out)
        .map_err(|error| format!("read log file {}: {error}", path.display()))?;
    *next_offset = start + (out.len() - before) as u64;
    Ok(())
}

fn file_len(path: &Path) -> Result<u64, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(format!("stat log file {}: {error}", path.display())),
    }
}

fn line_matches_cutoff(line: &str, cutoff: &str) -> bool {
    let Some(timestamp) = line.get(..19) else {
        return true;
    };
    let timestamp = timestamp.as_bytes();
    if timestamp.get(4) != Some(&b'-') || timestamp.get(10) != Some(&b'T') {
        return true;
    }
    std::str::from_utf8(timestamp).is_ok_and(|value| value >= cutoff)
}

fn format_unix_secs_utc(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let days = secs / 86_400;
    let time_secs = secs % 86_400;
    let hours = time_secs / 3_600;
    let minutes = (time_secs % 3_600) / 60;
    let seconds = time_secs % 60;

    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[test]
    fn read_logs_filters_by_cutoff_and_advances_offsets() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let log_dir = temp
            .path()
            .join("apps")
            .join("demo/production")
            .join("logs");
        fs::create_dir_all(&log_dir).expect("log dir");
        fs::write(
            log_dir.join("previous.log"),
            "2026-05-17T09:00:00.000Z [out] [old] old\n",
        )
        .expect("previous log");
        fs::write(
            log_dir.join("current.log"),
            "2026-05-18T09:00:00.000Z [out] [new] recent\nunstructured\n",
        )
        .expect("current log");

        let response = super::read_logs(
            temp.path(),
            "demo/production",
            0,
            0,
            Some(1_779_052_400),
            1024,
        )
        .expect("read logs");

        let text = String::from_utf8(response.bytes).expect("utf8");
        assert_eq!(
            text,
            "2026-05-18T09:00:00.000Z [out] [new] recent\nunstructured\n"
        );
        assert_eq!(response.previous_offset, response.previous_len);
        assert_eq!(response.current_offset, response.current_len);
        assert!(!response.truncated);
    }

    #[test]
    fn read_logs_since_honors_max_bytes() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let log_dir = temp
            .path()
            .join("apps")
            .join("demo/production")
            .join("logs");
        fs::create_dir_all(&log_dir).expect("log dir");
        fs::write(
            log_dir.join("current.log"),
            "2026-05-18T09:00:00.000Z [out] [a] first\n2026-05-18T09:00:01.000Z [out] [b] second\n",
        )
        .expect("current log");

        let response = super::read_logs(
            temp.path(),
            "demo/production",
            0,
            0,
            Some(1_779_052_400),
            "2026-05-18T09:00:00.000Z [out] [a] first\n".len(),
        )
        .expect("read logs");

        assert_eq!(
            String::from_utf8(response.bytes).expect("utf8"),
            "2026-05-18T09:00:00.000Z [out] [a] first\n"
        );
        assert!(response.truncated);
    }

    #[test]
    fn read_logs_resumes_current_file_from_offset() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let log_dir = temp
            .path()
            .join("apps")
            .join("demo/production")
            .join("logs");
        fs::create_dir_all(&log_dir).expect("log dir");
        let first = "2026-05-18T09:00:00.000Z [out] [a] first\n";
        fs::write(
            log_dir.join("current.log"),
            format!("{first}2026-05-18T09:00:01.000Z [out] [b] second\n"),
        )
        .expect("current log");

        let response = super::read_logs(
            temp.path(),
            "demo/production",
            0,
            first.len() as u64,
            None,
            1024,
        )
        .expect("read logs");

        assert_eq!(
            String::from_utf8(response.bytes).expect("utf8"),
            "2026-05-18T09:00:01.000Z [out] [b] second\n"
        );
        assert_eq!(response.current_offset, response.current_len);
    }
}
