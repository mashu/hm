#!/usr/bin/env python3
"""Independent check of the SPEC.md test vectors.

Re-implements base-40 packing and verifies ids and signatures with the
reference BLAKE3 and libsodium (PyNaCl) implementations, sharing no code with
the Rust crates.

    pip install blake3 pynacl cbor2
    cargo run -q -p hm-bundle --example vectors | python3 tools/check_vectors.py
"""
import re
import sys

import blake3
import cbor2
import nacl.signing

ALPHABET = {c: i + 1 for i, c in enumerate("ABCDEFGHIJKLMNOPQRSTUVWXYZ")}
ALPHABET.update({c: 27 + i for i, c in enumerate("0123456789")})
ALPHABET.update({"-": 37, "/": 38, ".": 39})
SYMBOL = {v: k for k, v in ALPHABET.items()}

BUNDLE_CTX, BUNDLE_SIG = "hm-net 2026-09 bundle id v0", b"hm/bundle-sig/v0"
BINDING_CTX, BINDING_SIG = "hm-net 2026-09 binding id v0", b"hm/binding-sig/v0"
ATTEST = b"hm/attest/v0"
XFER_CTX = "hm-net 2026-09 xfer v0"


def pack(call: str) -> bytes:
    v = 0
    for ch in reversed(call):
        v = v * 40 + ALPHABET[ch]
    return v.to_bytes(6, "big")


def unpack(b: bytes) -> str:
    v, s = int.from_bytes(b, "big"), ""
    while v:
        s += SYMBOL[v % 40]
        v //= 40
    return s


def section(text: str, title: str) -> str:
    return text.split(title, 1)[1].split("\n## ", 1)[0]


def wire_and_id(text: str, title: str):
    s = section(text, title)
    wire = bytes.fromhex(re.search(r"wire ([0-9a-f]+)", s).group(1))
    oid = re.search(r"id\s+([0-9a-f]{64})", s).group(1)
    return wire, oid


