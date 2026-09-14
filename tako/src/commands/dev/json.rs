use serde_json::{Map, Value, json};

use crate::dev_server_client::RegisteredAppInfo;

use super::{DevEvent, LogLevel, ScopedLog, TunnelCloseReason};

pub(super) fn ready_record(app: &str, url: &str, hosts: &[String]) -> Value {
    json!({
        "type": "ready",
        "ok": true,
        "command": "dev",
        "app": app,
        "url": url,
        "hosts": hosts,
    })
}

pub(super) fn log_record(log: &ScopedLog) -> Value {
    let mut out = Map::new();
    out.insert("type".to_string(), json!("log"));
    out.insert("ts".to_string(), json!(log.timestamp));
    out.insert("level".to_string(), json!(json_level(&log.level)));
    out.insert("scope".to_string(), json!(log.scope));
    out.insert("msg".to_string(), json!(log.message));
    if let Some(fields) = &log.fields {
        out.insert("fields".to_string(), json!(fields));
    }
    if let Some(kind) = &log.kind {
        out.insert("kind".to_string(), json!(kind));
    }
    Value::Object(out)
}

pub(super) fn event_record(event: &DevEvent) -> Option<Value> {
    match event {
        DevEvent::AppReady => Some(json!({
            "type": "event",
            "event": "app_ready",
        })),
        DevEvent::AppLaunching => Some(json!({
            "type": "event",
            "event": "app_launching",
        })),
        DevEvent::AppStopped => Some(json!({
            "type": "event",
            "event": "app_stopped",
        })),
        DevEvent::AppPid(pid) => Some(json!({
            "type": "event",
            "event": "app_pid",
            "pid": pid,
        })),
        DevEvent::AppError(message) => Some(json!({
            "type": "event",
            "event": "app_error",
            "message": message,
        })),
        DevEvent::ClientConnected {
            is_self: false,
            client_id,
        } => Some(json!({
            "type": "event",
            "event": "client_connected",
            "client_id": client_id,
        })),
        DevEvent::ClientDisconnected { client_id } => Some(json!({
            "type": "event",
            "event": "client_disconnected",
            "client_id": client_id,
        })),
        DevEvent::LanModeChanged {
            enabled, lan_ip, ..
        } => Some(json!({
            "type": "event",
            "event": "lan",
            "enabled": enabled,
            "ip": lan_ip,
        })),
        DevEvent::TunnelModeChanged {
            enabled,
            url,
            close_reason,
            ..
        } => Some(json!({
            "type": "event",
            "event": "tunnel",
            "enabled": enabled,
            "url": url,
            "reason": close_reason.map(TunnelCloseReason::log_message),
        })),
        DevEvent::TunnelConnectionChanged { connected, url } => Some(json!({
            "type": "event",
            "event": "tunnel_connection",
            "connected": connected,
            "url": url,
        })),
        DevEvent::ExitWithMessage(message) => Some(json!({
            "type": "event",
            "event": "exit",
            "message": message,
        })),
        DevEvent::AppStarted
        | DevEvent::AppProcessExited(_)
        | DevEvent::ClientConnected { is_self: true, .. }
        | DevEvent::LanStarting
        | DevEvent::LanFailed
        | DevEvent::TunnelStarting
        | DevEvent::TunnelFailed => None,
    }
}

pub(super) fn list_record(apps: &[RegisteredAppInfo]) -> Value {
    json!({
        "ok": true,
        "command": "dev",
        "apps": apps.iter().map(listed_app_record).collect::<Vec<_>>(),
    })
}

fn listed_app_record(app: &RegisteredAppInfo) -> Value {
    json!({
        "name": app.app_name,
        "status": app.status,
        "url": listed_app_url(app),
        "tunnel": app.tunnel_url,
        "config": app.config_path,
    })
}

fn listed_app_url(app: &RegisteredAppInfo) -> Option<String> {
    app.hosts.first().map(|host| format!("https://{host}/"))
}

