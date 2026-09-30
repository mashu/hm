//! Radio and the internet together: a gateway between them, and the
//! internet taking over when the radio fails.

use super::*;

/// A radio-only station has never heard of the destination, which is on the
/// internet only. It hands the mail to a relaying gateway it hears (the
/// default route), the gateway carries it over the internet, and the
/// destination's receipt finds its way back through the gateway.
#[test]
fn a_radio_only_station_reaches_an_internet_station_through_a_gateway() {
    let tnc = fake_tnc(0);
    let alice = KeyFile::generate(call("SA0KAM")).unwrap();
    let gateway = KeyFile::generate(call("SM0GW")).unwrap();
    let bob = KeyFile::generate(call("SO5KM-1")).unwrap();
    let (a_db, g_db, b_db) = (Tmp::new("dr-a"), Tmp::new("dr-g"), Tmp::new("dr-b"));
    let g = start_routed(
        Setup {
            key: &gateway,
            me: "SM0GW",
            peer: &alice,
            also: &[&bob],
            tnc: Some(tnc.addr),
            internet: Some(InternetConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                peers: vec![],
            }),
            store: &g_db,
            retry: QUICK,
            beacon_every: Some(Duration::from_secs(2)),
            trust_file: None,
        },
        RelaySettings {
            enabled: true,
            ..RelaySettings::default()
        },
        vec![],
    );
    let b = start(Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &gateway,
        also: &[&alice],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(gateway.call, g.internet_addr.unwrap())],
        }),
        store: &b_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &gateway,
        also: &[&bob],
        tnc: Some(tnc.addr),
        internet: None,
        store: &a_db,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    });
    wait_for(Duration::from_secs(30), "the gateway's link and beacons", || {
        let linked = get(g.http_addr, "/api/status")["internet_peers"] == json!(["SO5KM-1"]);
        // The gateway's beacon says it hears us: a way to it.
        let heard = get(a.http_addr, "/api/status")["heard"]
            .as_array()
            .is_some_and(|h| {
                h.iter().any(|s| {
                    s["station"] == "SM0GW"
                        && s["hears"]
                            .as_array()
                            .is_some_and(|l| l.iter().any(|c| c == "SA0KAM"))
                })
            });
        (linked && heard).then_some(())
    });
    send(
        a.http_addr,
        json!({"to": "SO5KM-1", "subject": "Via the gateway", "text": "A-G-B"}),
    );
    let sent = delivered(a.http_addr, 1, "delivery through the gateway");
    assert_eq!(sent["state"], "Delivered");
    assert_eq!(inbox(b.http_addr)[0]["text"], "A-G-B");
    a.stop().unwrap();
    g.stop().unwrap();
    b.stop().unwrap();
}

/// Both stations have radio and internet, and hear each other's beacons.
/// Mail goes by radio, which costs less; when the band dies, the failures and
/// the silence teach the node that the radio link is closed, and mail moves
/// to the internet by itself.
#[test]
fn radio_first_and_the_internet_when_radio_fails() {
    let tnc = fake_tnc(0);
    let (alice, bob) = keys();
    let (a_db, b_db) = (Tmp::new("a4"), Tmp::new("b4"));
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: Some(tnc.addr),
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &a_db,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    });
    let b = start(Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &alice,
        also: &[],
        tnc: Some(tnc.addr),
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(call("SA0KAM"), a.internet_addr.unwrap())],
        }),
        store: &b_db,
        retry: QUICK,
        beacon_every: Some(Duration::from_secs(2)),
        trust_file: None,
    });
    wait_for(Duration::from_secs(20), "the internet link and the radio", || {
        let st = get(a.http_addr, "/api/status");
        (st["internet_peers"] == json!(["SO5KM"]) && st["radio"] == json!(true)).then_some(())
    });
    // A's operator would rather not pay for the internet while the radio
    // might do: an internet attempt costs a fifth of a message. (At the
    // default cost A, knowing nothing yet of how its handoffs go by radio,
    // draws a doubtful radio now and then and tries the internet first, as
    // exploration should.)
    let internet_cost = |cost: f64| {
        let (status, body) = http(
            a.http_addr,
            "PATCH",
            "/api/settings",
            Some(&json!({ "internet_cost": cost })),
            Some(TOKEN),
        );
        assert_eq!(status, 200, "{body}");
    };
    internet_cost(20.0);
    // Radio goes first once it is known to work: B's beacon says it hears A.
    wait_for(Duration::from_secs(30), "B's beacon, hearing A", || {
        let st = get(a.http_addr, "/api/status");
        st["heard"].as_array().unwrap().iter().find_map(|h| {
            let hears_a = h["hears"]
                .as_array()
                .is_some_and(|l| l.iter().any(|c| c == "SA0KAM"));
            (h["station"] == "SO5KM-1" && hears_a).then_some(())
        })
    });

    send(a.http_addr, json!({"to": "SO5KM-1", "text": "one"}));
    let first = delivered(a.http_addr, 1, "first delivery");
    assert_eq!(first["delivered_by"], "radio");
    // Back to the default: with a radio handoff that worked in its beliefs,
    // A still tries the radio first, and turns to the internet soon once
    // the radio goes quiet.
    internet_cost(2.0);

    tnc.blocked.store(true, Ordering::SeqCst);
    send(a.http_addr, json!({"to": "SO5KM-1", "text": "two"}));
    let second = delivered(a.http_addr, 2, "second delivery");
    assert_eq!(
        (second["delivered_by"].as_str(), second["verified"].as_bool()),
        (Some("internet"), Some(true))
    );
    assert!(
        second["attempts"].as_u64().unwrap() >= 2,
        "radio was tried first: {second}"
    );
    let texts: Vec<String> = inbox(b.http_addr)
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(texts, vec!["two", "one"]);
    let est = get(a.http_addr, "/api/status")["estimates"].clone();
    eprintln!("estimates after the band died: {est}");
    a.stop().unwrap();
    b.stop().unwrap();
}
