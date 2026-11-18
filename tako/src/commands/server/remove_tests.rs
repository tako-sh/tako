use super::*;
use crate::{
    config::{ServerEntry, ServerInventory},
    management_http::NativeTarget,
    network::tests::fixture,
};
use std::cell::{Cell, RefCell};

fn native_server() -> ServerEntry {
    let fixture = fixture();
    ServerEntry {
        native: Some(NativeTarget {
            network: crate::config::LocalNetwork {
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
async fn native_removal_requires_publication_with_ssh_still_connected() {
    // A lost/denied publication must leave the remote service and its keys intact.
    for publish_succeeds in [true, false] {
        let server = native_server();
        let events = RefCell::new(Vec::new());
        let connected = Cell::new(false);
        let result = remove_remote_with(
            &server,
            Removal::Reset,
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
            |_| None,
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
            assert!(error.contains("Removal incomplete"), "{error}");
            assert!(
                error.contains("could not confirm network revocation"),
                "{error}"
            );
            assert!(
                error.contains("nothing on the server was deleted"),
                "{error}"
            );
            assert_eq!(*events.borrow(), ["connect", "revoke"]);
        }
        assert!(!connected.get(), "release SSH after completion or failure");
    }
}

#[tokio::test]
async fn unavailable_ssh_does_not_revoke_or_delete_the_server() {
    let error = remove_remote_with(
        &native_server(),
        Removal::Reset,
        async { Err::<(), _>("SSH unavailable".into()) },
        |_| panic!("must establish SSH before revoking"),
        |_| panic!("must not check before revoking"),
        async |()| panic!("must not remove the server"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "SSH unavailable");
}

#[tokio::test]
async fn remote_cleanup_failure_reports_revoked_membership_and_how_to_finish() {
    let revoked = Cell::new(false);
    let error = remove_remote_with(
        &native_server(),
        Removal::Reset,
        async { Ok(()) },
        |_| {
            revoked.set(true);
            Ok(())
        },
        |_| None,
        async |()| {
            assert!(revoked.get());
            Err("remote helper failed".into())
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Removal incomplete"), "{error}");
    assert!(error.contains("network membership is revoked"), "{error}");
    assert!(
        error.contains("sudo /usr/local/bin/tako-server-service reset"),
        "{error}"
    );
    assert!(error.contains("remote helper failed"), "{error}");
}

#[tokio::test]
async fn becoming_the_last_server_after_revocation_keeps_its_data() {
    // Another operator removed the other server meanwhile: stop before wiping.
    let revoked = Cell::new(false);
    let error = remove_remote_with(
        &native_server(),
        Removal::Reset,
        async { Ok(()) },
        |_| {
            revoked.set(true);
            Ok(())
        },
        |target| {
            assert!(revoked.get(), "check after revocation");
            Some(format!(
                "is the last server in network {}",
                target.network.name
            ))
        },
        async |()| panic!("must not delete the last server's data without --yes"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Removal stopped"), "{error}");
    assert!(error.contains("node is revoked"), "{error}");
    assert!(error.contains("apps and data were kept"), "{error}");
    assert!(error.contains("last server in network fleet"), "{error}");
    assert!(
        error.contains("sudo /usr/local/bin/tako-server-service reset"),
        "{error}"
    );
}

#[tokio::test]
async fn legacy_removal_does_not_require_network_revocation() {
    let removed = Cell::new(false);
    remove_remote_with(
        &ServerEntry::default(),
        Removal::Uninstall,
        async { Ok(()) },
        |_| panic!("legacy servers have no native membership"),
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

#[test]
fn removal_runs_the_approved_helper_action() {
    assert_eq!(
        build_helper_command(Removal::Reset),
        crate::ssh::SshClient::run_as_root("/usr/local/bin/tako-server-service reset")
    );
    assert_eq!(
        build_helper_command(Removal::Uninstall),
        crate::ssh::SshClient::run_as_root("/usr/local/bin/tako-server-service implode")
    );
}

#[test]
fn the_last_server_of_a_network_is_detected() {
    let server = native_server();
    let mut servers = ServerInventory::default();
    servers.servers.insert("fleet/node".into(), server.clone());
    assert!(is_last_server(&servers, &server));

    let mut other = server.clone();
    if let Some(target) = &mut other.native {
        target.server_id = target.network.device.parse().unwrap();
        target.server_name = "edge".parse().unwrap();
    }
    servers.servers.insert("fleet/edge".into(), other);
    assert!(!is_last_server(&servers, &server));
    assert!(!is_last_server(&servers, &ServerEntry::default()));
}
