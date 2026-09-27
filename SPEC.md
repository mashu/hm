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

An SSID is written as a suffix, `SA0KAM-7`. Every SSID is a station of its own
and may have its own key, so one operator can run several stations (SA0KAM-1 and
SA0KAM-2). A key bound to a callsign without an SSID speaks for every SSID of it
that has no key of its own. Mail addressed to SA0KAM-2 is for that station only.

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
| 3 | CTRL | first byte is the message type: 0x01 OFFER, 0x02 OPEN, 0x03 CLOSE (section 7); others reserved |
| 4 | BEACON | presence and identification (section 8) |

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
| 1 | callsign | bstr(6); with an SSID, that station only; without, every SSID lacking a record of its own |
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
| 8 | body | optional `array(2) [codec uint, bstr data]`; codecs below |
| 9 | parts | optional array of part references |
| 10 | reply_to | optional bstr(32), id of another bundle |
| 11 | max_hops | optional uint 1–16; absent = 8 |

Body codecs:

- 0: UTF-8 text.
- 1: one Zstandard frame containing UTF-8 text, using the hm-net v0 dictionary.
  The dictionary's BLAKE3 hash is published with the test vectors. A sender uses
  codec 1 only when it is smaller than codec 0. Relays keep unknown codecs
  byte-for-byte.

The pinned `hm-net v0` dictionary has BLAKE3
`4b09accfcc88e3a776ce40e97a841debebf4fd4b1d7b574089fbc18d512905de`.
For the UTF-8 text `Net control calling all stations. Check in with callsign,
location, and traffic. Net control calling all stations. Please acknowledge
receipt.`, codec 1 produces
`28b52ffd208e750000082003000825042a9ceb60740501` (142 bytes before and
23 bytes after compression). Decoders MUST reject decompressed bodies larger
than 1 MiB.

Address: `array(2) [tag, value]`: 0 station (callsign), 1 group (text ≤ 32 bytes),
2 tactical (text ≤ 32 bytes), 3 email (text ≤ 254 bytes, must contain `@`).

Part reference (attachment, pulled on demand): map `0 hash bstr(32)`, `1 size uint`,
`2 mime text`, `3 name text?`, `4 thumb bstr?`.

A receipt (kind 5) must set `reply_to` to the bundle it confirms, name the
original sender as its only recipient, and carry no subject, body or parts.
Empty optional arrays, `prec = 0`, and `max_hops = 8` must be omitted.

Receiving is two-step: decode for routing (recipients, precedence, expiry), then
verify with the sender's key from the local trust list (or a binding anchored
there) before acknowledging. An unverified bundle addressed to this station may
be stored and shown as unverified, but MUST NOT produce an automatic receipt.

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
- Count an ACK as late only after the time the over, a guard, the peer's key-up
  and ACK, and another guard would take. Predict airtime with the link's
  per-frame overhead (19 bytes for AX.25 UI) and an allowance for bit stuffing.
- The link may hold an over back while the channel is busy. While waiting for
  an ACK, on hearing any frame other than the peer's to us, wait at least until
  that traffic could have ended (for DATA, the frames it says remain), followed
  by our whole over and the peer's answer.
