# Wire specification — draft v0

Status: draft, Phase 1. Everything here may change until v1; the wire version
stays 0 until then. Every structure below has a test vector at the end, checked
by an independent implementation (`tools/check_vectors.py`).

## Conventions

- Integers in fixed layouts are big-endian.
- CBOR follows RFC 8949 with definite lengths. Maps use small integer keys.
- Hashes are BLAKE3. Signatures are Ed25519 (RFC 8032), verified strictly
  (small-order keys and non-canonical signatures are rejected).
- Content is never encrypted. Signatures authenticate; they do not hide.

## 1. Callsigns

A callsign is 1–9 characters from `A–Z 0–9 - / .`, packed base-40 into 48 bits.
Input is normalised to upper case.

| Digit | Character |
| --- | --- |
| 0 | none (only above the last character) |
| 1–26 | A–Z |
| 27–36 | 0–9 |
| 37, 38, 39 | `-`, `/`, `.` |

`packed = Σ digit(c[i]) · 40^i`: the first character is the least significant digit.
Valid values are 1 … 40⁹−1 with no zero digit below the most significant one, so
every value maps to exactly one string. On the wire: the low 6 bytes of the value,
big-endian. `FF FF FF FF FF FF` means broadcast in frame headers.

An SSID is written as a suffix, `SA0KAM-7`. Identity is bound to the base call
(`SA0KAM`); all SSIDs of a station share one key.

## 2. Radio frame header (18 bytes)

| Bytes | Field |
| --- | --- |
| 0 | version (high nibble, currently 0) and frame type (low nibble) |
| 1–6 | source callsign |
| 7–12 | destination callsign, or broadcast |
| 13–14 | session id |
| 15–17 | index: symbol or sequence number (24 bits) |

The payload follows the header. The modem supplies synchronisation and inner FEC
(IL2P Reed–Solomon or codec2 LDPC), so the header carries no checksum. The source
callsign is always in clear.

| Type | Name | Payload |
| --- | --- | --- |
| 0 | DATA | preamble and one RaptorQ symbol (section 7) |
| 1 | ACK | section 3 |
| 2 | SYNC | set reconciliation (defined in Phase 2) |
| 3 | CTRL | first byte is the message type: 0x01 OFFER (section 7); others reserved |
| 4 | BEACON | presence and identification (defined later in Phase 1) |

Receivers drop frames with an unknown version or type.

## 3. ACK payload (7 + 8n bytes, plus 64 with a receipt)

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 2 | symbols still needed for the current object; 0 = done, 0xFFFF = send the OFFER again |
| 2 | 1 | received SNR in dB, signed; −128 = unknown |
| 3 | 1 | suggested modem mode for the peer; 255 = none |
| 4 | 2 | airtime credit for the peer until the next ACK, ms |
| 6 | 1 | n, number of completed-object prefixes (≤ 16) |
| 7 | 8n | first 8 bytes of each completed object id |
| 7 + 8n | 64 | optional receipt: the receiver's Ed25519 signature (section 7); present exactly when the payload is 64 bytes longer |

## 4. Signed objects (envelopes)

Every signed object travels as CBOR `array(2) [bstr raw, bstr(64) sig]`.

- Object id = `BLAKE3.derive_key(context, raw)`.
- Signature = `Ed25519(sig_prefix || id)`.

| Domain | derive_key context | sig_prefix |
| --- | --- | --- |
| Bundle | `hm-net 2026-09 bundle id v0` | `hm/bundle-sig/v0` |
| Binding record | `hm-net 2026-09 binding id v0` | `hm/binding-sig/v0` |

Rules:

- Ids and signatures cover `raw` exactly as transmitted. Nodes store and forward
  envelope bytes untouched and never re-encode signed content.
- Envelopes larger than 64 KiB are rejected. Attachments are separate objects.
- Bytes after the envelope are an error.

## 5. Binding records

"This key speaks for this callsign", signed by the key itself (domain: binding).
CBOR map:

| Key | Field | Type |
| --- | --- | --- |
| 0 | v | uint, 0 |
| 1 | callsign | bstr(6), base call without SSID |
| 2 | key | bstr(32), Ed25519 public key |
| 3 | seq | uint; a higher value replaces older records |
| 4 | created | uint, Unix seconds |
| 5 | homes | optional array of callsigns: mailbox nodes holding this station's mail |
| 6 | attestations | optional array of attestations |

