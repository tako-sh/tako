use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Fatal => "FATAL",
        };
        f.pad(s)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScopedLog {
    pub timestamp: String,
    pub level: LogLevel,
    pub scope: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Wire format: SDK emits `{ts: <unix-millis>, level: <lowercase>, scope, msg, fields?}`.
/// Tako-internal lines may set `kind` (e.g. `"restarted"`, `"lan_mode_enabled"`)
/// to mark a user-triggered action, which the renderer shows as a divider.
/// When `kind` is set, `msg` is optional — the renderer humanizes `kind` for
/// the banner label.
#[derive(Debug, Deserialize)]
struct ScopedLogSerde {
    ts: i64,
    level: LogLevel,
    scope: String,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    fields: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    kind: Option<String>,
}

fn hms_timestamp(h: u8, m: u8, s: u8) -> String {
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn hms_from_unix_millis(ts: i64) -> String {
    let dt = OffsetDateTime::from_unix_timestamp_nanos((ts as i128) * 1_000_000)
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .to_offset(local_offset());
    hms_timestamp(dt.hour(), dt.minute(), dt.second())
}

impl<'de> Deserialize<'de> for ScopedLog {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = ScopedLogSerde::deserialize(deserializer)?;
        Ok(Self {
            timestamp: hms_from_unix_millis(raw.ts),
            level: raw.level,
            scope: raw.scope,
            message: raw.msg,
            fields: raw.fields,
            kind: raw.kind,
        })
    }
}

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

fn local_offset() -> UtcOffset {
    *LOCAL_OFFSET.get_or_init(|| UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC))
}

impl ScopedLog {
    pub fn at(level: LogLevel, scope: impl Into<String>, message: impl Into<String>) -> Self {
        let now = OffsetDateTime::now_utc().to_offset(local_offset());
        Self {
            timestamp: hms_timestamp(now.hour(), now.minute(), now.second()),
            level,
            scope: scope.into(),
            message: message.into(),
            fields: None,
            kind: None,
        }
    }

    pub fn info(scope: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(LogLevel::Info, scope, message)
    }

    pub fn warn(scope: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(LogLevel::Warn, scope, message)
    }

    pub fn error(scope: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(LogLevel::Error, scope, message)
    }
}

#[cfg(test)]
const APP_SCOPE: &str = "app";

#[cfg(test)]
pub(super) fn app_log_scope() -> String {
    APP_SCOPE.to_string()
}

/// Events from the dev server
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DevEvent {
    AppLaunching,
    AppStarted,
    AppReady,
    AppStopped,
    AppProcessExited(String),
    AppPid(u32),
    AppError(String),
    ClientConnected {
        is_self: bool,
        client_id: u32,
    },
    ClientDisconnected {
        client_id: u32,
    },
    LanModeChanged {
        enabled: bool,
        lan_ip: Option<String>,
        ca_url: Option<String>,
    },
    LanStarting,
    LanFailed,
    TunnelModeChanged {
        enabled: bool,
        url: Option<String>,
        expires_at: Option<u64>,
        close_reason: Option<TunnelCloseReason>,
    },
    TunnelConnectionChanged {
        connected: bool,
        url: String,
    },
    TunnelStarting,
    TunnelFailed,
    ExitWithMessage(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelCloseReason {
    User,
    Timeout,
    Shutdown,
    ConnectionClosed,
    ConnectionError,
}

impl TunnelCloseReason {
    pub(super) fn log_message(self) -> &'static str {
        match self {
            TunnelCloseReason::User => "Tunnel off: turned off by user",
            TunnelCloseReason::Timeout => "Tunnel off: session expired",
            TunnelCloseReason::Shutdown => "Tunnel off: app stopped",
            TunnelCloseReason::ConnectionClosed => "Tunnel off: connection closed",
            TunnelCloseReason::ConnectionError => "Tunnel off: connection lost",
        }
    }

    pub(super) fn log_level(self) -> LogLevel {
        match self {
            TunnelCloseReason::ConnectionError => LogLevel::Warn,
            TunnelCloseReason::User
            | TunnelCloseReason::Timeout
            | TunnelCloseReason::Shutdown
            | TunnelCloseReason::ConnectionClosed => LogLevel::Info,
        }
    }
}

impl From<crate::dev_server_client::TunnelCloseReason> for TunnelCloseReason {
    fn from(reason: crate::dev_server_client::TunnelCloseReason) -> Self {
        match reason {
            crate::dev_server_client::TunnelCloseReason::User => Self::User,
            crate::dev_server_client::TunnelCloseReason::Timeout => Self::Timeout,
            crate::dev_server_client::TunnelCloseReason::Shutdown => Self::Shutdown,
            crate::dev_server_client::TunnelCloseReason::ConnectionClosed => Self::ConnectionClosed,
            crate::dev_server_client::TunnelCloseReason::ConnectionError => Self::ConnectionError,
        }
    }
}

impl DevEvent {
    pub(super) fn is_state_only(&self) -> bool {
        matches!(
            self,
            DevEvent::LanStarting
                | DevEvent::LanFailed
                | DevEvent::TunnelModeChanged { .. }
                | DevEvent::TunnelConnectionChanged { .. }
                | DevEvent::TunnelStarting
                | DevEvent::TunnelFailed
        )
    }
}

#[cfg(test)]
fn strip_ascii_case_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() < prefix.len() {
        return None;
    }
    let (head, tail) = s.split_at(prefix.len());
    head.eq_ignore_ascii_case(prefix).then_some(tail)
}

