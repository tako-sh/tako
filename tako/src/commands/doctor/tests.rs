use super::common::*;
use super::macos::*;
use serde_json::json;

#[test]
fn format_static_doctor_rows_include_hints() {
    let mut buf = Vec::new();
    let dev_info = Ok(json!({
        "info": {
            "listen": "127.0.0.1:47831",
            "port": 47831,
            "local_dns_enabled": true,
            "local_dns_port": 53535
        }
    }));
    let dns_info = Ok(json!({
        "info": {
            "listen": "127.0.0.1:47831",
            "port": 47831,
            "local_dns_enabled": true,
            "local_dns_port": 53535
        }
    }));
    let macos = MacosData {
        dev_proxy: super::super::dev::DevProxyStatus {
            installed: true,
            bootstrap_loaded: true,
            alias_ready: true,
            launchd_loaded: true,
            https_ready: true,
            http_ready: true,
        },
        https_tcp_ok: true,
        http_tcp_ok: true,
        advertised_ip: "127.77.0.1".to_string(),
        local_dns_port: 53535,
        resolver_values: Some(("127.0.0.1".to_string(), 53535)),
        host_dns_results: vec![(
            "bun-example.tako.test".to_string(),
            Some("127.77.0.1".to_string()),
        )],
    };

    format_paths(&mut buf, "/tmp/tako-config", "/tmp/tako-data");
    format_certificate(&mut buf, &CaStatus::Trusted);
    format_dev_server(&mut buf, &dev_info);
    format_local_dns(&mut buf, &dns_info, &[], &macos);

    assert!(
        buf.iter()
            .any(|line| line.contains("Directory where Tako stores local configuration files"))
    );
    assert!(buf.iter().any(|line| {
        line.contains("Directory where Tako stores runtime state and cached assets")
    }));
    assert!(buf.iter().any(|line| {
        line.contains("Trust state of the Tako local certificate authority for https://*.tako.test")
    }));
    assert!(buf.iter().any(|line| {
        line.contains("Address where the Tako development server listens for local proxy traffic")
    }));
    assert!(buf.iter().any(|line| {
        line.contains("Whether the Tako development server has its local DNS responder enabled")
    }));
    assert!(
        buf.iter()
            .any(|line| line.contains("UDP port used by the local Tako DNS responder"))
    );
    assert!(buf.iter().any(|line| {
        line.contains(
            "Resolver file that should direct *.tako.test lookups to the local DNS server",
        )
    }));
}

#[test]
fn format_dev_server_uses_single_status_hint_for_unavailable_state() {
    let mut buf = Vec::new();
    let dev_info: Result<serde_json::Value, Box<dyn std::error::Error>> =
        Err(
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused").into(),
        );

    format_dev_server(&mut buf, &dev_info);

    assert!(buf.iter().any(|line| line.contains("Status")));
    assert!(buf.iter().any(|line| line.contains("not running")));
    assert_eq!(
        buf.iter()
            .filter(
                |line| line.contains("Current health of the local Tako development server process")
            )
            .count(),
        1
    );
}

#[test]
fn format_macos_sections_capitalizes_dev_proxy_labels() {
    let mut buf = Vec::new();
    let macos = MacosData {
        dev_proxy: super::super::dev::DevProxyStatus {
            installed: true,
            bootstrap_loaded: true,
            alias_ready: true,
            launchd_loaded: true,
            https_ready: true,
            http_ready: true,
        },
        https_tcp_ok: true,
        http_tcp_ok: true,
        advertised_ip: "127.77.0.1".to_string(),
        local_dns_port: 53535,
        resolver_values: Some(("127.0.0.1".to_string(), 53535)),
        host_dns_results: Vec::new(),
    };

    let dev_info = Err(std::io::Error::other("offline").into());
    format_macos_sections(&mut buf, &dev_info, &[], &macos);

    assert!(buf.iter().any(|line| line.contains("Installed")));
    assert!(buf.iter().any(|line| line.contains("Boot Helper")));
    assert!(buf.iter().any(|line| line.contains("Alias")));
    assert!(buf.iter().any(|line| line.contains("Launchd")));
    assert!(buf.iter().any(|line| line.contains("TCP 127.77.0.1:443")));
    assert!(buf.iter().any(|line| line.contains("TCP 127.77.0.1:80")));
    assert!(
        buf.iter()
            .any(|line| line.contains("Binary and support files are present on disk"))
    );
    assert!(buf.iter().any(|line| {
        line.contains("Boot-time helper is loaded so Tako can restore dev proxy setup")
    }));
    assert!(
        buf.iter()
            .any(|line| line.contains("127.77.0.1 is assigned on the lo0 loopback interface"))
    );
    assert!(
        buf.iter()
            .any(|line| line.contains("macOS launchd has loaded the proxy service definition"))
    );
    assert!(buf.iter().any(|line| {
        line.contains("HTTPS proxy is listening on the loopback address and accepts connections")
    }));
    assert!(buf.iter().any(|line| {
        line.contains("HTTP proxy is listening on the loopback address and accepts connections")
    }));
}

#[test]
fn format_local_dns_expects_macos_loopback_ip_for_app_hosts() {
    let mut buf = Vec::new();
    let macos = MacosData {
        dev_proxy: super::super::dev::DevProxyStatus {
            installed: true,
            bootstrap_loaded: true,
            alias_ready: true,
            launchd_loaded: true,
            https_ready: true,
            http_ready: true,
        },
        https_tcp_ok: true,
        http_tcp_ok: true,
        advertised_ip: "127.77.0.1".to_string(),
        local_dns_port: 53535,
        resolver_values: Some(("127.0.0.1".to_string(), 53535)),
        host_dns_results: vec![(
            "bun-example.tako.test".to_string(),
            Some("127.0.0.1".to_string()),
        )],
    };

    let dev_info = Err(std::io::Error::other("offline").into());
    format_local_dns(&mut buf, &dev_info, &[], &macos);

    assert!(
        buf.iter()
            .any(|line| line.contains("(expected 127.77.0.1)")),
        "expected loopback mismatch warning in output: {buf:?}"
    );
}

#[test]
fn format_local_dns_accepts_advertised_loopback_ip_for_app_hosts() {
    let mut buf = Vec::new();
    let macos = MacosData {
        dev_proxy: super::super::dev::DevProxyStatus {
            installed: true,
            bootstrap_loaded: true,
            alias_ready: true,
            launchd_loaded: true,
            https_ready: true,
            http_ready: true,
        },
        https_tcp_ok: true,
        http_tcp_ok: true,
        advertised_ip: "127.77.0.1".to_string(),
        local_dns_port: 53535,
        resolver_values: Some(("127.0.0.1".to_string(), 53535)),
        host_dns_results: vec![(
            "bun-example.tako.test".to_string(),
            Some("127.77.0.1".to_string()),
        )],
    };

    let dev_info = Err(std::io::Error::other("offline").into());
    format_local_dns(&mut buf, &dev_info, &[], &macos);

    assert!(
        buf.iter()
            .any(|line| line.contains("bun-example.tako.test") && line.contains("127.77.0.1")),
        "expected successful loopback resolution in output: {buf:?}"
    );
    assert!(
        !buf.iter()
            .any(|line| line.contains("(expected 127.77.0.1)")),
        "did not expect mismatch warning in output: {buf:?}"
    );
}
