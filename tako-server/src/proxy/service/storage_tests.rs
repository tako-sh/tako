use super::*;
use crate::instances::{AppConfig, AppManager};
use crate::lb::LoadBalancer;
use crate::proxy::{CloudflareIpRanges, ProxyConfig};
use crate::routing::RouteTable;
use crate::scaling::{ColdStartConfig, ColdStartManager};
use std::collections::HashMap;
use tako_core::{StorageBinding, StorageProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn storage_uses_the_routed_app_binding_and_waits_for_restore_without_starting_a_worker() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(AppManager::new(dir.path().into()));
    let binding = StorageBinding {
        provider: StorageProvider::Local,
        bucket: None,
        endpoint: None,
        region: None,
        access_key_id: None,
        secret_access_key: None,
        force_path_style: false,
        public_base_url: None,
        path: Some("storage/uploads".into()),
        signing_key: Some("test-signing-key".into()),
    };
    for name in ["app", "other"] {
        let mut binding = binding.clone();
        if name == "other" {
            binding.signing_key = Some("other-secret".into());
        }
        manager.register_app(AppConfig {
            name: name.into(),
            min_instances: 0,
            storages: HashMap::from([("uploads".into(), binding)]),
            ..Default::default()
        });
    }
    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    routes
        .write()
        .set_app_routes("app".into(), vec!["app.test".into()]);
    routes
        .write()
        .set_app_routes("other".into(), vec!["other.test".into()]);
    let proxy = TakoProxy::new(
        Arc::new(LoadBalancer::new(manager.clone())),
        routes,
        ProxyConfig {
            redirect_http_to_https: false,
            ..Default::default()
        },
        Arc::new(ColdStartManager::new(ColdStartConfig::default())),
        CloudflareIpRanges::default(),
    );
    let paths = crate::release::app_runtime_data_paths(dir.path(), "app");
    std::fs::create_dir_all(paths.app.join("storage/uploads/avatars")).unwrap();
    std::fs::write(paths.app.join("storage/uploads/avatars/a b.txt"), "hello").unwrap();
    for (host, status) in [("app.test", "200"), ("other.test", "403")] {
        let (mut client, stream) = tokio::io::duplex(8192);
        client.write_all(format!("GET /_tako/storages/uploads/avatars/a%20b.txt?expires=4102444800&token=970ee037d59e09374aca4ce2cea30fe57b397bed8e40ecd921dc49496cf05eb9 HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut session = Session::new_h1(Box::new(stream));
        session.read_request().await.unwrap();
        let mut ctx = proxy.new_ctx();
        let app = manager.get_app(host.split('.').next().unwrap()).unwrap();
        let restoring = app.lifecycle_gate.write().await;
        let mut filtered = Box::pin(proxy.request_filter(&mut session, &mut ctx));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut filtered)
                .await
                .is_err()
        );
        drop(restoring);
        assert!(
            tokio::time::timeout(Duration::from_secs(3), filtered)
                .await
                .unwrap()
                .unwrap()
        );
        session.downstream_session.finish().await.unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{response}"
        );
        if status == "200" {
            assert!(response.ends_with("hello"));
        }
    }
    assert!(manager.get_app("app").unwrap().get_instances().is_empty());
}
