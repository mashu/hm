//! Settings changed while the node runs: trusted stations, internet peers,
//! radio settings.

use super::*;

/// Trust and settings change without a restart: through the API (saved to
/// the settings file) and by editing that file by hand.
#[test]
fn trusted_stations_change_while_the_node_runs() {
    let (alice, bob) = keys();
    let (a_db, h_db) = (Tmp::new("trust-a"), Tmp::new("trust-h"));
    let trust_file = std::env::temp_dir().join(format!("hm-node-trust-{}.toml", std::process::id()));
    std::fs::write(&trust_file, "# stations this hub trusts\n").unwrap();
    let hub = start(Setup {
        key: &bob,
        me: "SO5KM",
        peer: &alice,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &h_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: Some(trust_file.clone()),
    });
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(call("SO5KM"), hub.internet_addr.unwrap())],
        }),
        store: &a_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let linked = |n: &NodeHandle| {
        get(n.http_addr, "/api/status")["internet_peers"]
            .as_array()
            .is_some_and(|p| !p.is_empty())
    };
    // The hub trusts nobody yet: no link, the mail waits.
    send(a.http_addr, json!({"to": "SO5KM", "text": "first"}));
    thread::sleep(Duration::from_secs(3));
    assert!(!linked(&a) && inbox(hub.http_addr).is_empty());

    // Trusted through the API: saved, and the link comes up without a restart.
    let (status, body) = http(
        hub.http_addr,
        "POST",
        "/api/trust",
        Some(&json!({"line": alice.trust_line()})),
        Some(TOKEN),
    );
    assert_eq!(status, 201, "{body}");
    let list = get(hub.http_addr, "/api/trust");
    assert_eq!(list["stations"][0]["station"], "SA0KAM");
    let saved = std::fs::read_to_string(&trust_file).unwrap();
    assert!(saved.starts_with("# stations this hub trusts\n"), "{saved}");
    assert_eq!(Config::load(&trust_file).unwrap().trust[0].station, "SA0KAM");

    // Settings change through the API too: in use at once, and saved.
    let (status, body) = http(
        hub.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({
            "internet_cost": 0.5,
            "retry_attempts": 7,
            "locator": "ko02md",
            "custody_grace_secs": 3600,
            "custody_suspect_secs": 7200,
            "receipt_retry_attempts": 9,
            "relay": { "enabled": true, "mailbox": true, "max_hops": 4 },
            "radio": { "max_rounds": 5 },
        })),
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let now = get(hub.http_addr, "/api/settings");
    assert_eq!(now["live"]["locator"], "KO02md");
    assert_eq!(get(hub.http_addr, "/api/status")["locator"], "KO02md");
    assert_eq!(
        (
            now["live"]["internet_cost"].as_f64(),
            now["live"]["retry_attempts"].as_u64(),
            now["live"]["custody_grace_secs"].as_u64(),
            now["live"]["custody_suspect_secs"].as_u64(),
            now["live"]["receipt_retry_attempts"].as_u64(),
            now["live"]["relay"]["enabled"].as_bool(),
            now["live"]["relay"]["mailbox"].as_bool(),
            now["live"]["relay"]["max_hops"].as_u64(),
            now["live"]["radio"]["max_rounds"].as_u64(),
        ),
        (
            Some(0.5),
            Some(7),
            Some(3600),
            Some(7200),
            Some(9),
            Some(true),
            Some(true),
            Some(4),
            Some(5)
        )
    );
    let c = Config::load(&trust_file).unwrap();
    assert_eq!((c.delivery.internet_cost, c.delivery.retry_attempts), (0.5, 7));
    assert_eq!(
        (
            c.delivery.custody_grace_secs,
            c.delivery.custody_suspect_secs,
            c.delivery.receipt_retry_attempts,
            c.relay.enabled,
            c.relay.mailbox,
            c.relay.max_hops,
            c.radio.max_rounds,
        ),
        (3600, 7200, 9, true, true, 4, 5)
    );
    assert_eq!(c.station.locator.as_deref(), Some("KO02md"));
    let (status, body) = http(
        hub.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({
            "restart_to_change": {
                "internet_listen": "127.0.0.1:9443",
                "open_hub": true,
                "modem": { "enabled": true, "kind": "ardop", "host": "127.0.0.1", "port": 8515, "bandwidth": 0, "ptt": "none" },
            }
        })),
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let fixed = get(hub.http_addr, "/api/settings")["restart_to_change"].clone();
    assert_eq!(fixed["internet_listen"], "127.0.0.1:9443");
    assert_eq!(fixed["open_hub"], true);
    assert_eq!(fixed["modem"]["enabled"], true);
    assert_eq!(fixed["modem"]["kind"], "ardop");
    assert_eq!(fixed["modem"]["port"], 8515);
    let saved = Config::load(&trust_file).unwrap();
    assert_eq!(saved.internet.listen.as_deref(), Some("127.0.0.1:9443"));
    assert!(saved.internet.open_hub);
    assert!(saved.modem.enabled);
    assert_eq!(saved.modem.kind, "ardop");
    assert_eq!(saved.modem.port, 8515);
    let (status, _) = http(
        hub.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({"radio_cost": -1})),
        Some(TOKEN),
    );
    assert_eq!(status, 400);
    delivered(a.http_addr, 1, "delivery once trusted");
    assert_eq!(inbox(hub.http_addr)[0]["verified"], true);

    // Removed through the API: the link goes at once, and further mail waits.
    let (status, _) = http(hub.http_addr, "DELETE", "/api/trust/SA0KAM", None, Some(TOKEN));
    assert_eq!(status, 204);
    assert_eq!(
        http(hub.http_addr, "DELETE", "/api/trust/SA0KAM", None, Some(TOKEN)).0,
        404
    );
    wait_for(Duration::from_secs(10), "the link to drop", || {
        (!linked(&hub)).then_some(())
    });
    send(a.http_addr, json!({"to": "SO5KM", "text": "second"}));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(inbox(hub.http_addr).len(), 1);

    // Trusted again by editing the file by hand.
    let mut text = std::fs::read_to_string(&trust_file).unwrap();
    let (c, k) = alice
        .trust_line()
        .split_once(' ')
        .map(|(c, k)| (c.to_string(), k.to_string()))
        .unwrap();
    text.push_str(&format!(
        "\n[[trust]]\nstation = \"{c}\"\nkey = \"{k}\"\nnote = \"added by hand\"\n"
    ));
    std::fs::write(&trust_file, text).unwrap();
    wait_for(Duration::from_secs(30), "the second message", || {
        (inbox(hub.http_addr).len() == 2).then_some(())
    });
    // Bad requests are refused.
    let (status, _) = http(
        hub.http_addr,
        "POST",
        "/api/trust",
        Some(&json!({"line": "SA0KAM nothex"})),
        Some(TOKEN),
    );
    assert_eq!(status, 400);
    assert_eq!(http(hub.http_addr, "GET", "/api/trust", None, None).0, 401);
    a.stop().unwrap();
    hub.stop().unwrap();
    std::fs::remove_file(&trust_file).unwrap();
}

