//! The message API: dropping queued messages and clearing conversations.

use super::*;

#[test]
fn queued_outbound_message_can_be_dropped() {
    let (alice, bob) = keys();
    let db = Tmp::new("cancel");
    let node = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: None,
        internet: None,
        store: &db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let (status, body) = http(
        node.http_addr,
        "POST",
        "/api/send",
        Some(&json!({"to": "SO5KM-1", "text": "cancel me"})),
        Some(TOKEN),
    );
    assert_eq!(status, 201, "{body}");
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let detail = get(node.http_addr, &format!("/api/messages/{id}"));
    assert_eq!(
        (detail["id"].as_str(), detail["state"].as_str()),
        (Some(id.as_str()), Some("Queued"))
    );
    assert!(detail["raw_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        detail["raw_hex"].as_str().unwrap().len(),
        detail["raw_bytes"].as_u64().unwrap() as usize * 2
    );
    assert_eq!(
        http(
            node.http_addr,
            "DELETE",
            &format!("/api/messages/{id}"),
            None,
            Some(TOKEN)
        )
        .0,
        204
    );
    assert_eq!(
        get(node.http_addr, "/api/messages?direction=out")[0]["state"],
        "Cancelled"
    );
    assert_eq!(
        http(
            node.http_addr,
            "DELETE",
            &format!("/api/messages/{id}"),
            None,
            Some(TOKEN)
        )
        .0,
        204
    );
    assert!(get(node.http_addr, "/api/messages?direction=out")
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        http(
            node.http_addr,
            "DELETE",
            &format!("/api/messages/{id}"),
            None,
            Some(TOKEN)
        )
        .0,
        404
    );

    send(node.http_addr, json!({"to": "SO5KM-1", "text": "keep pending"}));
    let (status, body) = http(
        node.http_addr,
        "DELETE",
        "/api/conversations/SO5KM-1",
        None,
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let cleared: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (cleared["deleted"].as_u64(), cleared["active"].as_u64()),
        (Some(0), Some(1))
    );
    assert_eq!(
        get(node.http_addr, "/api/messages?direction=out")[0]["state"],
        "Queued"
    );
    let pending_id = get(node.http_addr, "/api/messages?direction=out")[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        http(
            node.http_addr,
            "DELETE",
            &format!("/api/messages/{pending_id}"),
            None,
            Some(TOKEN)
        )
        .0,
        204
    );
    let (status, body) = http(
        node.http_addr,
        "DELETE",
        "/api/conversations/SO5KM-1",
        None,
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let cleared: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (cleared["deleted"].as_u64(), cleared["active"].as_u64()),
        (Some(1), Some(0))
    );
    assert!(get(node.http_addr, "/api/messages?direction=out")
        .as_array()
        .unwrap()
        .is_empty());
    node.stop().unwrap();
}