def check(text: str) -> None:
    for call in ["SA0KAM", "SO5KM-7", "Q0CLUB"]:
        assert pack(call).hex() in section(text, "## Callsigns"), call
    print("ok  callsigns")

    frame = bytes.fromhex(section(text, "## Frame header").split("\n")[1])
    assert frame[0] == 0x00
    assert unpack(frame[1:7]) == "SA0KAM" and unpack(frame[7:13]) == "SO5KM-1"
    assert frame[13:15] == b"\xbe\xef" and frame[15:18] == b"\x01\x23\x45" and frame[18:] == b"hello"
    print("ok  frame header")

    ack = bytes.fromhex(section(text, "## ACK").split("\n")[1])
    assert ack == bytes([0, 3, 0xFC, 2, 0x05, 0xDC, 1, 1, 2, 3, 4, 5, 6, 7, 8])
    print("ok  ack")

    me = nacl.signing.SigningKey(bytes([11] * 32)).verify_key
    club = nacl.signing.SigningKey(bytes([21] * 32)).verify_key
    wire, oid = wire_and_id(text, "## Binding record")
    raw, sig = cbor2.loads(wire)
    rec = cbor2.loads(raw)
    digest = blake3.blake3(raw, derive_key_context=BINDING_CTX).digest()
    assert digest.hex() == oid
    me.verify(BINDING_SIG + digest, sig)
    assert rec[0] == 0 and unpack(rec[1]) == "SA0KAM" and rec[2] == me.encode()
    by, by_key, asig = rec[6][0]
    assert unpack(by) == "Q0CLUB" and by_key == club.encode()
    club.verify(ATTEST + rec[1] + rec[2], asig)
    print("ok  binding record (id, self-signature, attestation)")

    # AX.25 UI and KISS, encoded here from the AX.25 and KISS definitions.
    s_ax = section(text, "## AX.25 UI and KISS")
    ax = bytes.fromhex(re.search(r"ax25 ([0-9a-f]+)", s_ax).group(1))
    kiss = bytes.fromhex(re.search(r"kiss ([0-9a-f]+)", s_ax).group(1))

    def ax25_addr(call, ssid, command, last):
        return bytes(c << 1 for c in call.ljust(6).encode()) + bytes(
            [(command << 7) | 0x60 | (ssid << 1) | last]
        )

    expect_ax = ax25_addr("HMNET", 0, 1, 0) + ax25_addr("SA0KAM", 0, 0, 1) + b"\x03\xf0" + frame
    assert ax == expect_ax, "ax25 wrapper"
    esc = ax.replace(b"\xdb", b"\xdb\xdd").replace(b"\xc0", b"\xdb\xdc")
    assert kiss == b"\xc0\x00" + esc + b"\xc0", "kiss framing"
    # Compact form: to the destination's own address (SO5KM-1), with a
    # 6-byte header (version 1 | type, session, index) instead of 18 bytes.
    compact = bytes.fromhex(re.search(r"compact ([0-9a-f]+)", s_ax).group(1))
    expect_compact = (
        ax25_addr("SO5KM", 1, 1, 0)
        + ax25_addr("SA0KAM", 0, 0, 1)
        + b"\x03\xf0"
        + bytes([0x10 | (frame[0] & 0x0F)])
        + frame[13:18]
        + frame[18:]
    )
    assert compact == expect_compact, "compact AX.25 form"
    assert len(ax) - len(compact) == 12
    print("ok  AX.25 UI wrapper, compact form and KISS framing")

    sender = nacl.signing.SigningKey(bytes([7] * 32)).verify_key
    for title in ["## Chat bundle", "## Mail bundle"]:
        wire, oid = wire_and_id(text, title)
        raw, sig = cbor2.loads(wire)
        b = cbor2.loads(raw)
        digest = blake3.blake3(raw, derive_key_context=BUNDLE_CTX).digest()
        assert digest.hex() == oid
        sender.verify(BUNDLE_SIG + digest, sig)
        assert b[0] == 0 and unpack(b[1]) == "SA0KAM"
        print(f"ok  {title[3:].lower()} (id, signature)")

    # Transfer: OFFER fields, DATA preamble, and the RaptorQ systematic property
    # (source symbol 0 is the start of the object, zero-padded to the symbol
    # size). The object fits one symbol, so the sender sizes the symbol to the
    # object rounded up to 8 bytes, not to its 200-byte maximum.
    chat_wire, _ = wire_and_id(text, "## Chat bundle")
    s_x = section(text, "## Transfer of the chat bundle")
    xid = re.search(r"object_id ([0-9a-f]{64})", s_x).group(1)
    assert blake3.blake3(chat_wire, derive_key_context=XFER_CTX).hexdigest() == xid, "xfer object id"
    offer = bytes.fromhex(re.search(r"offer ([0-9a-f]+)", s_x).group(1))
    datas = [bytes.fromhex(h) for h in re.findall(r"data  ([0-9a-f]+)", s_x)]
    assert offer[0] == 0x03 and unpack(offer[1:7]) == "SA0KAM" and unpack(offer[7:13]) == "SO5KM-1"
    body = offer[18:]
    assert body[0] == 0x01 and body[1:33].hex() == xid
    assert int.from_bytes(body[33:36], "big") == len(chat_wire)
    t = int.from_bytes(body[36:38], "big")
    assert t == -(-len(chat_wire) // 8) * 8, f"symbol size {t} fits the {len(chat_wire)}-byte object"
    assert body[38] == 0 and body[39] == len(datas)
    for n, d in enumerate(datas):
        assert d[0] == 0x00 and d[13:15] == offer[13:15], "DATA type and session"
        esi = int.from_bytes(d[15:18], "big")
        assert int.from_bytes(d[18:21], "big") == len(chat_wire) and d[21] == len(datas) - 1 - n
        if esi == 0:
            assert d[22:] == chat_wire.ljust(t, b"\x00"), "systematic source symbol"
    print(f"ok  transfer OFFER and {len(datas)} DATA frame(s), systematic symbol")

    # The first over to a new peer opens the session: OPEN before the OFFER,
    # with the sender's features and limits (256 KiB, symbols up to 65528, 2 at once).
    def check_open(frame, src, dst, reply):
        assert frame[0] == 0x03 and unpack(frame[1:7]) == src and unpack(frame[7:13]) == dst
        assert frame[13:15] == offer[13:15] and frame[15:18] == bytes(3), "session of the transfer, index 0"
        body = frame[18:]
        assert len(body) == 12 and body[0] == 0x02 and body[1] == (1 if reply else 0)
        assert int.from_bytes(body[2:6], "big") == 0, "no features"
        assert int.from_bytes(body[6:9], "big") == 256 * 1024
        assert int.from_bytes(body[9:11], "big") == 65528 and body[11] == 2

    lines = [l.split() for l in s_x.splitlines() if l[:5] in ("open ", "offer", "data ")]
    assert [l[0] for l in lines[:2]] == ["open", "offer"], "OPEN comes first"
    check_open(bytes.fromhex(lines[0][1]), "SA0KAM", "SO5KM-1", reply=False)
    print("ok  OPEN before the first OFFER")

    # The receiver's final ACK: need 0, the id prefix, and a receipt signature
    # over "hm/xfer-receipt/v0" || receiver || sender || session || object id.
    s_r = section(text, "## Receipt ACK")
    check_open(bytes.fromhex(re.search(r"open  ([0-9a-f]+)", s_r).group(1)), "SO5KM-1", "SA0KAM", reply=True)
    print("ok  OPEN reply before the ACK")
    ack = bytes.fromhex(re.search(r"ack   ([0-9a-f]+)", s_r).group(1))
    receiver = nacl.signing.SigningKey(bytes([12] * 32)).verify_key
    assert ack[0] == 0x01 and unpack(ack[1:7]) == "SO5KM-1" and unpack(ack[7:13]) == "SA0KAM"
    assert ack[13:15] == offer[13:15], "same session"
    body = ack[18:]
    assert body[0:2] == b"\x00\x00" and body[6] == 1 and body[7:15].hex() == xid[:16]
    receipt = body[15:]
    assert len(receipt) == 64
    statement = b"hm/xfer-receipt/v0" + ack[1:7] + ack[7:13] + ack[13:15] + bytes.fromhex(xid)
    receiver.verify(statement, receipt)
    print("ok  receipt ACK (signature by the receiver)")

    # CLOSE: CTRL type 0x03, then reason 1 (busy) and 90 s to wait.
    s_c = section(text, "## CLOSE")
    c = bytes.fromhex(re.search(r"close ([0-9a-f]+)", s_c).group(1))
    assert c[0] == 0x03 and unpack(c[1:7]) == "SO5KM-1" and unpack(c[7:13]) == "SA0KAM"
    assert c[13:15] == b"\xbe\xef" and c[18:] == bytes([0x03, 1, 0, 90])
    print("ok  CLOSE (busy, retry after)")

    # Beacon: broadcast, session and index 0, flags, key, time, locator, heard list,
    # and a signature over "hm/beacon-sig/v0" || source callsign || body.
    s_b = section(text, "## Beacon")
    b = bytes.fromhex(re.search(r"beacon ([0-9a-f]+)", s_b).group(1))
    assert b[0] == 0x04 and unpack(b[1:7]) == "SA0KAM-10" and b[7:13] == b"\xff" * 6
    assert b[13:18] == bytes(5), "session and index 0"
    p = b[18:]
    assert p[0] == 0x01 and p[1:33] == me.encode(), "flags and key"
    assert int.from_bytes(p[33:37], "big") == 1_790_000_000
    assert p[37:43] == b"JO89XI" and p[43] == 1, "locator and heard count"
    assert unpack(p[44:50]) == "SO5KM-1" and p[50] == 3 and len(p) == 44 + 7 + 64
    me.verify(b"hm/beacon-sig/v0" + b[1:7] + p[:51], p[51:])
    print("ok  beacon (layout, signature)")


if __name__ == "__main__":
    check(sys.stdin.read())
    print("all vectors verified independently")
