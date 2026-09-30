use super::*;

#[test]
fn a_new_connection_opens_with_a_bundle() {
    let message = bundle_message(b"object");
    // Whole, or only its first bytes so far.
    for n in 1..=message.len() {
        assert!(opens_connection(&message[..n]), "{n} bytes");
    }
    // Answers and stray bytes only follow a bundle on a live connection.
    assert!(!opens_connection(&[]));
    assert!(!opens_connection(&[0; 65]));
    assert!(!opens_connection(&rejection("no")));
    assert!(!opens_connection(b"HMX0"));
}

#[test]
fn inbox_splits_bundles_and_answers() {
    let mut inbox = Inbox::default();
    inbox.buf.extend_from_slice(&bundle_message(b"one"));
    inbox.buf.push(0);
    inbox.buf.extend_from_slice(&[7; 64]);
    inbox.buf.extend_from_slice(&rejection("busy"));
    assert!(matches!(inbox.next(), Some(Message::Bundle(b)) if b == b"one"));
    assert!(matches!(inbox.next(), Some(Message::Receipt(r)) if r == [7; 64]));
    assert!(matches!(inbox.next(), Some(Message::Rejected(r)) if r == "busy"));
    inbox
        .buf
        .extend_from_slice(&rejection("busy for 90 s: holdings full"));
    assert!(matches!(
        inbox.next(),
        Some(Message::Busy { retry_after: 90, reason }) if reason == "holdings full"
    ));
    assert!(inbox.next().is_none());
}
