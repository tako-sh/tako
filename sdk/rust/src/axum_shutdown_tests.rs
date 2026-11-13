use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    child: Child,
    directory: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn serve_drains_requests_and_returns_for_app_cleanup() {
    if let Some(directory) = std::env::var_os("TAKO_SHUTDOWN_TEST_DIR") {
        let directory = PathBuf::from(directory);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let entered = directory.join("entered");
            let router = axum::Router::new().route(
                "/slow",
                axum::routing::get(move || {
                    let entered = entered.clone();
                    async move {
                        std::fs::write(entered, "ready").unwrap();
                        tokio::time::sleep(Duration::from_millis(400)).await;
                        "finished"
                    }
                }),
            );
            crate::axum::serve(router).await.unwrap();
            std::fs::write(directory.join("cleaned"), "done").unwrap();
        });
        return;
    }

    for signal in [libc::SIGTERM, libc::SIGINT] {
        let directory = std::env::temp_dir().join(format!(
            "tako-rust-shutdown-{}-{signal}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "axum_shutdown_tests::serve_drains_requests_and_returns_for_app_cleanup",
            ])
            .env("TAKO_SHUTDOWN_TEST_DIR", &directory)
            .env("HOST", "127.0.0.1")
            .env("PORT", address.port().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut fixture = Fixture { child, directory };
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            if let Ok(stream) = TcpStream::connect(address) {
                break stream;
            }
            assert!(
                fixture.child.try_wait().unwrap().is_none(),
                "server exited before readiness"
            );
            assert!(Instant::now() < deadline, "server did not bind");
            std::thread::sleep(Duration::from_millis(10));
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        while !fixture.directory.join("entered").exists() {
            assert!(
                Instant::now() < deadline,
                "request did not enter its handler"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(unsafe { libc::kill(fixture.child.id() as i32, signal) }, 0);
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.ends_with("finished"), "{response}");
        loop {
            if let Some(status) = fixture.child.try_wait().unwrap() {
                assert!(status.success(), "server did not return normally: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "server did not finish shutdown");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(fixture.directory.join("cleaned")).unwrap(),
            "done"
        );
    }
}