Attestation: `array(3) [by callsign, by_key bstr(32), sig bstr(64)]`, where `sig`
is Ed25519 over `"hm/attest/v0" || callsign (6 bytes) || key (32 bytes)`.
Each attestation is checked on its own; an invalid one does not invalidate the record.
Empty optional arrays must be omitted.

## 6. Bundles

One message, sealed in an envelope (domain: bundle). CBOR map:

| Key | Field | Type |
| --- | --- | --- |
| 0 | v | uint, 0 |
| 1 | from | callsign |
| 2 | to | array of 1–32 addresses |
| 3 | kind | uint: 0 mail, 1 chat, 2 form, 3 bulletin, 4 position, 5 receipt |
| 4 | prec | optional uint: 1 priority, 2 immediate, 3 flash; absent = routine |
| 5 | created | uint, Unix seconds |
| 6 | ttl | uint > 0, seconds after `created` when relays may drop it |
| 7 | subject | optional text, 1–128 bytes |
| 8 | body | optional `array(2) [codec uint, bstr data]`; codec 0 = UTF-8 text |
| 9 | parts | optional array of part references |
| 10 | reply_to | optional bstr(32), id of another bundle |

Address: `array(2) [tag, value]`: 0 station (callsign), 1 group (text ≤ 32 bytes),
2 tactical (text ≤ 32 bytes), 3 email (text ≤ 254 bytes, must contain `@`).

Part reference (attachment, pulled on demand): map `0 hash bstr(32)`, `1 size uint`,
`2 mime text`, `3 name text?`, `4 thumb bstr?`.

A receipt (kind 5) must set `reply_to` to the bundle it confirms. Empty optional
arrays and `prec = 0` must be omitted.

Receiving is two-step: decode for routing (recipients, precedence, expiry), then
verify with the sender's key from their binding record before showing,
acknowledging or delivering anything.

## 7. Transfers

A transfer moves one object (for example a bundle envelope) from one station
to another over a half-duplex link.

**Object id**: `BLAKE3.derive_key("hm-net 2026-09 xfer v0", object)`. This is a
transport checksum, distinct from bundle ids.

**Coding**: RaptorQ (RFC 6330) with one source block (Z = 1), one sub-block
(N = 1) and alignment Al = 8. The symbol size T is a multiple of 8, and
K = ceil(len / T) ≤ 8192. The object is zero-padded to K·T bytes. ESIs
0…K−1 are the source symbols (the padded object in T-byte pieces); ESIs from K
on are repair symbols. Any K distinct symbols decode the object with high
probability.

**Frames**: the header's `session` names the transfer (chosen by the sender) and,
in DATA frames, `index` is the ESI.

DATA payload:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 3 | object length |
| 3 | 1 | DATA frames still to come in this over after this one |
| 4 | T | symbol |

OFFER (CTRL payload, 40 bytes):

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | 0x01 |
| 1 | 32 | object id |
| 33 | 3 | object length |
| 36 | 2 | symbol size T |
| 38 | 1 | precedence (0 routine … 3 flash) |
| 39 | 1 | DATA frames following in this over |

**An over** is an optional OFFER followed by n DATA frames sent back to back in
one key-up. The first over of a transfer starts with an OFFER.

**Receiver rules**:

- Collect distinct ESIs for `(source callsign, session)`. The symbol size is the DATA payload length minus 4.
- Reject objects longer than the local limit, symbol sizes that are not multiples of 8, and K > 8192.
- A decoded object is accepted only if it hashes to the OFFER's id. Otherwise every
  collected symbol is discarded (one was corrupt) and collection restarts.
- Predict the end of the sender's over from the latest frame: its arrival time +
  `remaining` (believed up to 64) × one DATA frame's airtime + a guard time.
- When hearing several overs, answer once the last one ends. Then send one ACK per
  transfer, addressed to its sender with the same session:
  - need 0, the id prefix and a receipt once verified. The receipt is the
    receiver's Ed25519 signature, by the key bound to its callsign, over
    `"hm/xfer-receipt/v0" || receiver callsign (6) || sender callsign (6) || session (2) || object id (32)`,
    with the callsigns exactly as in the frame headers;
  - 0xFFFF if no OFFER has been seen;
  - otherwise K minus the symbols held (1 if K are held but decoding has not succeeded).