fn json_level(level: &LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
        LogLevel::Fatal => "fatal",
    }
}

pub(super) fn emit(value: Value) {
    let _ = crate::output::json_result(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_record_is_structured_json_without_logo() {
        let value = ready_record("demo", "https://demo.test/", &["demo.test".to_string()]);

        assert_eq!(value["type"], "ready");
        assert_eq!(value["ok"], true);
        assert_eq!(value["command"], "dev");
        assert_eq!(value["app"], "demo");
        assert_eq!(value["url"], "https://demo.test/");
        assert_eq!(value["hosts"][0], "demo.test");
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(!encoded.contains('▀'));
        assert!(!encoded.contains("Tako Dev"));
    }

    #[test]
    fn log_record_uses_lowercase_level_and_msg() {
        let log = ScopedLog::info("vite", "ready in 279 ms");
        let value = log_record(&log);

        assert_eq!(value["type"], "log");
        assert_eq!(value["level"], "info");
        assert_eq!(value["scope"], "vite");
        assert_eq!(value["msg"], "ready in 279 ms");
        assert_eq!(value["ts"].as_str().unwrap().len(), 8);
        assert!(value.get("fields").is_none());
    }

    #[test]
    fn log_record_includes_optional_fields() {
        let mut fields = serde_json::Map::new();
        fields.insert("ms".to_string(), json!(24));
        let log = ScopedLog {
            timestamp: "16:14:02".to_string(),
            level: LogLevel::Warn,
            scope: "tako".to_string(),
            message: "slow".to_string(),
            fields: Some(fields),
            kind: Some("restarted".to_string()),
        };
        let value = log_record(&log);

        assert_eq!(value["fields"]["ms"], 24);
        assert_eq!(value["kind"], "restarted");
        assert_eq!(value["level"], "warn");
    }

    #[test]
    fn event_record_emits_app_ready() {
        let value = event_record(&DevEvent::AppReady).unwrap();
        assert_eq!(value["type"], "event");
        assert_eq!(value["event"], "app_ready");
    }

    #[test]
    fn event_record_includes_tunnel_url() {
        let value = event_record(&DevEvent::TunnelModeChanged {
            enabled: true,
            url: Some("https://demo.tako.website".to_string()),
            expires_at: None,
            close_reason: None,
        })
        .unwrap();

        assert_eq!(value["event"], "tunnel");
        assert_eq!(value["enabled"], true);
        assert_eq!(value["url"], "https://demo.tako.website");
    }

    #[test]
    fn event_record_skips_self_client_and_internal_noise() {
        assert!(event_record(&DevEvent::AppStarted).is_none());
        assert!(
            event_record(&DevEvent::ClientConnected {
                is_self: true,
                client_id: 1,
            })
            .is_none()
        );
    }

    #[test]
    fn list_record_returns_empty_apps() {
        let value = list_record(&[]);
        assert_eq!(value["ok"], true);
        assert_eq!(value["command"], "dev");
        assert_eq!(value["apps"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_record_includes_app_url_and_null_tunnel() {
        let app = RegisteredAppInfo {
            config_path: "/tmp/tako.toml".to_string(),
            project_dir: "/tmp".to_string(),
            app_name: "demo".to_string(),
            variant: None,
            hosts: vec!["demo.test".to_string()],
            upstream_port: 3000,
            status: "running".to_string(),
            pid: Some(12),
            client_pid: None,
            tunnel_url: None,
            tunnel_expires_at: None,
        };
        let value = list_record(&[app]);
        let listed = &value["apps"][0];

        assert_eq!(listed["name"], "demo");
        assert_eq!(listed["status"], "running");
        assert_eq!(listed["url"], "https://demo.test/");
        assert!(listed["tunnel"].is_null());
        assert_eq!(listed["config"], "/tmp/tako.toml");
    }
}
