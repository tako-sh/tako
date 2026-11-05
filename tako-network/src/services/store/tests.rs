use super::*;

#[test]
fn query_framing_is_bounded_and_requires_complete_response() {
    let valid = b"{\"columns\":[\"server_id\",\"payload\"]}\n{\"row\":[0,[\"server\",\"payload\"]]}\n{\"eoq\":{}}";
    assert_eq!(
        service_rows(valid).unwrap(),
        vec![(json!("server"), json!("payload"))]
    );
    assert!(service_rows(&valid[..valid.len() - 10]).is_err());
    let extra = format!("{}\n{{\"eoq\":{{}}}}", std::str::from_utf8(valid).unwrap());
    assert!(service_rows(extra.as_bytes()).is_err());
    let mut oversized = String::from("{\"columns\":[\"server_id\",\"payload\"]}\n");
    for n in 0..33 {
        oversized.push_str(&format!("{{\"row\":[{n},[\"s\",\"p\"]]}}\n"));
    }
    oversized.push_str("{\"eoq\":{}}");
    assert!(service_rows(oversized.as_bytes()).is_err());
}

use crate::{
    Approval, DeviceKind, LocalDeviceKeys, Permissions, WgChallenge, corrosion::BearerToken,
};
use std::cell::RefCell;

thread_local! {
    static ROWS: RefCell<BTreeMap<String, String>> = RefCell::new(BTreeMap::new());
    static BATCHES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn read_only_transport(path: &str, body: &Value) -> RuntimeResult<Vec<u8>> {
    assert_eq!(path, "/v1/queries");
    let sql = body[0].as_str().unwrap();
    assert!(sql.starts_with("SELECT server_id, CASE WHEN length(CAST(payload AS BLOB)) <= 65536"));
    assert!(sql.contains("WHERE server_id IN ("));
    let requested = body[1].as_array().unwrap();
    assert!(requested.len() <= 32);
    BATCHES.with_borrow_mut(|b| b.push(requested.len()));
    let mut lines = vec![json!({"columns":["server_id","payload"]}).to_string()];
    ROWS.with_borrow(|rows| {
        for (index, id) in requested.iter().enumerate() {
            if let Some(payload) = rows.get(id.as_str().unwrap()) {
                let payload = if payload.len() <= MAX_LEASE_BYTES {
                    json!(payload)
                } else {
                    Value::Null
                };
                lines.push(json!({"row":[index,[id,payload]]}).to_string());
            }
        }
    });
    lines.push(json!({"eoq":{}}).to_string());
    Ok(lines.join("\n").into_bytes())
}

#[test]
fn only_admitted_servers_are_queried_in_batches_and_bad_leases_are_isolated() {
    let (mut registry, root_keys) = crate::services::tests::fixture();
    let mut keys = vec![root_keys];
    for _ in 0..34 {
        let key = LocalDeviceKeys::generate().unwrap();
        let request = key
            .enrollment_request(None, DeviceKind::Server, "server".parse().unwrap())
            .unwrap();
        let (challenge, pending) =
            WgChallenge::begin(&request, registry.network.id.clone(), 2, 60).unwrap();
        let verified = pending
            .verify(&key.answer_challenge(&challenge, &request, 2).unwrap(), 2)
            .unwrap();
        let member = registry
            .approve(
                verified,
                Approval {
                    kind: registry.members[0].kind.clone(),
                    permissions: Permissions {
                        server_admin: true,
                        ..Default::default()
                    },
                    approved_by: Some(registry.members[0].device_id.clone()),
                    independent_admin: false,
                    created_at: 2,
                    expires_at: None,
                },
            )
            .unwrap();
        registry.members.push(member);
        keys.push(key);
    }
    let leases: Vec<_> = keys
        .iter()
        .map(|key| {
            ServiceLease::sign(
                key,
                registry.network.id.clone(),
                ["api/prod".parse().unwrap()].into(),
                100,
            )
            .unwrap()
        })
        .collect();
    ROWS.with_borrow_mut(|rows| {
        rows.clear();
        for lease in &leases {
            rows.insert(
                lease.server_id.to_string(),
                serde_json::to_string(lease).unwrap(),
            );
        }
        rows.insert(leases[1].server_id.to_string(), "not JSON".into());
        let expired = ServiceLease::sign(
            &keys[2],
            registry.network.id.clone(),
            Default::default(),
            10,
        )
        .unwrap();
        rows.insert(
            leases[2].server_id.to_string(),
            serde_json::to_string(&expired).unwrap(),
        );
        rows.insert(
            leases[3].server_id.to_string(),
            serde_json::to_string(&leases[4]).unwrap(),
        );
        rows.insert(
            leases[5].server_id.to_string(),
            "x".repeat(MAX_LEASE_BYTES + 1),
        );
        let mut tampered = leases[6].clone();
        tampered.apps.clear();
        rows.insert(
            leases[6].server_id.to_string(),
            serde_json::to_string(&tampered).unwrap(),
        );
        rows.insert("not-an-admitted-server".into(), "unread".into());
    });
    BATCHES.with_borrow_mut(Vec::clear);
    let mut store = CorrosionStore::new(
        "http://127.0.0.1:9834",
        BearerToken::new("fixture-token-not-a-credential".into()).unwrap(),
        registry.network.clone(),
    )
    .unwrap();
    store.transport = Some(read_only_transport);
    let valid = store.service_leases(&registry, 100).unwrap();
    assert_eq!(valid.len(), 30);
    BATCHES.with_borrow(|b| assert_eq!(b, &[32, 3]));
    for i in [1, 2, 3, 5, 6] {
        assert!(
            !valid
                .iter()
                .any(|lease| lease.server_id == leases[i].server_id)
        );
    }
    registry.members[1].device_id = "00".repeat(32).parse().unwrap();
    assert!(store.service_leases(&registry, 100).is_err());
    BATCHES.with_borrow(|b| assert_eq!(b, &[32, 3]));
}