- With no ACK in time, wait a random backoff drawn uniformly from
  [0, (last over's airtime + guard) · 2^min(misses, 5)]. Then send an OFFER and
  at most two symbols as a probe.
- Keep a long-run airtime budget (duty cycle with a burst allowance), and never
  send an over larger than the allowance.

**Receiver resources** (recommended): cap concurrent incoming transfers overall
and per sender (8 and 2 by default). When full, evict the least valuable
transfer: not finished before finished, no OFFER before OFFER seen, then fewest
symbols, then least recently heard. When every slot holds an offered, unfinished
transfer from other senders, answer a new sender's OFFER with CLOSE busy instead.

### Sessions

Two stations tell each other what they offer and accept with OPEN, and a
receiver that cannot take a transfer says so with CLOSE. Neither costs a
turnaround: they travel in overs and answers that are sent anyway.

OPEN (CTRL payload, 12 bytes):

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | 0x02 |
| 1 | 1 | flags: 0x01 reply (answers the peer's OPEN); other bits 0 |
| 2 | 4 | feature bits: 0x01 mailbox (holds mail for others), 0x02 relay (passes bundles on, Phase 2), 0x04 IL2P (decodes IL2P framing on this link); unknown bits are ignored |
| 6 | 3 | largest object accepted |
| 9 | 2 | largest symbol size accepted (a multiple of 8) |
| 11 | 1 | transfers accepted at once from this peer |

CLOSE (CTRL payload, 4 bytes):

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | 0x03 |
| 1 | 1 | reason: 0 done (the sender has nothing more; the receiver may forget its finished transfers), 1 busy, 2 refused, 3 too large; unknown reasons count as refused |
| 2 | 2 | seconds before trying again (busy); 0 otherwise |

The frame header's session names the transfer the message belongs to; a CLOSE
with session 0 applies to every transfer between the two stations.

Rules:

- A sender that has no OPEN from the peer from the last half hour puts its own
  OPEN (flags 0) first in the first over of a transfer, before the OFFER, and
  again in its probes until the peer answers.
- A receiver answers an OPEN (flags 0) with its own (flags 1) in front of its
  next ACK or CLOSE to that station, or of its next over to it.
- A sender that holds the peer's OPEN sends no object larger than its limit
  (the transfer fails at once) and no symbols larger than its limit.
- A receiver answers an OFFER for an object larger than it accepts with CLOSE
  too large, and a busy receiver answers with CLOSE busy, both once the over
  ends, like an ACK. It does not collect symbols for such a transfer.
- A DATA frame of the same transfer whose length the receiver accepts shows
  that the OFFER was corrupted: the receiver then does not send CLOSE too large.
- On CLOSE busy, the sender waits the given time, then offers again. On CLOSE
  refused, the transfer fails. On CLOSE too large, it fails if the peer's OPEN
  gives a limit below the object's length; otherwise the CLOSE answered a
  corrupted OFFER, and the sender sends its OPEN and OFFER again.

## 8. Beacons

A station announces itself with a BEACON frame: destination broadcast, session
and index 0 (receivers ignore both). Payload, 108 + 7n bytes:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | flags: 0x01 mailbox (holds mail for other stations), 0x02 relay (passes mail on, Phase 2), 0x04 internet (has internet links); other bits 0 |
| 1 | 32 | the station's Ed25519 key |
| 33 | 4 | Unix time in seconds when sent |
| 37 | 6 | Maidenhead locator: 4 or 6 characters in upper-case ASCII (`JO89` or `JO89XI`), a 4-character one followed by two zero bytes; all zero when the station gives none |
| 43 | 1 | n, stations heard (at most 16) |
| 44 | 7n | per station heard: callsign (6), minutes since last heard (1; 255 = 255 or more) |
| 44 + 7n | 64 | signature by the key over `"hm/beacon-sig/v0" \|\| header source callsign (6) \|\| bytes 0 to 43 + 7n` |

Rules:

- Drop a beacon with a malformed locator (letters A–R, digits, then A–X).
- Drop a beacon whose signature does not verify with the key it carries. One that
  verifies proves only that its sender holds that key.
- Never learn a key from a beacon. Compare it with the trusted keys instead: the
  listed key (trusted), no key for the station (unknown), or another key
  (mismatch: an impostor, or a station with a new key; worth a warning).
- Recommended: beacon every 10 minutes with ±10% jitter, the first one at a random
  moment 5 to 30 s after the radio comes up; list stations heard in the last hour,
  most recent first.

## 9. KISS and AX.25 encapsulation

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

On air, the AX.25 frame travels in HDLC (flags, bit stuffing, CRC) or in IL2P
as NinoTNC and Direwolf 1.7 send it: one 0x55 byte, the sync word 0xF15E48, a
type 1 header (the two addresses, UI control and PID; 13 bytes, scrambled, then
2 Reed–Solomon parity bytes) and the information field in scrambled blocks, each
followed by its parity (16 bytes per block of at most 239 with maximum FEC). A
station that decodes IL2P sets the IL2P feature bit in its OPEN (section 7).

## 10. Internet links

Stations may also link over the internet. A link is a QUIC connection (RFC 9000)
with TLS 1.3, ALPN `hm-net/1`, and mutual authentication by station key:

- Each station presents a self-signed X.509 certificate whose subject public key
  is its Ed25519 station key. Only Ed25519 handshake signatures are used.
- Each side accepts the other only if that key is one it trusts, and names the
  peer by the callsign bound to the key (with its SSID, if the trust entry has one;
  two stations sharing one callsign can link at once if each has its own key). Certificate names, issuers and validity
  periods carry no meaning.
- The dialer's TLS 1.3 handshake completes before the listener has checked the
  dialer's certificate. Once the listener has accepted the dialer, it opens a
  unidirectional stream and sends `"HMOK"`. The dialer counts the link as up
  only after receiving it; a listener that refuses the dialer closes the
  connection instead.
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
not fail against its trust file. This receipt proves next-hop custody of these
exact bytes. Only the destination-signed end-to-end receipt in section 11.3
marks the origin's message delivered.

**Over ARQ modems.** A connection made by an ARQ modem (VARA, Mercury, ARDOP)
is a reliable byte stream between two stations named by the modem. It carries
the same exchange, one bundle after another on the same stream: `"HMD0"`,
length, object from the sender; `0x00` and the receipt, or `0x01`, length and
reason, from the receiver. Either station may send on a connection, whoever
called; the first byte tells a bundle (`H`) from an answer (`0x00`, `0x01`).
The caller hangs up when it has nothing more to send. There is no TLS on this
path: the bundles' signatures and the custody receipt carry the authentication.

## 11. Phase 2 routing, custody and synchronization

### 11.1 Contacts and route selection

A contact is a directed opportunity to transfer bytes:

`(origin, peer, seq, start, end, bearer, success, rate, capacity, flags)`.

`start` and `end` are Unix seconds. `success` is a conservative probability in
parts per 10,000, `rate` is effective bits/s, and `capacity` is residual bytes.
Only `origin` may sign an outgoing contact claim. Claims expire at `end`.

Every node keeps Beta evidence `(alpha, beta)` for each directed
`(origin, peer, bearer, UTC-hour)` edge. A successful custody handoff increments
`alpha`, a failed handoff increments `beta`, and old evidence decays. Routing
uses a conservative posterior quantile `q`, not an RSSI value or a random draw.

For a time-respecting route `r`, under the independent-edge approximation:

`P_success(r) = product(q_e)` and `C_risk(r) = sum(-ln(q_e))`.

A route is feasible only when every contact has residual volume for the object,
projected arrival is before bundle expiry, no station repeats, and `max_hops`
is not exceeded. Among feasible routes within the local airtime budget, choose
the greatest `P_success`; break ties by earliest projected arrival, least
airtime, then fewest hops. Keep up to three alternatives for sequential
failover, but activate only one.

Routine and priority bundles have one active custodian. Immediate and flash
bundles may have two copies only on edge-disjoint feasible routes and only when
both fit the airtime budget. There is no neighbourhood payload flood.

### 11.2 Custody and end-to-end delivery

The transfer receipt in section 7 means only that the next hop durably accepted
custody. A custodian stores the exact signed bundle bytes before signing that
receipt. After a verified custody receipt the previous custodian deactivates
its queued copy; it may retain bytes for deduplication and audit.

Each hop carries mutable routing metadata outside the signed bundle:

| Field | Type |
| --- | --- |
| hop_count | uint 0–16 |
| visited | array of callsigns, at most 16 |

On a transfer the metadata is encoded as a routing wrapper:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | ASCII magic `HMR1` |
| 4 | 1 | hop count |
| 5 | 1 | visited count, equal to hop count |
| 6 | 4 | inner signed-envelope length, big-endian |
| 10 | 6n | visited callsigns |
| 10+6n | variable | exact signed bundle envelope |

The wrapper must end exactly after the declared inner envelope. Duplicate
callsigns, unequal counts, an empty inner envelope, or more than 16 hops are
invalid. A payload without `HMR1` is a legacy hop-zero signed envelope.

A relay increments `hop_count`, appends itself, and rejects a wrapper that
already names it or exceeds the bundle's `max_hops`. The inner signed envelope
is never changed.

Only the final recipient establishes end-to-end delivery. After storing a
non-receipt bundle addressed to itself, it emits one signed kind-5 receipt
bundle. The receipt is routed like any other bundle. The original sender marks
the outbox delivered only after verifying that final recipient's signature and
matching `reply_to`.

### 11.3 SYNC payloads

SYNC frame payload byte 0 selects a message. Headers name the immediate sender
and receiver; contact signatures name their origin separately.

**CONTACT (0x01), 101 bytes**

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | subtype 0x01 |
| 1 | 6 | origin callsign |
| 7 | 4 | sequence |
| 11 | 4 | contact start, Unix seconds |
| 15 | 4 | contact end, Unix seconds |
| 19 | 6 | peer callsign |
| 25 | 1 | bearer: 0 radio, 1 internet, 2 ARQ modem |
| 26 | 2 | success, parts per 10,000 |
| 28 | 4 | effective bit rate |
| 32 | 4 | residual capacity bytes |
| 36 | 1 | origin flags: mailbox, relay, internet |
| 37 | 64 | Ed25519 signature over `"hm/contact/v0"` and bytes 1–36 |

**FILTER (0x02), 8 + ceil(m/8) bytes**

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | subtype 0x02 |
| 1 | 1 | scope: 0 mail held for receiver, 1 relayable holdings |
| 2 | 1 | number of hashes `k`, 1–16 |
| 3 | 1 | reserved 0 |
| 4 | 4 | salt |
| 8 | variable | Bloom bits, 8–2048 bits |

For `n` expected ids, choose `m` and `k` for target false-positive probability
`(1 - exp(-k*n/m))^k`. False positives delay reconciliation; they never cause
wrong delivery.

**WANT (0x03), 2 + 8n bytes**

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | subtype 0x03 |
| 1 | 1 | count `n`, 1–16 |
| 2 | 8n | first 8 bytes of requested bundle ids |

**OFFER (0x04), 4 + 8n bytes**

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | subtype 0x04 |
| 1 | 1 | scope, as in FILTER |
| 2 | 1 | count `n`, 1–16 |
| 3 | 1 | reserved 0 |
| 4 | 8n | first 8 bytes of offered bundle ids |

Filters, offers and wants are pairwise. A receiver first sends a FILTER of ids
it already holds. The peer sends bounded OFFER pages only for eligible ids not
present in that filter, and the receiver returns WANT for missing offers.
Eight-byte prefixes MUST resolve unambiguously at the sender; otherwise that
entry is not transferred. The explicit OFFER is necessary because a Bloom
filter alone cannot enumerate ids known only to its sender.

SYNC vectors (hex), using station secret `07` repeated 32 times for CONTACT:

- CONTACT: `0100004f8af6fb000000076ab13b806ab14990001b97cbd86b00222e0000258000008000012748b2718a6e97fd88aab77bccf6918a6dddf9f1d5173c8739140c446d2fe20c1b2c0150717390e1506d7a225cd2f28520f5d54bbb151f7708d2b4e2ab96ff0c`
- FILTER: `02000500123456780000080020001018`
- OFFER: `04000100032a5159a9ca4717`
- WANT: `0301032a5159a9ca4717`

CONTACT dissemination uses the Trickle
algorithm (RFC 6206): inconsistency resets the interval, consistent duplicates
suppress transmission, and a stable network becomes quiet. Control traffic has
a separate rolling radio budget, 2% by default. A beacon may carry a directory
digest; peers request details only when state differs.

### 11.4 Admission, expiry and cancellation

A relay answers busy/refuses custody when its configured holdings count, queue
bytes or airtime budget is exhausted. Contact residual volume is reserved while
a handoff is active and consumed on custody acceptance.

A relay MUST accept custody only when the signed bundle origin verifies against
its local trust list (the current RF-authorization allowlist) and the bundle
names a single final station destination. The destination need not be listed
locally. A relay MUST NOT forward an existing holding after that origin is
removed from trust.

Before transmitting over packet radio or an ARQ modem, a node MUST re-check that
the bundle's end-to-end origin is either this station or present in the local
trust list. Internet handoffs remain separately authenticated by mutual TLS and
are not subject to that airtime gate. A hop MUST transfer custody only after a
receipt signed by the next hop's trusted key; an unverified ACK leaves the
previous custodian holding the copy.

Final delivery of an unverified bundle is stored and shown as unverified, but
MUST NOT produce an automatic end-to-end receipt (which could otherwise leave
over RF).

An operator may cancel a locally queued outbound bundle. Cancellation removes
it from the local delivery queue and ignores late hop acknowledgements. It
cannot recall a copy whose custody was already accepted downstream.

## 12. Forward compatibility

- Unknown map keys are ignored when decoding.
- Unknown `kind`, `prec` and `codec` values decode as "other" and are kept.
- Unknown address tags are kept with their raw CBOR value and re-encoded verbatim.
- Because ids and signatures cover raw bytes, older nodes route and verify bundles
  written by newer software.

## 13. Test vectors

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

## Transfer of the chat bundle (SA0KAM -> SO5KM-1, symbol size 200, first over, opening the session)
object_id 26a90b587084a213a812a105b23553637c1afd5c125fc66381203f4583e09807
open  0300004f8af6fb001b97cbd86bf2b4000000020000000000040000fff802
offer 0300004f8af6fb001b97cbd86bf2b40000000126a90b587084a213a812a105b23553637c1afd5c125fc66381203f4583e0980700007700c80001
data  0000004f8af6fb001b97cbd86bf2b400000000007700825832a70000014600004f8af6fb028182004600000207586b0301051a6ab13b8006190e100882004c3733206465205341304b414d5840ce3c7adc856375a2ce7cbb47011edcbbfa93ff43bf4346232367268be9e35cf86d01b31ab2408b6ba8954f5088b430c211eca59261c6d9805cd446f9773aa602000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000

## Receipt ACK from SO5KM-1 (secret 0x0c x 32) for that transfer, after its OPEN reply
public key 0b513ad9b4924015ca0902ed079044d3ac5dbec2306f06948c10da8eb6e39f2d
open  03001b97cbd86b00004f8af6fbf2b4000000020100000000040000fff802
ack   01001b97cbd86b00004f8af6fbf2b4000000000080ff00000126a90b587084a21311b09926955fc1657ddf7c42bcd35089e157a8f522ae8b7fdef4005e57dfc38369d6f4e7ee8c50ba3f1b1fb1659e1139f6b4bc0d8783a6ea8ed55498b5025204

## CLOSE from SO5KM-1 to SA0KAM (busy, retry after 90 s, session 0xBEEF)
close 03001b97cbd86b00004f8af6fbbeef0000000301005a

## Beacon from SA0KAM-10 (secret 0x0b x 32, mailbox, JO89xi, heard SO5KM-1 3 min ago)
beacon 04a53e713ef6fbffffffffffff00000000000166be7e332c7a453332bd9d0a7f7db055f5c5ef1a06ada66d98b39fb6810c473a6ab13b804a4f3839584901001b97cbd86b036310c680632414ca1c334f57df9df4b445475c36b99df19b143662928e5b8bdb1a2d9709298a363a5c9781d062ccba553c8b2b75e4a78d978bb209f6f2fac302
```
