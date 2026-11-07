use super::*;
use crate::{config::ServerEntry, management_http::NativeTarget, network::tests::fixture};
use std::cell::{Cell, RefCell};

fn native_server() -> ServerEntry {
    let fixture = fixture();
    ServerEntry {
        native: Some(NativeTarget {
            network: crate::config::NetworkAlias {
                name: "fleet".into(),
                id: fixture.profile.snapshot().network.id.to_string(),
                device: fixture.profile.snapshot().network.initial_admin.to_string(),
            },
            server_id: fixture.server,
            server_name: "node".parse().unwrap(),
        }),
        ..Default::default()
    }
}

struct Connection<'a>(&'a Cell<bool>);

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

#[tokio::test]
async fn native_uninstall_requires_publication_with_ssh_still_connected() {
    // A lost/denied publication must leave the remote service and its keys intact.
    for publish_succeeds in [true, false] {
        let server = native_server();
        let events = RefCell::new(Vec::new());
        let connected = Cell::new(false);
        let result = uninstall_remote_with(
            &server,
            async {
                events.borrow_mut().push("connect");
                connected.set(true);
                Ok(Connection(&connected))
            },
            |target| {
                assert_eq!(Some(target), server.native.as_ref());
                assert!(connected.get(), "revocation must retain the SSH session");
                events.borrow_mut().push("revoke");
                if publish_succeeds {
                    Ok(())
                } else {
                    Err("authority unreachable or response lost".into())
                }
            },
            async |_connection| {
                assert!(connected.get(), "cleanup must reuse the live SSH session");
                events.borrow_mut().push("remove");
                Ok(())
            },
        )
        .await;
        if publish_succeeds {
            result.unwrap();
            assert_eq!(*events.borrow(), ["connect", "revoke", "remove"]);
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("Uninstall incomplete"), "{error}");
            assert!(
                error.contains("could not confirm network revocation"),
                "{error}"
            );
            assert!(
                error.contains("server files have not been removed"),
                "{error}"
            );
            assert_eq!(*events.borrow(), ["connect", "revoke"]);
        }
        assert!(!connected.get(), "release SSH after completion or failure");
    }
}

#[tokio::test]
async fn unavailable_ssh_does_not_revoke_or_delete_the_server() {
    let error = uninstall_remote_with(
        &native_server(),
        async { Err::<(), _>("SSH unavailable".into()) },
        |_| panic!("must establish SSH before revoking"),
        async |()| panic!("must not remove the server"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "SSH unavailable");
}

#[tokio::test]
async fn remote_cleanup_failure_reports_revoked_membership_and_incomplete_uninstall() {
    let revoked = Cell::new(false);
    let error = uninstall_remote_with(
        &native_server(),
        async { Ok(()) },
        |_| {
            revoked.set(true);
            Ok(())
        },
        async |()| {
            assert!(revoked.get());
            Err("remote helper failed".into())
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Uninstall incomplete"), "{error}");
    assert!(error.contains("network membership is revoked"), "{error}");
    assert!(error.contains("remote helper failed"), "{error}");
}

#[tokio::test]
async fn legacy_uninstall_does_not_require_network_revocation() {
    let removed = Cell::new(false);
    uninstall_remote_with(
        &ServerEntry::default(),
        async { Ok(()) },
        |_| panic!("legacy servers have no native membership"),
        async |()| {
            removed.set(true);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert!(removed.get());
}