- Remember completed objects for a while. A repeated OFFER is answered "done",
  with the receipt, without delivering the object again.

**Sender rule**: a completion ACK proves delivery only through its receipt. If the
sender knows the receiver's key, an ACK whose receipt is missing or does not
verify is ignored as forged. Without the key, the delivery is reported as
unverified. The channel is broadcast and cleartext, so hearing the object proves
nothing about who received it.

**Sender behaviour** (recommended; peers do not depend on it):

- Size each over as the smallest n for which P[at least `need` of n frames arrive] ≥ 0.9, at the estimated loss rate.
- Update the loss estimate from each ACK.
- With no ACK in time, wait a random backoff drawn uniformly from
  [0, (last over's airtime + guard) · 2^min(misses, 5)]. Then send an OFFER and
  at most two symbols as a probe.
- Keep a long-run airtime budget (duty cycle with a burst allowance), and never
  send an over larger than the allowance.

**Receiver resources** (recommended): cap concurrent incoming transfers overall
and per sender (8 and 2 by default). When full, evict the least valuable
transfer: not finished before finished, no OFFER before OFFER seen, then fewest
symbols, then least recently heard.

## 8. KISS and AX.25 encapsulation

KISS TNCs (Direwolf, NinoTNC, radios with a built-in TNC) carry AX.25 frames.
On that path every hm frame is the information field of an AX.25 UI frame:

| Field | Value |
| --- | --- |
| destination | `HMNET`, SSID 0, command bit set |
| source | the station callsign (1–6 letters or digits, SSID 0–15), last-address bit set |
| control | 0x03 (UI) |
| PID | 0xF0 (no layer 3) |
| information | the hm frame |

The AX.25 source is the station identification on this path. Callsigns that
AX.25 cannot express cannot use a KISS TNC. Receivers accept frames with a
digipeater path and with the poll bit set, and ignore frames with another
destination or PID.

KISS framing is standard: FEND 0xC0, FESC 0xDB, TFEND 0xDC, TFESC 0xDD. The type
byte holds the TNC port in its high nibble and command 0 (data).

## 9. Internet links

Stations may also link over the internet. A link is a QUIC connection (RFC 9000)
with TLS 1.3, ALPN `hm-net/0`, and mutual authentication by station key:

- Each station presents a self-signed X.509 certificate whose subject public key
  is its Ed25519 station key. Only Ed25519 handshake signatures are used.
- Each side accepts the other only if that key is one it trusts, and names the
  peer by the callsign bound to the key. Certificate names, issuers and validity
  periods carry no meaning.
- Either side may open streams on a link, whichever side dialled.

One bundle travels on one bidirectional stream:

| Direction | Content |
| --- | --- |
| sender to receiver | `"HMD0"`, object length (u32, ≤ 1 MiB), object |
| receiver to sender | `0x00` and a 64-byte receipt if the object is stored or already held; or `0x01`, reason length (u16), UTF-8 reason |

The receipt is the transfer receipt of section 7 with base callsigns and
session 0: the receiver's signature over
`"hm/xfer-receipt/v0" || receiver base call || sender base call || 0x0000 || transfer id`.
A receiver accepts only bundles addressed to its callsign whose signatures do
not fail against its trust file. Delivery therefore means the same on every
link: the receiver signed for exactly these bytes.

## 10. Forward compatibility

- Unknown map keys are ignored when decoding.
- Unknown `kind`, `prec` and `codec` values decode as "other" and are kept.
- Unknown address tags are kept with their raw CBOR value and re-encoded verbatim.
- Because ids and signatures cover raw bytes, older nodes route and verify bundles
  written by newer software.

## 11. Test vectors

Generated with `cargo run -p hm-bundle --example vectors`, verified by
`tools/check_vectors.py` (reference BLAKE3, libsodium, cbor2).

```text
## Callsigns
SA0KAM   packed       1334507259  bytes 00004f8af6fb
SO5KM-7  packed     143086835819  bytes 002150a3d86b
Q0CLUB   packed        259333897  bytes 00000f751f09

## Frame header (DATA, SA0KAM -> SO5KM-1, session 0xBEEF, index 0x012345, payload "hello")
0000004f8af6fb001b97cbd86bbeef01234568656c6c6f

## ACK (need 3, snr -4 dB, mode 2, credit 1500 ms, one completed prefix)
0003fc0205dc010102030405060708

## Binding record (secret 0x0b x 32, attested by Q0CLUB with secret 0x15 x 32)
public key   66be7e332c7a453332bd9d0a7f7db055f5c5ef1a06ada66d98b39fb6810c473a
attester key d54207da194977dcf46adbfec2bc2e75b52d5a8a42184fedfdc00024f0e3e8da
id   680473175f932686f86e641e74a00465ffbbfe789dc785cc3e9752fff2d049ff
wire 8258b4a70000014600004f8af6fb02582066be7e332c7a453332bd9d0a7f7db055f5c5ef1a06ada66d98b39fb6810c473a0301041a6ab13b80058246a53e713ef6fb4604218fabd86b0681834600000f751f095820d54207da194977dcf46adbfec2bc2e75b52d5a8a42184fedfdc00024f0e3e8da58403cc67c68fd346e834ac8a415ff02fb94513ce00ee6fa8af84ed18a910392f7f7cfedcb716ad2a25f197a744d16bcd4853eefaaa9e4caf6a956fbed0041f5240d5840a5d6ac73e989b05d40fe8544925f5183d4638f9f2f40ea43375fd13bbdd17fff96d3f9573589cda3ae23c47bcce42a775bc0f120871ff9212faea926c0560c0d

## Chat bundle (secret 0x07 x 32)
public key ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c
id   d938aeaf37a378615c56aca3b385fcbc6e3d0eb724f6cce8901f260b3e2626f1
wire 825832a70000014600004f8af6fb028182004600000207586b0301051a6ab13b8006190e100882004c3733206465205341304b414d5840ce3c7adc856375a2ce7cbb47011edcbbfa93ff43bf4346232367268be9e35cf86d01b31ab2408b6ba8954f5088b430c211eca59261c6d9805cd446f9773aa602
size 119 bytes

## Mail bundle (same key, priority, two recipients, subject)
id   5626a11146e756ac138672b8f9399e34c7436b3a5036076cd22df51eb7762983
wire 82585ea90000014600004f8af6fb028282004600000207586b82036f71736c406578616d706c652e6f726703000401051a6ab13be4061a00093a800764536b6564088200581b34306d20372e303437204d487a2061742031393a3030205554433f58405c2b9d97f7d51ba0e1d3123eb88e3de075aca4e8b8d9d3b4d86e433e9e1c4f7d6115d6ec0bc22adeef38ec991e60ae65bcb8ec56b0f9aa49a920a9563b47cc04

## AX.25 UI and KISS (the frame header vector above, from SA0KAM)
ax25 909a9c8aa840e0a6826096829a6103f00000004f8af6fb001b97cbd86bbeef01234568656c6c6f
kiss c000909a9c8aa840e0a6826096829a6103f00000004f8af6fb001b97cbd86bbeef01234568656c6c6fc0

## Transfer of the chat bundle (SA0KAM -> SO5KM-1, symbol size 200, first over)
object_id 26a90b587084a213a812a105b23553637c1afd5c125fc66381203f4583e09807
offer 0300004f8af6fb001b97cbd86bf2b40000000126a90b587084a213a812a105b23553637c1afd5c125fc66381203f4583e0980700007700c80001
data  0000004f8af6fb001b97cbd86bf2b400000000007700825832a70000014600004f8af6fb028182004600000207586b0301051a6ab13b8006190e100882004c3733206465205341304b414d5840ce3c7adc856375a2ce7cbb47011edcbbfa93ff43bf4346232367268be9e35cf86d01b31ab2408b6ba8954f5088b430c211eca59261c6d9805cd446f9773aa602000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000

## Receipt ACK from SO5KM-1 (secret 0x0c x 32) for that transfer
public key 0b513ad9b4924015ca0902ed079044d3ac5dbec2306f06948c10da8eb6e39f2d
ack   01001b97cbd86b00004f8af6fbf2b4000000000080ff00000126a90b587084a21311b09926955fc1657ddf7c42bcd35089e157a8f522ae8b7fdef4005e57dfc38369d6f4e7ee8c50ba3f1b1fb1659e1139f6b4bc0d8783a6ea8ed55498b5025204
```

## 12. Not yet specified

CTRL session open/close with feature bits and BEACON payloads (later in Phase 1);
SYNC reconciliation messages and the node directory (Phase 2); body compression
dictionaries (codec 1, reserved).