/// A radio-only node can gain an internet peer without a restart: the dial
/// stack starts when the first peer is added.
#[test]
fn internet_peer_added_while_radio_only_node_runs() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, h_db) = (Tmp::new("dial-later-a"), Tmp::new("dial-later-h"));
    let settings = std::env::temp_dir().join(format!("hm-node-dial-later-{}.toml", std::process::id()));
    std::fs::write(
        &settings,
        format!("[[trust]]\nstation = {:?}\nkey = {:?}\n", bob.call.to_string(), {
            let line = bob.trust_line();
            line.split_once(' ').unwrap().1.to_string()
        }),
    )
    .unwrap();
    let hub = start(Setup {
        key: &bob,
        me: "SO5KM",
        peer: &alice,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &h_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
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
        trust_file: Some(settings.clone()),
    });
    assert!(get(a.http_addr, "/api/status")["internet_listen"].is_null());
    let hub_addr = hub.internet_addr.unwrap();
    let (status, body) = http(
        a.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({
            "peers": [{"station": "SO5KM", "address": hub_addr.to_string()}]
        })),
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    wait_for(Duration::from_secs(15), "internet dial after peer add", || {
        let st = get(a.http_addr, "/api/status");
        (st["internet_peers"] == json!(["SO5KM"]) && !st["internet_listen"].is_null()).then_some(())
    });
    send(a.http_addr, json!({"to": "SO5KM", "text": "dialled after start"}));
    delivered(a.http_addr, 1, "internet delivery without restart");
    assert_eq!(inbox(hub.http_addr)[0]["text"], "dialled after start");
    a.stop().unwrap();
    hub.stop().unwrap();
    let _ = std::fs::remove_file(&settings);
}

/// Radio settings change without a restart: the node opens the link the new
/// settings describe, and mail waiting for the radio goes out on it.
#[test]
fn radio_settings_change_while_the_node_runs() {
    let (here, there) = (fake_tnc(0), fake_tnc(0));
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("radio-a"), Tmp::new("radio-b"));
    let setup = |key, me, peer, tnc, store| Setup {
        key,
        me,
        peer,
        also: &[],
        tnc: Some(tnc),
        internet: None,
        store,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    };
    // Bob listens on the other channel; nothing Alice sends reaches him.
    let b = start(setup(&bob, "SO5KM-1", &alice, there.addr, &b_db));
    let a = start(setup(&alice, "SA0KAM", &bob, here.addr, &a_db));
    let up_on = |n: &NodeHandle, addr: SocketAddr| {
        let st = get(n.http_addr, "/api/status");
        (st["radio"] == true
            && st["radio_via"]
                .as_str()
                .is_some_and(|v| v.contains(&addr.to_string())))
        .then_some(())
    };
    wait_for(Duration::from_secs(10), "the radio up", || up_on(&a, here.addr));
    send(
        a.http_addr,
        json!({"to": "SO5KM-1", "text": "over the other channel"}),
    );
    thread::sleep(Duration::from_secs(2));
    assert!(inbox(b.http_addr).is_empty());

    let (status, body) = http(
        a.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({"radio": {"kiss": there.addr.to_string()}})),
        Some(TOKEN),
    );
    assert_eq!(status, 200, "{body}");
    let now: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(now["live"]["radio"]["kiss"], there.addr.to_string());
    assert_eq!(now["radio_applies_now"], true);
    wait_for(Duration::from_secs(10), "the radio on the new TNC", || {
        up_on(&a, there.addr)
    });
    delivered(a.http_addr, 1, "delivery on the new channel");
    assert_eq!(inbox(b.http_addr)[0]["text"], "over the other channel");

    // Settings that cannot work are refused and change nothing.
    let (status, _) = http(
        a.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({"radio": {"kiss": "serial:"}})),
        Some(TOKEN),
    );
    assert_eq!(status, 400);
    // Switched off: the status says no radio.
    let (status, _) = http(
        a.http_addr,
        "PATCH",
        "/api/settings",
        Some(&json!({"radio": {"enabled": false}})),
        Some(TOKEN),
    );
    assert_eq!(status, 200);
    wait_for(Duration::from_secs(10), "the radio off", || {
        get(a.http_addr, "/api/status")["radio"].is_null().then_some(())
    });
    a.stop().unwrap();
    b.stop().unwrap();
}
