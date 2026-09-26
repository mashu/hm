//! Two and three stations on localhost QUIC.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hm_ident::Identity;
use hm_net::{ed25519_key_in_cert, station_certificate, Accept, Net, NetConfig, NetError, Verdict};
use hm_wire::Callsign;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

fn secret(n: u8) -> [u8; 32] {
    [n; 32]
}

fn any_port() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

type Log = Arc<Mutex<Vec<(Callsign, Vec<u8>)>>>;

fn recorder(verdict: Verdict) -> (Accept, Log) {
    let log: Log = Arc::default();
    let l = log.clone();
    let accept: Accept = Arc::new(move |from, obj| {
        l.lock().unwrap().push((from, obj));
        verdict.clone()
    });
    (accept, log)
}

fn cfg(me: &str, s: u8, trust: &[(&str, u8)], dial: Vec<(Callsign, SocketAddr)>) -> NetConfig {
    NetConfig {
        me: call(me),
        secret: secret(s),
        trust: trust
            .iter()
            .map(|(c, k)| (call(c), Identity::from_secret(secret(*k)).public()))
            .collect(),
        listen: any_port(),
        dial,
    }
}

async fn wait_connected(net: &Net, peer: &str) -> bool {
    for _ in 0..100 {
        if net.is_connected(call(peer)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[test]
fn certificate_carries_the_station_key() {
    let (cert, _) = station_certificate(secret(7)).unwrap();
    assert_eq!(
        ed25519_key_in_cert(&cert).unwrap(),
        Identity::from_secret(secret(7)).public().0
    );
}

#[tokio::test]
async fn delivery_with_verified_receipt() {
    let (b_accept, b_log) = recorder(Verdict::Stored);
    let b = Net::start(cfg("SO5KM-1", 2, &[("SA0KAM", 1)], vec![]), b_accept).unwrap();
    let (a_accept, _) = recorder(Verdict::Stored);
    let b_addr = b.local_addr().unwrap();
    let a = Net::start(
        cfg("SA0KAM", 1, &[("SO5KM", 2)], vec![(call("SO5KM"), b_addr)]),
        a_accept,
    )
    .unwrap();
    assert!(wait_connected(&a, "SO5KM-1").await, "A connects to B");
    assert!(wait_connected(&b, "SA0KAM").await, "B names A from its key");

    a.deliver(call("SO5KM-1"), b"a signed bundle").await.unwrap();
    assert_eq!(
        b_log.lock().unwrap().as_slice(),
        &[(call("SA0KAM"), b"a signed bundle".to_vec())]
    );
    // The connection is symmetric: B can deliver to A on it.
    b.deliver(call("SA0KAM"), b"reply").await.unwrap();
}

#[tokio::test]
async fn rejection_and_duplicates() {
    let (b_accept, _) = recorder(Verdict::Rejected("not addressed to this station".into()));
    let b = Net::start(cfg("SO5KM", 2, &[("SA0KAM", 1)], vec![]), b_accept).unwrap();
    let (a_accept, _) = recorder(Verdict::Stored);
    let a = Net::start(
        cfg(
            "SA0KAM",
            1,
            &[("SO5KM", 2)],
            vec![(call("SO5KM"), b.local_addr().unwrap())],
        ),
        a_accept,
    )
    .unwrap();
    assert!(wait_connected(&a, "SO5KM").await);
    match a.deliver(call("SO5KM"), b"x").await {
        Err(NetError::Rejected(r)) => assert_eq!(r, "not addressed to this station"),
        other => panic!("{other:?}"),
    }

    let (c_accept, _) = recorder(Verdict::Duplicate);
    let c = Net::start(cfg("SP5AAA", 3, &[("SA0KAM", 1)], vec![]), c_accept).unwrap();
    let (a2_accept, _) = recorder(Verdict::Stored);
    let a2 = Net::start(
        cfg(
            "SA0KAM",
            1,
            &[("SP5AAA", 3)],
            vec![(call("SP5AAA"), c.local_addr().unwrap())],
        ),
        a2_accept,
    )
    .unwrap();
    assert!(wait_connected(&a2, "SP5AAA").await);
    a2.deliver(call("SP5AAA"), b"seen before")
        .await
        .expect("a duplicate is still receipted");
}

#[tokio::test]
async fn stations_outside_the_trust_file_cannot_connect() {
    let (b_accept, b_log) = recorder(Verdict::Stored);
    // B trusts only SA0KAM (key 1).
    let b = Net::start(cfg("SO5KM", 2, &[("SA0KAM", 1)], vec![]), b_accept).unwrap();
    let b_addr = b.local_addr().unwrap();
    // An impostor claims to be SA0KAM but has a different key; it trusts B.
    let (i_accept, _) = recorder(Verdict::Stored);
    let impostor = Net::start(
        cfg("SA0KAM", 9, &[("SO5KM", 2)], vec![(call("SO5KM"), b_addr)]),
        i_accept,
    )
    .unwrap();
    // The impostor's side of the TLS 1.3 handshake finishes before B has
    // checked its certificate, so watch closely: it must never count the
    // link as up, not even for the moment until B's refusal arrives.
    for _ in 0..3000 {
        assert!(!impostor.is_connected(call("SO5KM")), "handshake refused");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(b.connected().is_empty());
    assert!(matches!(
        impostor.deliver(call("SO5KM"), b"x").await,
        Err(NetError::NotConnected)
    ));
    assert!(b_log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_server_with_the_wrong_key_is_refused() {
    // A expects SO5KM to have key 2, but the server at that address has key 5.
    let (s_accept, _) = recorder(Verdict::Stored);
    let s = Net::start(cfg("SO5KM", 5, &[("SA0KAM", 1)], vec![]), s_accept).unwrap();
    let (a_accept, _) = recorder(Verdict::Stored);
    let a = Net::start(
        cfg(
            "SA0KAM",
            1,
            &[("SO5KM", 2)],
            vec![(call("SO5KM"), s.local_addr().unwrap())],
        ),
        a_accept,
    )
    .unwrap();
    assert!(!wait_connected(&a, "SO5KM").await);
}

#[tokio::test]
async fn reconnects_after_the_peer_restarts() {
    let (b_accept, _) = recorder(Verdict::Stored);
    let b = Net::start(cfg("SO5KM", 2, &[("SA0KAM", 1)], vec![]), b_accept).unwrap();
    let addr = b.local_addr().unwrap();
    let (a_accept, _) = recorder(Verdict::Stored);
    let a = Net::start(
        cfg("SA0KAM", 1, &[("SO5KM", 2)], vec![(call("SO5KM"), addr)]),
        a_accept,
    )
    .unwrap();
    assert!(wait_connected(&a, "SO5KM").await);
    b.close();
    drop(b);
    let mut down = false;
    for _ in 0..100 {
        if !a.is_connected(call("SO5KM")) {
            down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(down, "A notices the connection is gone");
    // B comes back on the same address; A redials by itself. The old B's UDP
    // socket is released only once its background tasks have wound down, which
    // under load can take a moment (a restarted process would not wait: the
    // OS frees the port when the old one exits), so retry the bind until it is free.
    let (b2_accept, b2_log) = recorder(Verdict::Stored);
    let mut tries = 0;
    let _b2 = loop {
        let mut c = cfg("SO5KM", 2, &[("SA0KAM", 1)], vec![]);
        c.listen = addr;
        match Net::start(c, b2_accept.clone()) {
            Ok(n) => break n,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < 100 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("B cannot listen on its old address again: {e}"),
        }
    };
    assert!(wait_connected(&a, "SO5KM").await);
    a.deliver(call("SO5KM"), b"after restart").await.unwrap();
    assert_eq!(b2_log.lock().unwrap().len(), 1);
}

/// Two stations of one operator, SA0KAM-1 and SA0KAM-2, with their own keys,
/// both linked to one hub: each is its own link, and each receives only what
/// is sent to it, with a receipt from its own key.
#[tokio::test]
async fn two_ssids_link_to_one_hub_at_once() {
    let (hub_accept, _) = recorder(Verdict::Stored);
    let hub = Net::start(
        cfg("SO5KM", 2, &[("SA0KAM-1", 11), ("SA0KAM-2", 12)], vec![]),
        hub_accept,
    )
    .unwrap();
    let addr = hub.local_addr().unwrap();
    let mut logs = Vec::new();
    let mut stations = Vec::new();
    for (me, secret) in [("SA0KAM-1", 11), ("SA0KAM-2", 12)] {
        let (accept, log) = recorder(Verdict::Stored);
        let n = Net::start(
            cfg(me, secret, &[("SO5KM", 2)], vec![(call("SO5KM"), addr)]),
            accept,
        )
        .unwrap();
        assert!(wait_connected(&n, "SO5KM").await, "{me} links to the hub");
        logs.push(log);
        stations.push(n);
    }
    assert!(wait_connected(&hub, "SA0KAM-1").await && wait_connected(&hub, "SA0KAM-2").await);
    let mut linked = hub.connected();
    linked.sort();
    assert_eq!(linked, vec![call("SA0KAM-1"), call("SA0KAM-2")]);

    hub.deliver(call("SA0KAM-2"), b"to two")
        .await
        .expect("receipt from SA0KAM-2's key");
    hub.deliver(call("SA0KAM-1"), b"to one")
        .await
        .expect("receipt from SA0KAM-1's key");
    assert_eq!(
        logs[0].lock().unwrap().as_slice(),
        &[(call("SO5KM"), b"to one".to_vec())]
    );
    assert_eq!(
        logs[1].lock().unwrap().as_slice(),
        &[(call("SO5KM"), b"to two".to_vec())]
    );
    // A station the hub has no link or key for is not reached through its sibling.
    assert!(matches!(
        hub.deliver(call("SA0KAM-3"), b"x").await,
        Err(NetError::NotConnected)
    ));
}
