use std::sync::OnceLock;

use tako_core::{BackupInfo, BackupStatusResponse};
use time::{OffsetDateTime, UtcOffset};

use crate::output;

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

pub(super) fn output_backup_line(backup: &BackupInfo) {
    output::bullet(&format!(
        "{} {} {}",
        output::strong(&backup.id),
        format_backup_time(backup.created_at_unix_secs),
        output::format_size(backup.size_bytes)
    ));
}

pub(super) fn format_status_line(server_name: &str, status: &BackupStatusResponse) -> String {
    if !status.enabled {
        return format!("{server_name}: disabled");
    }
    let last = status
        .last_backup
        .as_ref()
        .map(|backup| {
            format!(
                "last {} at {}",
                backup.id,
                format_backup_time(backup.created_at_unix_secs)
            )
        })
        .unwrap_or_else(|| "no backups yet".to_string());
    let next = status
        .next_backup_at_unix_secs
        .map(format_backup_time)
        .unwrap_or_else(|| "-".to_string());
    let retention = status
        .retention_days
        .map(|days| format!("{days}d retention"))
        .unwrap_or_else(|| "retention unknown".to_string());
    format!("{server_name}: enabled, {last}, next {next}, {retention}")
}

fn local_offset() -> UtcOffset {
    *LOCAL_OFFSET.get_or_init(|| UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC))
}

fn format_backup_time(unix_secs: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix_secs)
        .map(|dt| {
            let dt = dt.to_offset(local_offset());
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                dt.year(),
                dt.month() as u8,
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second()
            )
        })
        .unwrap_or_else(|_| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_marks_disabled_backup() {
        let status = BackupStatusResponse {
            app: "demo/production".to_string(),
            enabled: false,
            retention_days: None,
            last_backup: None,
            next_backup_at_unix_secs: None,
        };
        assert_eq!(format_status_line("prod", &status), "prod: disabled");
    }
}
