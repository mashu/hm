//! Stations on one radio channel: mail both ways, restarts, a receiver off
//! the air, beacons, two SSIDs on one callsign.

use super::*;

#[test]
fn radio_nodes_exchange_mail_and_survive_a_restart() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("a1"), Tmp::new("b1"));
    let b_setup = || Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &alice,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &b_db,
        retry: RetryPolicy::default(),
        beacon_every: None,
        trust_file: None,
    };
    let b = start(b_setup());
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &a_db,
        retry: RetryPolicy::default(),
        beacon_every: None,
        trust_file: None,
    });

    // The page is public; the API wants the token.
    let (status, page) = http(a.http_addr, "GET", "/", None, None);
    assert!(status == 200 && page.contains("Queue message"));
    assert!(
        page.contains("Archive")
            && page.contains("Clear history")
            && page.contains("View message details and raw signed object")
            && page.contains("Drop this queued message")
    );
    let headers = raw_http(a.http_addr, "GET", "/", None, None).to_ascii_lowercase();
    assert!(headers.contains("content-security-policy:"));
    assert!(headers.contains("permissions-policy:"));
    assert!(headers.contains("x-frame-options: deny"));
    assert!(headers.contains("cache-control: no-store"));
    assert_eq!(http(a.http_addr, "GET", "/api/status", None, None).0, 401);
    assert_eq!(
        http(a.http_addr, "GET", "/api/status", None, Some("wrong")).0,
        401
    );
    let (status, err) = http(
        a.http_addr,
        "POST",
        "/api/send",
        Some(&json!({"to": "NOT A CALL", "text": "x"})),
        Some(TOKEN),
    );
    assert!(status == 400 && err.contains("to:"), "{status} {err}");
    let st = get(a.http_addr, "/api/status");
    assert_eq!(
        (st["call"].as_str(), st["internet_listen"].is_null()),
        (Some("SA0KAM"), true)
    );

    send(
        a.http_addr,
        json!({"to": "SO5KM-1", "subject": "Sked", "text": "40m 7.047 at 19Z?", "precedence": "priority"}),
    );
    let got = wait_for(Duration::from_secs(30), "the message at SO5KM-1", || {
        let i = inbox(b.http_addr);
        (i.len() == 1).then(|| i[0].clone())
    });
    assert_eq!(
        (got["from"].as_str(), got["subject"].as_str()),
        (Some("SA0KAM"), Some("Sked"))
    );
    assert_eq!(
        (got["verified"].as_bool(), got["state"].as_str()),
        (Some(true), Some("Unread"))
    );
    let sent = delivered(a.http_addr, 1, "delivery");
    assert_eq!(
        (sent["verified"].as_bool(), sent["delivered_by"].as_str()),
        (Some(true), Some("radio"))
    );

    let id = got["id"].as_str().unwrap();
    assert_eq!(
        http(b.http_addr, "POST", &format!("/api/read/{id}"), None, Some(TOKEN)).0,
        204
    );
    b.stop().unwrap();
    let b = start(b_setup());
    let i = inbox(b.http_addr);
    assert_eq!((i.len(), i[0]["state"].as_str()), (1, Some("Read")));
    a.stop().unwrap();
    b.stop().unwrap();
}

#[test]
fn mail_waits_while_the_receiver_is_off_air() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("a2"), Tmp::new("b2"));
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &a_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    send(a.http_addr, json!({"to": "SO5KM-1", "text": "are you there?"}));
    let queued = wait_for(Duration::from_secs(60), "a failed attempt", || {
        let out = get(a.http_addr, "/api/messages?direction=out");
        (out[0]["attempts"].as_u64().unwrap() >= 1).then(|| out[0].clone())
    });
    assert_eq!(queued["state"], "Queued");
    assert!(
        queued["note"].as_str().unwrap().starts_with("NoAnswer"),
        "{queued}"
    );
    // The receiver comes on the air and says so: its beacon wakes the mail.
    let b = start(Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &alice,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store: &b_db,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    });
    delivered(a.http_addr, 1, "delivery after the receiver came up");
    assert_eq!(inbox(b.http_addr)[0]["text"], "are you there?");
    a.stop().unwrap();
    b.stop().unwrap();
}

