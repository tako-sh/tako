use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn retired_session_cannot_clear_or_report_for_new_session_at_same_url() {
    let config = "/tmp/generation-test.toml";
    let state = super::tests::tunnel_state_with_app(config, 0, false);
    let task = tokio::spawn(std::future::pending::<()>());
    let mut events = {
        let mut s = state.lock().unwrap();
        s.tunnel_lifecycles
            .entry(config.into())
            .or_default()
            .generation = 2;
        s.apps.get_mut(config).unwrap().tunnel = Some(TunnelRegistration {
            generation: 2,
            url: "https://stable.example.com".into(),
            expires_at: 0,
            abort_handle: task.abort_handle(),
        });
        s.events.subscribe()
    };
    disable_tunnel_generation(&state, config, TunnelCloseReason::Replaced, Some(1));
    broadcast_tunnel_connection(
        &state,
        config,
        "app",
        "https://stable.example.com",
        false,
        1,
    );
    assert!(tunnel_is_current(&state, config, 2));
    assert!(
        events.try_recv().is_err(),
        "old sessions must not emit status for a new session"
    );
    disable_tunnel_generation(&state, config, TunnelCloseReason::User, Some(2));
    assert!(!tunnel_is_current(&state, config, 2));
    assert!(task.await.unwrap_err().is_cancelled());
}

async fn request(stream: &mut tokio::net::TcpStream) {
    let mut data = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let size = stream.read(&mut chunk).await.unwrap();
        assert_ne!(size, 0);
        data.extend_from_slice(&chunk[..size]);
        if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if data.len() >= end + 4 + length {
                return;
            }
        }
    }
}

#[tokio::test]
async fn disable_during_registration_cancels_enable() {
    const CASE: &str = "TAKO_TUNNEL_LIFECYCLE_FIXTURE";
    if std::env::var_os(CASE).is_none() {
        for mode in ["disable", "replace", "disconnect", "reregister"] {
            let temp = tempfile::tempdir().unwrap();
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tunnel::lifecycle_tests::disable_during_registration_cancels_enable",
                    "--nocapture",
                ])
                .env(CASE, mode)
                .env("TAKO_HOME", temp.path())
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let mode = std::env::var(CASE).unwrap();
    let fixture_mode = mode.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // This test runs alone in its process; the override only reaches the local fixture.
    unsafe {
        std::env::set_var("TAKO_TUNNEL_URL", format!("http://{address}"));
    }
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        request(&mut stream).await;
        let challenge = r#"{"host":"app-review.example.com","nonce":"review-nonce"}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{challenge}", challenge.len()).as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        request(&mut stream).await;
        reached_tx.send(()).unwrap();
        release_rx.await.unwrap();
        if matches!(fixture_mode.as_str(), "replace" | "disconnect") {
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err(),
                "a newer enable sent a remote registration before the older one finished"
            );
        }
        let created = r#"{"host":"app-review.example.com","url":"https://app-review.example.com","session":"review-session","expires_at":0}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{created}", created.len()).as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        if matches!(fixture_mode.as_str(), "replace" | "disconnect") {
            for body in [challenge, created] {
                let (mut stream, _) = listener.accept().await.unwrap();
                request(&mut stream).await;
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        }
    });
    let config = "/tmp/review-tako.toml";
    let state = super::tests::tunnel_state_with_app(config, 0, false);
    let enabling_state = state.clone();
    let enabling = tokio::spawn(async move { enable_tunnel(&enabling_state, config).await });
    tokio::time::timeout(Duration::from_secs(5), reached_rx)
        .await
        .unwrap()
        .unwrap();
    let replacement = if matches!(mode.as_str(), "replace" | "disconnect") {
        if mode == "disconnect" {
            enabling.abort();
        }
        let next_state = state.clone();
        let next = tokio::spawn(async move { enable_tunnel(&next_state, config).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.lock().unwrap().tunnel_lifecycles[config].generation != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Some(next)
    } else {
        if mode == "reregister" {
            state
                .lock()
                .unwrap()
                .apps
                .get_mut(config)
                .unwrap()
                .bootstrap_token = "replacement-instance".into();
        } else {
            let disabled = handle_toggle_tunnel(&state, config.to_string(), false).await;
            assert!(matches!(
                disabled,
                Response::TunnelToggled { enabled: false, .. }
            ));
        }
        None
    };
    assert!(state.lock().unwrap().apps[config].tunnel.is_none());
    release_tx.send(()).unwrap();
    if mode == "disconnect" {
        assert!(enabling.await.unwrap_err().is_cancelled());
    } else {
        assert!(
            enabling.await.unwrap().is_err(),
            "superseded enable should report cancellation"
        );
    }
    if let Some(replacement) = replacement {
        replacement.await.unwrap().unwrap();
        assert_eq!(
            state.lock().unwrap().apps[config]
                .tunnel
                .as_ref()
                .unwrap()
                .generation,
            2
        );
    } else {
        assert!(
            state.lock().unwrap().apps[config].tunnel.is_none(),
            "a stale enable undid the acknowledged lifecycle change"
        );
    }
    disable_tunnel(&state, config, TunnelCloseReason::User);
    fixture.await.unwrap();
}
