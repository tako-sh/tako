use std::sync::OnceLock;

use tako_core::{BackupInfo, BackupKind, BackupStatusResponse, RestoredDatabases};
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

const WORKFLOWS_DATABASE: &str = "tako/workflows.sqlite";

/// What restoring `backup` replaces. `None` when the kind is unknown.
pub(super) fn restore_confirm_description(backup: Option<&BackupInfo>) -> String {
    let Some(backup) = backup else {
        return "This stops the app, then replaces its data directory from a full backup, or only the SQLite databases in a database snapshot.".to_string();
    };
    match backup.kind {
        BackupKind::Full => "This stops the app and replaces its data directory.".to_string(),
        BackupKind::Databases if backup.databases.is_empty() => "This stops the app and replaces the SQLite databases in this snapshot. Other files and databases stay untouched. If the snapshot includes workflow state, workflow runs rewind to the snapshot time.".to_string(),
        BackupKind::Databases => {
            let mut description = format!(
                "This stops the app and replaces the {} in this snapshot. Other files and databases stay untouched.",
                count_databases(backup.databases.len())
            );
            if backup.databases.iter().any(|path| path == WORKFLOWS_DATABASE) {
                description.push_str(" Workflow runs rewind to the snapshot time.");
            }
            description
        }
    }
}

pub(super) fn output_restored_databases(
    app_name: &str,
    backup: &BackupInfo,
    databases: &RestoredDatabases,
) {
    for line in format_restored_databases(app_name, backup, databases) {
        match line {
            RestoreLine::Summary(text) => output::info(&text),
            RestoreLine::Database(text) => output::bullet(&text),
            RestoreLine::Note(text) => output::muted(&text),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RestoreLine {
    Summary(String),
    Database(String),
    Note(String),
}

fn format_restored_databases(
    app_name: &str,
    backup: &BackupInfo,
    databases: &RestoredDatabases,
) -> Vec<RestoreLine> {
    let mut lines = vec![RestoreLine::Summary(format!(
        "Restored {} of {} from {} ({})",
        count_databases(databases.replaced.len() + databases.created.len()),
        output::strong(app_name),
        output::strong(&backup.id),
        output::format_size(backup.size_bytes)
    ))];
    for path in &databases.replaced {
        lines.push(RestoreLine::Database(if path == WORKFLOWS_DATABASE {
            format!("{path} (workflow runs rewound)")
        } else {
            path.clone()
        }));
    }
    for path in &databases.created {
        lines.push(RestoreLine::Database(format!("{path} (created)")));
    }
    if !databases.untouched.is_empty() {
        lines.push(RestoreLine::Note(format!(
            "Untouched, not in the snapshot: {}",
            databases.untouched.join(", ")
        )));
    }
    lines
}

fn count_databases(count: usize) -> String {
    match count {
        1 => "1 SQLite database".to_string(),
        count => format!("{count} SQLite databases"),
    }
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

/// Why scheduled backups on this server are failing, if they are.
pub(super) fn format_status_failure(
    server_name: &str,
    status: &BackupStatusResponse,
) -> Option<String> {
    let failure = status.last_backup_failure.as_ref()?;
    let retry = format_backup_time(failure.retry_at_unix_secs);
    Some(match failure.reason {
        tako_core::BackupFailureReason::IndexUnreadable => {
            format!(
                "{server_name}: {}; next attempt at {retry}",
                failure.message
            )
        }
        tako_core::BackupFailureReason::Failed
        | tako_core::BackupFailureReason::InsufficientDisk => format!(
            "{server_name}: full backup failed at {}: {}; next attempt at {retry}",
            format_backup_time(failure.at_unix_secs),
            failure.message
        ),
    })
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
            last_backup_failure: None,
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
            last_backup_failure: None,
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
        assert_eq!(format_status_failure("prod", &status), None);

        status.last_backup_failure = Some(tako_core::BackupFailure {
            at_unix_secs: 0,
            reason: tako_core::BackupFailureReason::InsufficientDisk,
            message: "Not enough free disk space for a full backup.".to_string(),
            retry_at_unix_secs: 0,
        });
        assert_eq!(
            format_status_failure("prod", &status).unwrap(),
            format!(
                "prod: full backup failed at {time}: Not enough free disk space for a full backup.; next attempt at {time}"
            )
        );
        status.last_backup_failure = Some(tako_core::BackupFailure {
            at_unix_secs: 0,
            reason: tako_core::BackupFailureReason::IndexUnreadable,
            message: "Failed to read backup index: invalid JSON".to_string(),
            retry_at_unix_secs: 0,
        });
        assert_eq!(
            format_status_failure("prod", &status).unwrap(),
            format!("prod: Failed to read backup index: invalid JSON; next attempt at {time}")
        );
    }

    fn snapshot(databases: &[&str]) -> BackupInfo {
        serde_json::from_value(serde_json::json!({
            "id": "d1", "app": "demo", "environment": "production", "server": "la",
            "created_at_unix_secs": 0, "size_bytes": 2048, "sha256_hex": "",
            "archive_key": "", "manifest_key": "", "kind": "databases", "databases": databases,
            "encryption": {"algorithm": "", "key_id": "", "nonce_base64": "", "tag_base64": ""}
        }))
        .unwrap()
    }

    #[test]
    fn restore_confirmation_describes_what_each_kind_replaces() {
        let mut full = snapshot(&[]);
        full.kind = BackupKind::Full;
        assert_eq!(
            restore_confirm_description(Some(&full)),
            "This stops the app and replaces its data directory."
        );
        assert_eq!(
            restore_confirm_description(Some(&snapshot(&["app/data.sqlite"]))),
            "This stops the app and replaces the 1 SQLite database in this snapshot. Other files and databases stay untouched."
        );
        assert_eq!(
            restore_confirm_description(Some(&snapshot(&[
                "app/data.sqlite",
                "tako/workflows.sqlite"
            ]))),
            "This stops the app and replaces the 2 SQLite databases in this snapshot. Other files and databases stay untouched. Workflow runs rewind to the snapshot time."
        );
        assert!(restore_confirm_description(Some(&snapshot(&[]))).ends_with(
            "If the snapshot includes workflow state, workflow runs rewind to the snapshot time."
        ));
        assert!(restore_confirm_description(None).contains("only the SQLite databases"));
    }

    #[test]
    fn restored_databases_list_changes_and_untouched_databases() {
        let lines = format_restored_databases(
            "demo",
            &snapshot(&[]),
            &RestoredDatabases {
                replaced: vec!["app/data.sqlite".into(), "tako/workflows.sqlite".into()],
                created: vec!["app/new.sqlite".into()],
                untouched: vec!["app/a.sqlite".into(), "app/b.sqlite".into()],
            },
        );
        let RestoreLine::Summary(summary) = &lines[0] else {
            panic!("{lines:?}");
        };
        assert!(
            summary.starts_with("Restored 3 SQLite databases of "),
            "{summary}"
        );
        assert_eq!(
            &lines[1..],
            [
                RestoreLine::Database("app/data.sqlite".into()),
                RestoreLine::Database("tako/workflows.sqlite (workflow runs rewound)".into()),
                RestoreLine::Database("app/new.sqlite (created)".into()),
                RestoreLine::Note(
                    "Untouched, not in the snapshot: app/a.sqlite, app/b.sqlite".into()
                ),
            ]
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
