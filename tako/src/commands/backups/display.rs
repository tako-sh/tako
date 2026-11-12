use std::sync::OnceLock;

use tako_core::{BackupInfo, BackupStatusResponse};
use time::{OffsetDateTime, UtcOffset};

use crate::output;

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

pub(super) fn output_backup_line(backup: &BackupInfo) {
    output::bullet(&format_backup_line(backup));
}

fn format_backup_line(backup: &BackupInfo) -> String {
    format!(
        "{} {} {} {}",
        output::strong(&backup.id),
        format_backup_time(backup.created_at_unix_secs),
        backup.kind.as_str(),
        output::format_size(backup.size_bytes)
    )
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
    let databases = status
        .last_database_snapshot_at_unix_secs
        .map(|time| format!("last database snapshot at {}", format_backup_time(time)))
        .unwrap_or_else(|| "no database snapshots yet".to_string());
    let skipped = status
        .last_database_snapshot_skip
        .as_ref()
        .map(|skip| {
            format!(
                ", database snapshot skipped at {}: {}",
                format_backup_time(skip.at_unix_secs),
                skip.message
            )
        })
        .unwrap_or_default();
    format!("{server_name}: enabled, {last}, next {next}, {retention}, {databases}{skipped}")
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
            last_database_snapshot_at_unix_secs: None,
            last_database_snapshot_skip: None,
        };
        assert_eq!(format_status_line("prod", &status), "prod: disabled");
    }

    #[test]
    fn status_line_reports_database_snapshots_and_skips() {
        let mut status = BackupStatusResponse {
            app: "demo/production".to_string(),
            enabled: true,
            retention_days: Some(30),
            last_backup: None,
            next_backup_at_unix_secs: None,
            last_database_snapshot_at_unix_secs: None,
            last_database_snapshot_skip: None,
        };
        let line = format_status_line("prod", &status);
        assert!(
            line.ends_with("30d retention, no database snapshots yet"),
            "{line}"
        );

        status.last_database_snapshot_at_unix_secs = Some(0);
        status.last_database_snapshot_skip = Some(tako_core::BackupSkip {
            at_unix_secs: 0,
            reason: tako_core::BackupSkipReason::Unchanged,
            message: "No database changed since the latest backup.".to_string(),
        });
        let line = format_status_line("prod", &status);
        let time = format_backup_time(0);
        assert!(
            line.ends_with(&format!(
                "last database snapshot at {time}, database snapshot skipped at {time}: No database changed since the latest backup."
            )),
            "{line}"
        );
    }

    #[test]
    fn backup_line_shows_kind() {
        let mut backup: BackupInfo = serde_json::from_value(serde_json::json!({
            "id": "b1", "app": "demo", "environment": "production", "server": "la",
            "created_at_unix_secs": 0, "size_bytes": 1, "sha256_hex": "",
            "archive_key": "", "manifest_key": "",
            "encryption": {"algorithm": "", "key_id": "", "nonce_base64": "", "tag_base64": ""}
        }))
        .unwrap();
        assert!(format_backup_line(&backup).contains(" full "));
        backup.kind = tako_core::BackupKind::Databases;
        assert!(format_backup_line(&backup).contains(" databases "));
    }
}