#[cfg(test)]
fn prefixed_child_log_level_and_message(line: &str) -> Option<(LogLevel, String)> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    let candidates = [
        ("[TRACE]", LogLevel::Debug),
        ("[DEBUG]", LogLevel::Debug),
        ("[INFO]", LogLevel::Info),
        ("[WARN]", LogLevel::Warn),
        ("[WARNING]", LogLevel::Warn),
        ("[ERROR]", LogLevel::Error),
        ("[FATAL]", LogLevel::Fatal),
        ("TRACE", LogLevel::Debug),
        ("DEBUG", LogLevel::Debug),
        ("INFO", LogLevel::Info),
        ("WARN", LogLevel::Warn),
        ("WARNING", LogLevel::Warn),
        ("ERROR", LogLevel::Error),
        ("FATAL", LogLevel::Fatal),
    ];

    for (prefix, level) in candidates {
        let Some(rest) = strip_ascii_case_prefix(trimmed, prefix) else {
            continue;
        };

        if !prefix.starts_with('[')
            && rest
                .chars()
                .next()
                .is_some_and(|ch| !ch.is_whitespace() && ch != ':' && ch != '-' && ch != '|')
        {
            continue;
        }

        let message = rest.trim_start_matches(|ch: char| {
            ch.is_whitespace() || ch == ':' || ch == '-' || ch == '|'
        });
        let message = if message.is_empty() { trimmed } else { message };
        return Some((level.clone(), message.to_string()));
    }

    None
}

#[cfg(test)]
pub(super) fn child_log_level_and_message(
    default_level: LogLevel,
    line: &str,
) -> (LogLevel, String) {
    prefixed_child_log_level_and_message(line).unwrap_or((default_level, line.to_string()))
}

#[cfg(test)]
pub(super) fn should_drop_child_log_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    let Some(rest) = trimmed.strip_prefix("$ ") else {
        return false;
    };
    rest.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '/' || ch == '@')
}

#[cfg(test)]
pub(super) fn trim_child_log_message(message: &str) -> Option<String> {
    let trimmed_end = message.trim_end();
    if trimmed_end.trim().is_empty() {
        None
    } else {
        Some(trimmed_end.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{DevEvent, LogLevel, TunnelCloseReason};

    #[test]
    fn tunnel_mode_changes_are_state_only() {
        assert!(
            DevEvent::TunnelModeChanged {
                enabled: true,
                url: Some("https://yh5spxz5.tako.website".to_string()),
                expires_at: Some(1_797_132_000),
                close_reason: None,
            }
            .is_state_only()
        );
        assert!(
            DevEvent::TunnelModeChanged {
                enabled: false,
                url: None,
                expires_at: None,
                close_reason: Some(TunnelCloseReason::User),
            }
            .is_state_only()
        );
    }

    #[test]
    fn tunnel_close_reason_has_user_facing_log_copy() {
        assert_eq!(
            TunnelCloseReason::Timeout.log_message(),
            "Tunnel off: session expired"
        );
        assert!(matches!(
            TunnelCloseReason::Timeout.log_level(),
            LogLevel::Info
        ));
    }

    #[test]
    fn lan_mode_changes_still_render_user_output() {
        assert!(
            !DevEvent::LanModeChanged {
                enabled: true,
                lan_ip: Some("192.168.1.42".to_string()),
                ca_url: Some("http://192.168.1.42/ca.pem".to_string()),
            }
            .is_state_only()
        );
    }
}
