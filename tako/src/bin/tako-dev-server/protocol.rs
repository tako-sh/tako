use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum Request {
    Ping,
    Info,
    /// Register a persistent app by config path.
    RegisterApp {
        config_path: String,
        project_dir: String,
        app_name: String,
        #[serde(default)]
        variant: Option<String>,
        #[serde(default)]
        hosts: Vec<String>,
        command: Vec<String>,
        env: std::collections::HashMap<String, String>,
        #[serde(default)]
        images: Box<tako_images::ImagesConfig>,
        #[serde(default)]
        storages: std::collections::HashMap<String, tako_core::StorageBinding>,
        #[serde(default)]
        client_pid: Option<u32>,
        #[serde(default)]
        readiness_failure_hint: Option<String>,
        /// Command to spawn the workflow worker subprocess on demand (first
        /// `program`, rest are args). Present iff the project has a
        /// configured workflows directory — the client resolves the runtime-specific
        /// entrypoint (e.g. `bun run .../bun-worker.mjs`) and hands it over
        /// so the daemon doesn't need to re-do runtime detection. Omitted
        /// when there are no workflows to run.
        #[serde(default)]
        worker_command: Option<Vec<String>>,
    },
    /// Unregister (stop) an app by config path.
    UnregisterApp {
        config_path: String,
    },
    /// Update an app's status (running/idle/stopped).
    SetAppStatus {
        config_path: String,
        status: String,
    },
    /// Hand off a running process PID to the daemon.
    HandoffApp {
        config_path: String,
        pid: u32,
    },
    /// Request an app restart (relayed to the owning client via events).
    RestartApp {
        config_path: String,
    },
    /// A client session started for an app.
    ConnectClient {
        config_path: String,
        client_id: u32,
    },
    /// Subscribe to an app's log stream.
    SubscribeLogs {
        config_path: String,
        #[serde(default)]
        after: Option<u64>,
    },
    /// Toggle LAN mode (expose dev server on the local network).
    ToggleLan {
        enabled: bool,
    },
    /// List all registered apps.
    ListRegisteredApps,
    ListApps,
    SubscribeEvents,
    StopServer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum Response {
    Pong,
    Apps {
        apps: Vec<AppInfo>,
    },
    Info {
        info: DevInfo,
    },
    AppRegistered {
        app_name: String,
        config_path: String,
        project_dir: String,
        url: String,
    },
    AppUnregistered {
        config_path: String,
    },
    AppStatusUpdated {
        config_path: String,
        status: String,
    },
    AppRestarting {
        config_path: String,
    },
    AppHandedOff {
        config_path: String,
    },
    RegisteredApps {
        apps: Vec<RegisteredAppInfo>,
    },
    Subscribed,
    Event {
        event: DevEvent,
    },
    LogsSubscribed,
    LogEntry {
        id: u64,
        line: String,
    },
    LogsTruncated,
    LanToggled {
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lan_ip: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca_url: Option<String>,
    },
    Stopping,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum DevEvent {
    RequestStarted {
        host: String,
        path: String,
    },
    RequestFinished {
        host: String,
        path: String,
    },
    AppStatusChanged {
        config_path: String,
        app_name: String,
        status: String,
    },
    RestartRequested {
        config_path: String,
        app_name: String,
    },
    ClientConnected {
        config_path: String,
        app_name: String,
        client_id: u32,
    },
    ClientDisconnected {
        config_path: String,
        app_name: String,
        client_id: u32,
    },
    LanModeChanged {
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lan_ip: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca_url: Option<String>,
    },
    AppLaunching {
        config_path: String,
        app_name: String,
    },
    AppPid {
        config_path: String,
        app_name: String,
        pid: u32,
    },
    AppStarted {
        config_path: String,
        app_name: String,
    },
    AppReady {
        config_path: String,
        app_name: String,
    },
    AppProcessExited {
        config_path: String,
        app_name: String,
        message: String,
    },
    AppError {
        config_path: String,
        app_name: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisteredAppInfo {
    pub config_path: String,
    pub project_dir: String,
    pub app_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub hosts: Vec<String>,
    pub upstream_port: u16,
    pub status: String,
    pub pid: Option<u32>,
    pub client_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppInfo {
    pub app_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    pub upstream_port: u16,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DevInfo {
    /// Where the daemon proxy is currently listening.
    pub listen: String,
    pub port: u16,
    /// IP currently advertised for `.test` (and `.tako.test`) hostnames.
    pub advertised_ip: String,
    #[serde(default)]
    pub local_dns_enabled: bool,
    #[serde(default)]
    pub local_dns_port: u16,
    #[serde(default)]
    pub control_clients: u32,
    #[serde(default)]
    pub lan_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_ip: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_ping_pong() {
        let req = Request::Ping;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"type":"Ping"}"#);
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::Pong;
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"type":"Pong"}"#);
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_stop() {
        let req = Request::StopServer;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::Stopping;
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_events() {
        let req = Request::SubscribeEvents;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::Subscribed;
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);

        let resp = Response::Event {
            event: DevEvent::RequestStarted {
                host: "a.test".to_string(),
                path: "/api".to_string(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);

        let resp = Response::Event {
            event: DevEvent::RequestFinished {
                host: "a.test".to_string(),
                path: "/api".to_string(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_info() {
        let req = Request::Info;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::Info {
            info: DevInfo {
                listen: "127.0.0.1:8443".to_string(),
                port: 8443,
                advertised_ip: "127.0.0.1".to_string(),
                local_dns_enabled: true,
                local_dns_port: 53535,
                control_clients: 1,
                lan_enabled: false,
                lan_ip: None,
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_register_app() {
        let req = Request::RegisterApp {
            config_path: "/home/user/proj/tako.toml".to_string(),
            project_dir: "/home/user/proj".to_string(),
            app_name: "my-app".to_string(),
            variant: None,
            hosts: vec!["my-app.test".to_string(), "my-app.test/api".to_string()],
            command: vec!["bun".to_string(), "run".to_string(), "index.ts".to_string()],
            env: std::collections::HashMap::from([(
                "NODE_ENV".to_string(),
                "development".to_string(),
            )]),
            images: Box::default(),
            storages: std::collections::HashMap::new(),
            client_pid: Some(1234),
            readiness_failure_hint: Some("install the adapter".to_string()),
            worker_command: Some(vec![
                "bun".to_string(),
                "run".to_string(),
                "node_modules/tako.sh/dist/entrypoints/bun-worker.mjs".to_string(),
            ]),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::AppRegistered {
            app_name: "my-app".to_string(),
            config_path: "/home/user/proj/tako.toml".to_string(),
            project_dir: "/home/user/proj".to_string(),
            url: "https://my-app.test/".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_unregister_app() {
        let req = Request::UnregisterApp {
            config_path: "/proj/tako.toml".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::AppUnregistered {
            config_path: "/proj/tako.toml".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_set_app_status() {
        let req = Request::SetAppStatus {
            config_path: "/proj/tako.toml".to_string(),
            status: "idle".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::AppStatusUpdated {
            config_path: "/proj/tako.toml".to_string(),
            status: "idle".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_handoff_app() {
        let req = Request::HandoffApp {
            config_path: "/proj/tako.toml".to_string(),
            pid: 12345,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::AppHandedOff {
            config_path: "/proj/tako.toml".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_list_registered_apps() {
        let req = Request::ListRegisteredApps;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::RegisteredApps {
            apps: vec![RegisteredAppInfo {
                config_path: "/proj/tako.toml".to_string(),
                project_dir: "/proj".to_string(),
                app_name: "app".to_string(),
                variant: None,
                hosts: vec!["app.test".to_string()],
                upstream_port: 3000,
                status: "running".to_string(),
                pid: Some(111),
                client_pid: Some(222),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_app_status_changed_event() {
        let resp = Response::Event {
            event: DevEvent::AppStatusChanged {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "app".to_string(),
                status: "idle".to_string(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_restart_app() {
        let req = Request::RestartApp {
            config_path: "/proj/tako.toml".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::AppRestarting {
            config_path: "/proj/tako.toml".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_restart_requested_event() {
        let resp = Response::Event {
            event: DevEvent::RestartRequested {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "app".to_string(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_toggle_lan() {
        let req = Request::ToggleLan { enabled: true };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);

        let resp = Response::LanToggled {
            enabled: true,
            lan_ip: Some("192.168.1.42".to_string()),
            ca_url: Some("http://192.168.1.42/ca.pem".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_roundtrip_app_lifecycle_events() {
        for event in [
            DevEvent::AppLaunching {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "app".to_string(),
            },
            DevEvent::AppPid {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "app".to_string(),
                pid: 4242,
            },
            DevEvent::AppStarted {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "app".to_string(),
            },
            DevEvent::AppReady {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "app".to_string(),
            },
            DevEvent::AppProcessExited {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "app".to_string(),
                message: "exit code 1".to_string(),
            },
            DevEvent::AppError {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "app".to_string(),
                message: "spawn failed".to_string(),
            },
        ] {
            let resp = Response::Event { event };
            let json = serde_json::to_string(&resp).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
        }
    }

    #[test]
    fn serde_roundtrip_lan_mode_changed_event() {
        let resp = Response::Event {
            event: DevEvent::LanModeChanged {
                enabled: true,
                lan_ip: Some("192.168.1.42".to_string()),
                ca_url: Some("http://192.168.1.42/ca.pem".to_string()),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn serde_request_started_requires_path() {
        let json = r#"{"type":"Event","event":{"type":"RequestStarted","host":"a.test"}}"#;
        assert!(serde_json::from_str::<Response>(json).is_err());
    }
}
