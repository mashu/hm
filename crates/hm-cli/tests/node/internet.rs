//! Stations linked over the internet only, directly and through relays.

use super::*;

#[test]
fn four_internet_nodes_relay_end_to_end_without_flooding() {
    let alice = KeyFile::generate(call("SA0KAM")).unwrap();
    let relay_one = KeyFile::generate(call("SM0R1")).unwrap();
    let relay_two = KeyFile::generate(call("SM0R2")).unwrap();
    let bob = KeyFile::generate(call("SO5KM-1")).unwrap();
    let (a_db, r1_db, r2_db, b_db) = (
        Tmp::new("route-a"),
        Tmp::new("route-r1"),
        Tmp::new("route-r2"),
        Tmp::new("route-b"),
    );
    let b = start(Setup {
        key: &bob,
        me: "SO5KM-1",
        peer: &relay_two,
        also: &[&alice, &relay_one],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &b_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let relay = RelaySettings {
        enabled: true,
        mailbox: true,
        ..RelaySettings::default()
    };
    let r2 = start_routed(
        Setup {
            key: &relay_two,
            me: "SM0R2",
            peer: &bob,
            also: &[&alice, &relay_one],
            tnc: None,
            internet: Some(InternetConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                peers: vec![(bob.call, b.internet_addr.unwrap())],
            }),
            store: &r2_db,
            retry: QUICK,
            beacon_every: None,
            trust_file: None,
        },
        relay.clone(),
        vec![],
    );
    let r1 = start_routed(
        Setup {
            key: &relay_one,
            me: "SM0R1",
            peer: &relay_two,
            also: &[&alice, &bob],
            tnc: None,
            internet: Some(InternetConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                peers: vec![(relay_two.call, r2.internet_addr.unwrap())],
            }),
            store: &r1_db,
            retry: QUICK,
            beacon_every: None,
            trust_file: None,
        },
        relay,
        vec![],
    );
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &relay_one,
        also: &[&relay_two, &bob],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(relay_one.call, r1.internet_addr.unwrap())],
        }),
        store: &a_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    wait_for(Duration::from_secs(20), "the three authenticated links", || {
        let links = [
            get(a.http_addr, "/api/status")["internet_peers"]
                .as_array()
                .unwrap()
                .len(),
            get(r1.http_addr, "/api/status")["internet_peers"]
                .as_array()
                .unwrap()
                .len(),
            get(r2.http_addr, "/api/status")["internet_peers"]
                .as_array()
                .unwrap()
                .len(),
            get(b.http_addr, "/api/status")["internet_peers"]
                .as_array()
                .unwrap()
                .len(),
        ];
        (links == [1, 2, 2, 1]).then_some(())
    });
    send(
        a.http_addr,
        json!({"to": "SO5KM-1", "subject": "Multi-hop", "text": "A-R1-R2-B"}),
    );
    let sent = delivered(a.http_addr, 1, "four-node end-to-end receipt");
    assert_eq!(sent["state"], "Delivered");
    assert_eq!(inbox(b.http_addr)[0]["text"], "A-R1-R2-B");

    a.stop().unwrap();
    r1.stop().unwrap();
    r2.stop().unwrap();
    b.stop().unwrap();
    for db in [&r1_db, &r2_db] {
        let records = Store::open(&db.0).unwrap().list(Direction::Relay, 10).unwrap();
        let original = records
            .iter()
            .find(|record| record.final_destination() == bob.call)
            .expect("relay retained one audit copy of the original");
        // Each holding is closed: the last relay's by Bob's own custody
        // receipt, the one before by Bob's end-to-end receipt passing back
        // through it. Neither takes the message on again if it is offered
        // anew.
        assert_eq!(original.state, State::Delivered);
    }
}

/// A station without a radio, as on an internet server, and one that dials it.
#[test]
fn internet_only_nodes_exchange_mail_both_ways() {
    let (alice, bob) = keys();
    let (a_db, c_db) = (Tmp::new("a3"), Tmp::new("c3"));
    let server = start(Setup {
        key: &bob,
        me: "SO5KM",
        peer: &alice,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![],
        }),
        store: &c_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    let server_addr = server.internet_addr.unwrap();
    let a = start(Setup {
        key: &alice,
        me: "SA0KAM",
        peer: &bob,
        also: &[],
        tnc: None,
        internet: Some(InternetConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: vec![(call("SO5KM"), server_addr)],
        }),
        store: &a_db,
        retry: QUICK,
        beacon_every: None,
        trust_file: None,
    });
    assert!(get(a.http_addr, "/api/status")["radio"].is_null());
    // The server's page listens for changes; it hears of the message at once.
    let mut events = TcpStream::connect(server.http_addr).unwrap();
    write!(
        events,
        "GET /api/events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    events.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    send(a.http_addr, json!({"to": "SO5KM", "text": "via the internet"}));
    let sent = delivered(a.http_addr, 1, "internet delivery");
    assert_eq!(
        (sent["verified"].as_bool(), sent["delivered_by"].as_str()),
        (Some(true), Some("internet"))
    );
    assert_eq!(inbox(server.http_addr)[0]["text"], "via the internet");
    let mut seen = String::new();
    let mut buf = [0u8; 1024];
    while !seen.contains("data: message") {
        let n = events.read(&mut buf).expect("an event within 30 s");
        assert!(n > 0, "event stream closed: {seen}");
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(seen.contains("text/event-stream"), "{seen}");
    // The server never dialled; it answers over the link SA0KAM opened.
    send(server.http_addr, json!({"to": "SA0KAM", "text": "and back"}));
    delivered(server.http_addr, 1, "delivery back");
    assert_eq!(inbox(a.http_addr)[0]["text"], "and back");
    // One conversation, both ways, newest first; mail with a subject is not chat.
    send(
        a.http_addr,
        json!({"to": "SO5KM", "subject": "Sked", "text": "40 m at 19Z?"}),
    );
    let chat = get(a.http_addr, "/api/messages?direction=all&peer=SO5KM&kind=chat");
    let lines: Vec<(&str, &str)> = chat
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["direction"].as_str().unwrap(), m["text"].as_str().unwrap()))
        .collect();
    assert_eq!(lines, vec![("in", "and back"), ("out", "via the internet")]);
    let mail = get(a.http_addr, "/api/messages?direction=all&kind=mail");
    assert_eq!(mail.as_array().unwrap().len(), 1);
    assert!(get(a.http_addr, "/api/messages?direction=all&peer=SP5AAA")
        .as_array()
        .unwrap()
        .is_empty());
    a.stop().unwrap();
    server.stop().unwrap();
}
