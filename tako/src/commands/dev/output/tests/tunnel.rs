use super::*;
use crate::commands::dev::TunnelCloseReason;

#[test]
fn tunnel_close_log_reports_timeout_reason() {
    let log = tunnel_close_log(Some(TunnelCloseReason::Timeout));

    assert!(matches!(log.level, LogLevel::Info));
    assert_eq!(log.scope, "tako");
    assert_eq!(log.message, "Tunnel off: session expired");
}

#[test]
fn tunnel_close_log_warns_for_connection_error() {
    let log = tunnel_close_log(Some(TunnelCloseReason::ConnectionError));

    assert!(matches!(log.level, LogLevel::Warn));
    assert_eq!(log.message, "Tunnel off: connection lost");
}
