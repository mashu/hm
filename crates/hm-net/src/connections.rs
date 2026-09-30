//! The tasks behind a [`Net`]: accepting and dialling connections, and
//! serving each one's streams.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hm_wire::stream::{request_header, StreamMessage, CONTROL_TAKEN};
use hm_wire::Callsign;
use hm_xfer::{object_id, receipt_statement};
use quinn::Connection;

use crate::{Net, Verdict, ACCEPTED, CONFIRM_TIMEOUT, LIMITS, MAX_HUB_LINKS, READ_TIMEOUT, REDIAL_EVERY};

pub(crate) async fn accept_loop(net: Arc<Net>) {
    while let Some(incoming) = net.endpoint.accept().await {
        if net.open && net.endpoint.open_connections() >= MAX_HUB_LINKS {
            incoming.refuse();
            continue;
        }
        let net = net.clone();
        tokio::spawn(async move {
            if let Ok(conn) = incoming.await {
                // The handshake is done on our side, so the dialer's key is
                // one we trust: tell it so before it counts the link as up.
                if net.peer_of(&conn).is_some() && confirm(&conn).await.is_err() {
                    return;
                }
                net.register(conn);
            }
        });
    }
}

/// Keeps the dial list's links up. Holds the station only while dialling, so
/// a station that is dropped releases its socket, and the loop ends.
pub(crate) async fn dial_loop(net: std::sync::Weak<Net>) {
    loop {
        let Some(net) = net.upgrade() else { return };
        let peers = net.dial.lock().expect("lock").clone();
        for (call, address) in &peers {
            if net.is_connected(*call) {
                continue;
            }
            let Ok(Ok(addrs)) =
                tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host(address)).await
            else {
                continue;
            };
            // A name can resolve to IPv6 and IPv4 addresses (localhost to ::1
            // and 127.0.0.1): try each, those our socket can reach first.
            let mut addrs: Vec<SocketAddr> = addrs.collect();
            let v4 = net.endpoint.local_addr().is_ok_and(|a| a.is_ipv4());
            addrs.sort_by_key(|a| a.is_ipv4() != v4);
            for addr in addrs {
                let Ok(connecting) = net.endpoint.connect(addr, "hm-net") else {
                    continue;
                };
                let Ok(Ok(conn)) = tokio::time::timeout(Duration::from_secs(5), connecting).await else {
                    continue;
                };
                let named = net.peer_of(&conn);
                if named != Some(*call) && named != Some(call.base()) {
                    conn.close(1u32.into(), b"unexpected station");
                } else if confirmed(&conn).await {
                    net.register(conn);
                } else {
                    conn.close(1u32.into(), b"not accepted");
                }
                break;
            }
        }
        drop(net);
        tokio::time::sleep(REDIAL_EVERY).await;
    }
}

/// Listener side: confirm to the dialer that it was accepted.
pub(crate) async fn confirm(conn: &Connection) -> Result<(), String> {
    let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
    s.write_all(ACCEPTED).await.map_err(|e| e.to_string())?;
    s.finish().map_err(|e| e.to_string())
}

/// Dialer side: whether the listener confirmed it accepted us. A listener that
/// refuses our key closes the connection instead.
pub(crate) async fn confirmed(conn: &Connection) -> bool {
    let wait = async {
        let mut s = conn.accept_uni().await.ok()?;
        s.read_to_end(ACCEPTED.len()).await.ok()
    };
    matches!(
        tokio::time::timeout(CONFIRM_TIMEOUT, wait).await,
        Ok(Some(msg)) if msg == ACCEPTED
    )
}

pub(crate) async fn serve_connection(net: Arc<Net>, peer: Callsign, conn: Connection) {
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let net = net.clone();
        tokio::spawn(async move {
            let message = tokio::time::timeout(READ_TIMEOUT, read_message(&mut recv))
                .await
                .unwrap_or_else(|_| Err("message not sent in time".into()));
            let reply = match message {
                Ok(Incoming::Object(object)) => {
                    let id = object_id(&object);
                    let accept = net.accept.clone();
                    let verdict = tokio::task::spawn_blocking(move || accept(peer, object))
                        .await
                        .unwrap_or_else(|_| Verdict::Rejected("internal error".into()));
                    match verdict {
                        Verdict::Stored | Verdict::Duplicate => {
                            let sig = net.identity.sign(&receipt_statement(net.me, peer.base(), 0, &id));
                            StreamMessage::Stored(sig).encode()
                        }
                        Verdict::Busy { retry_after, reason } => StreamMessage::Busy {
                            retry_after,
                            reason: reason.as_bytes(),
                        }
                        .encode(),
                        Verdict::Rejected(reason) => StreamMessage::Rejected(reason.as_bytes()).encode(),
                    }
                }
                Ok(Incoming::Control(payload)) => {
                    (net.control)(peer, payload);
                    CONTROL_TAKEN.to_vec()
                }
                Err(reason) => StreamMessage::Rejected(reason.as_bytes()).encode(),
            };
            let _ = send.write_all(&reply).await;
            let _ = send.finish();
        });
    }
    let mut conns = net.conns.lock().expect("lock");
    if conns
        .get(&peer)
        .is_some_and(|c| c.stable_id() == conn.stable_id())
    {
        conns.remove(&peer);
    }
}

pub(crate) enum Incoming {
    Object(Vec<u8>),
    Control(Vec<u8>),
}

pub(crate) async fn read_message(recv: &mut quinn::RecvStream) -> Result<Incoming, String> {
    let mut head = [0u8; 8];
    recv.read_exact(&mut head).await.map_err(|e| e.to_string())?;
    let (control, len) = request_header(&head, LIMITS)
        .map_err(|error| error.to_string())?
        .expect("eight bytes decide the header");
    // The sender finishes the stream after the message. Take the bytes as
    // they come rather than setting aside what the header claims up front.
    let payload = recv.read_to_end(len).await.map_err(|e| e.to_string())?;
    if payload.len() != len {
        return Err(format!("message of {} bytes, header says {len}", payload.len()));
    }
    Ok(if control {
        Incoming::Control(payload)
    } else {
        Incoming::Object(payload)
    })
}