#[test]
fn radio_nodes_beacon_and_list_each_other() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("a-beacon"), Tmp::new("b-beacon"));
    let setup = |key, me, peer, store| Setup {
        key,
        me,
        peer,
        also: &[],
        tnc: Some(tnc.addr),
        internet: None,
        store,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    };
    let a = start(setup(&alice, "SA0KAM", &bob, &a_db));
    let b = start(setup(&bob, "SO5KM-1", &alice, &b_db));
    // Each lists the other as trusted, and the other's beacon says it hears us.
    for (node, me, other) in [(&a, "SA0KAM", "SO5KM-1"), (&b, "SO5KM-1", "SA0KAM")] {
        let seen = wait_for(Duration::from_secs(30), "a beacon listing us", || {
            let st = get(node.http_addr, "/api/status");
            st["heard"].as_array().unwrap().iter().find_map(|h| {
                let hears_us = h["hears"].as_array().is_some_and(|l| l.iter().any(|c| c == me));
                (h["station"] == other && hears_us).then(|| h.clone())
            })
        });
        assert_eq!(seen["key"], "trusted", "{seen}");
        assert_eq!(seen["offers"], serde_json::json!([]), "{seen}");
        assert!(seen["clock_offset"].as_i64().unwrap().abs() <= 5, "{seen}");
        // Both give JO89xi: the same square, no distance.
        assert_eq!(
            (seen["locator"].as_str(), seen["distance_km"].as_f64()),
            (Some("JO89xi"), Some(0.0)),
            "{seen}"
        );
    }
    a.stop().unwrap();
    b.stop().unwrap();
}

/// Two stations of one operator, SA0KAM-1 and SA0KAM-2, each with its own key:
/// mail to one reaches that one only, with a receipt from its own key.
#[test]
fn two_ssids_are_two_stations_with_their_own_keys() {
    let tnc = fake_tnc(0);
    let sender = KeyFile::generate(call("SO5KM")).unwrap();
    let home = KeyFile::generate(call("SA0KAM-1")).unwrap();
    let server = KeyFile::generate(call("SA0KAM-2")).unwrap();
    let (s_db, h_db, v_db) = (Tmp::new("ssid-s"), Tmp::new("ssid-1"), Tmp::new("ssid-2"));
    let s = start(Setup {
        key: &sender,
        me: "SO5KM",
        peer: &home,
        also: &[&server],
        tnc: Some(tnc.addr),
        internet: None,
        store: &s_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let station = |key, me, store| {
        start(Setup {
            key,
            me,
            peer: &sender,
            also: &[],
            tnc: Some(tnc.addr),
            internet: None,
            store,
            retry: QUICK,
            beacon_every: None,
            trust_file: None,
        })
    };
    let h = station(&home, "SA0KAM-1", &h_db);
    let v = station(&server, "SA0KAM-2", &v_db);

    send(s.http_addr, json!({"to": "SA0KAM-2", "text": "for the server"}));
    let got = wait_for(Duration::from_secs(60), "the message at SA0KAM-2", || {
        let i = inbox(v.http_addr);
        (i.len() == 1).then(|| i[0].clone())
    });
    assert_eq!(
        (got["text"].as_str(), got["verified"].as_bool()),
        (Some("for the server"), Some(true))
    );
    let sent = delivered(s.http_addr, 1, "delivery to SA0KAM-2");
    assert_eq!(
        sent["verified"].as_bool(),
        Some(true),
        "receipt from SA0KAM-2's own key"
    );

    send(s.http_addr, json!({"to": "SA0KAM-1", "text": "for home"}));
    let got = wait_for(Duration::from_secs(60), "the message at SA0KAM-1", || {
        let i = inbox(h.http_addr);
        (i.len() == 1).then(|| i[0].clone())
    });
    assert_eq!(got["text"].as_str(), Some("for home"));
    delivered(s.http_addr, 2, "delivery to SA0KAM-1");
    // Each station holds only its own mail.
    assert_eq!(inbox(v.http_addr).len(), 1);
    assert_eq!(inbox(h.http_addr).len(), 1);
    for n in [s, h, v] {
        n.stop().unwrap();
    }
}
