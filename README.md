# hm-net

**hm-net** is an experimental research protocol and station software for
delay-tolerant amateur-radio messaging: mail, chat, forms and group bulletins
over VHF/UHF and HF, with optional authenticated Internet and ARQ-modem paths.
It is a **federated** network of peer stations—each operator runs their own
node and chooses whom to trust—not a client/server service or a central
mailbox. Written in Rust, it keeps content in the clear and authenticates
every identity with Ed25519 signatures.

The project treats the radio channel as a scarce, half-duplex, lossy medium
rather than as a transparent pipe. Custody, contact modelling and airtime
discipline are first-class; flooding and anonymous relays are not. The wire
format and the crates are still evolving (wire version 0) so that policy,
routing and bearer behaviour can be changed without rewriting the station
around a monolith.

`hm` is a working command-line prefix until the project has a settled name.

## Why this design

Amateur packet networks often inherit TCP/IP assumptions—always-on links,
end-to-end round trips, and best-effort flooding—that fit poorly on shared RF.
hm-net instead combines store-and-forward custody with a **probabilistic
contact graph** and **bearer selection under uncertainty**, so a station can
decide *whether*, *when* and *over which medium* to spend airtime.

| Strength | What it means in practice |
| --- | --- |
| **Signed custody, not blind relay** | Each hop moves durable custody only after a verified next-hop receipt; the prior hop retains a shadow copy for reclaim. The origin marks **Delivered** only on a destination-signed end-to-end receipt (else eventually **DeliveredUnconfirmed** or **Failed**). |
| **Bayesian contacts** | Link success is a decaying Beta posterior from schedules, beacons, live links and handoff outcomes—not a single SNR snapshot. |
| **Thompson-sampled bearers** | Radio, Internet and ARQ modems compete by discounted cost ÷ sampled success rate, so a failing radio yields to Internet and is retried as evidence fades. |
| **Fountain-coded RF transfers** | RaptorQ symbol bursts with adaptive sizing absorb loss without stop-and-wait ACKs on every fragment. |
| **Airtime as a budget** | Relays and control traffic (Trickle contacts, holdings SYNC) are capped; urgent traffic may use at most one extra edge-disjoint copy. |
| **Federated trust** | Station keys authenticate Internet (QUIC/TLS 1.3) and verify bundles; optional public core hubs accept dial-ins without a directory of every home station. |
| **Research-shaped codebase** | Protocol layers live in small, dependency-ordered crates (`hm-wire` → `hm-bundle` → `hm-xfer` / `hm-route` → `hm-cli`), so policies can be swapped or simulated in isolation. |

## Bayesian pieces

Two places use Bayesian reasoning explicitly:

1. **Contact graph (`hm-route`)** — each directed edge
   `(origin, peer, bearer, UTC hour)` keeps Beta evidence `(α, β)`. Successful
   custody increments `α`, failures increment `β`, and evidence decays with
   time. Route scoring uses a **conservative posterior quantile** (not a mean
   and not a random draw), so plans prefer contacts that are both likely and
   well-supported. Feasible routes must meet deadline, residual capacity,
   airtime and hop limits; the node keeps a short failover list and activates
   one custodian at a time for routine traffic.

2. **Bearer choice (`hm-cli` chooser)** — for a neighbour that can be reached
   on more than one medium, each `(station, bearer)` arm is a Beta belief
   (optimistic prior). The node draws a success rate from each available arm
   and picks the lowest expected cost `cost / rate` (**discounted Thompson
   sampling**). Failures push traffic toward Internet or modem; as they fade,
   radio is tried again.

Payload routing itself stays deterministic given the graph; randomness is
confined to bearer exploration so simulation and replay remain reproducible
when the RNG seed is fixed.

## Protocol layers

| Layer | Responsibility | Crate(s) | Design note |
| --- | --- | --- | --- |
| **Identity & trust** | Callsigns, Ed25519 keys, signed envelopes, local trust lists | `hm-ident`, `hm-cli` config | Trust is explicit and local; keys are never learned from beacons. |
| **Application objects** | Chat / mail / bulletin / receipt bundles, compression | `hm-bundle` | Content-addressed, sealed objects; cleartext by design for amateur service. |
| **Custody & persistence** | Inbox, outbox, relay holdings, retries, shadow retain, suspect reclaim | `hm-store` | Crash-safe `redb` store; at-most-once local store by content id; hop at-least-once while queued |
| **Contact & routing** | Bayesian graph, CGR-style plans, failover, urgent dual-path | `hm-route` | Policy lives here: change scoring or admission without touching RF framing. |
| **Transfer** | Sessions, OFFER, RaptorQ bursts, ACKs, transfer receipts | `hm-xfer` | Sans-IO engine driven by `hm-core::Machine`; same logic on radio and in sim. |
| **Wire framing** | 18-byte headers, ACK/OFFER/OPEN/CLOSE, beacons, SYNC | `hm-wire` | Compact, versioned; independent Python vectors in CI. |
| **Bearers** | KISS/AX.25, built-in AFSK, QUIC Internet, ARQ modem hosts | `hm-bearer`, `hm-modem-afsk`, `hm-net`, `hm-rig`, modem glue in `hm-cli` | Bearers are interchangeable ports into the transfer engine. |
| **Station orchestration** | Daemon, setup, web UI/API, live settings | `hm-cli` | Thin coordination over the libraries above—not a second protocol stack. |
| **Validation** | Discrete-event RF/Internet simulation, baseline routing compare | `hm-sim` | Deterministic experiments before on-air trials. |

Wire details and test vectors: [`SPEC.md`](SPEC.md). Bulletin channels:
[`docs/bulletin-channels.md`](docs/bulletin-channels.md).

## Crates at a glance

| Crate | What it does |
| --- | --- |
| `hm-core` | `Millis`, the sans-IO `Machine` trait, station `Input`/`Output` with radio ports, deterministic RNG |
| `hm-wire` | Base-40 callsigns, 18-byte frame header, ACK payload, object ids, signed contact deltas and pairwise holdings-reconciliation messages |
| `hm-ident` | Ed25519 identities, signed envelopes, callsign binding records, attestations |
| `hm-bundle` | Messages: build, seal, open, verify; end-to-end receipts; attachment references; bounded zstd body compression with a versioned shared dictionary |
| `hm-route` | Bayesian contact graph from schedules, beacons, live links and delivery evidence; capacity-, deadline-, airtime- and storage-aware route selection with sequential failover and bounded urgent replication |
| `hm-sim` | Discrete-event simulator: multiple channels and radios per station, airtime with keyed-PTT bursts and AX.25/HDLC framing checked against the modulator, half-duplex, p-persistent CSMA with a measured carrier-detect delay, hidden-terminal collisions, Bernoulli / Gilbert–Elliott / hourly HF loss or the built-in modem's measured loss by SNR and frame length, outages, partitions, clock drift, corrupted frames, airtime split by purpose, delivery and latency metrics; deterministic routing comparisons against epidemic, Spray-and-Wait, PRoPHET-like and MEED baselines |
| `hm-bearer` | KISS framing (streaming decoder) and AX.25 UI encapsulation for Direwolf and hardware TNCs |
| `hm-xfer` | Fountain-coded (RaptorQ) transfer engine: OFFER + symbol bursts, ACKs with the missing count, hash-verified delivery, signed delivery receipts, duplicate suppression, per-sender resource limits, sessions (OPEN with feature bits and limits, CLOSE for busy, refused or too large), loss-adaptive burst sizing, random exponential backoff, airtime budget |
| `hm-store` | Persistent store on `redb` (pure Rust, crash-safe): content-addressed messages, inbox, outbox and relay holdings, custody records, cancellations, retries with exponential backoff, shadow retain after handoff, custody suspect reclaim; at-most-once local store by content id |
| `hm-net` | Internet links: QUIC with TLS 1.3 on Ed25519 station keys (mutual trust by default; optional `open_hub` for public cores), automatic redial, one stream per bundle with a signed receipt |
| `hm-modem-afsk` | Our own AFSK 1200 modem (Bell 202), pure Rust and `no_std`: HDLC framing and CRC, multi-slicer demodulator with per-tone AGC and PLL clock recovery, carrier detect |
| `hm-rig` | Radio hardware: sound cards through `cpal` (ALSA, CoreAudio, WASAPI); PTT by rigctld (Hamlib CAT), serial RTS/DTR, CM108 GPIO (AIOC, Digirig) or VOX; a virtual radio channel for tests |
| `hm-cli` | The `hm` command: interactive first-run `setup`; `node` (the station daemon: packet radio, ARQ modems (VARA, Mercury, ARDOP) and/or Internet links, relay/mailbox, web interface, JSON API with access token, settings applied live); `keygen`, `whoami`, `trust`, `send` and `listen`; KISS links over TCP or a serial port, and a real-time driver |

## Status: Phase 0 complete; Phase 1 hardware validation open; Phase 2 relay software implemented

Phase 1's remaining gate is the on-air test ([plan](docs/on-air-test-plan.md)). The
first-run setup wizard and the built-in modem's deterministic deep-noise target are
implemented; real RF measurements may still lead to tuning.
Phase 2 now provides opt-in
multi-hop relaying, mailbox custody, congestion-bounded control traffic, compressed
message bodies and destination-signed end-to-end delivery receipts. It has deterministic
simulation and localhost integration coverage; real RF deployment testing is still needed.

### What remains

The core protocol and daemon are implemented, but the project is not yet field-complete:

- Run the first multi-hop on-air trials and tune contact probabilities, airtime limits,
  modem decoding and relay admission from real RF measurements.
- Test long-running mixed radio/Internet networks and interoperability between independently
  deployed nodes before freezing wire version 0.
- Field-test the ARQ modem integrations.
- Add discovery or rendezvous if it proves necessary. Today Internet peers use explicit
  DNS names or addresses; two nodes behind restrictive NAT need a publicly reachable relay.
- The built-in web server intentionally does not terminate TLS or provide multi-user
  accounts. Remote administration therefore needs an HTTPS reverse proxy, VPN or SSH tunnel.

## Run a station

```sh
hm setup                                  # KISS, sound-card modem, internet only, or core node
hm trust add "SO5KM-1 8a1e…"              # the line `hm whoami` prints on their side
hm node                                   # radio through Direwolf on 127.0.0.1:8001
```

`hm setup` validates each answer, lists detected sound devices, defaults the built-in
modem to IL2P, and can configure a locator, authenticated Internet listener, relay and
mailbox. Option **4 (core node)** is for a hub with no radio: it listens on the internet,
turns relay and mailbox on, and is meant to run on a computer with a public address (a
small rented cloud server is typical). It shows a summary before writing and never
overwrites an existing key or configuration. A second profile in the same folder uses
matching names (`hm setup --config core.toml` writes `core.key` and `core.db`, leaving
`station.key` alone). Type `q` at any prompt to leave without changing files. For
scripted provisioning, `hm keygen --call SA0KAM-1` remains available.

New stations trust and dial the public core hub **SA0KAM-0** at `34.51.161.47:4433` by
default. Remove or replace that `[[trust]]` / `[[internet.peers]]` entry if you do not
want it. A core hub sets `internet.open_hub = true` so home stations can dial in without
being pre-listed in the hub's trust file (QUIC and SYNC both use the dialer's certificate;
homes still must trust the hub).

### station.toml

Everything a station needs to know lives in one TOML file in the station's folder:
`[station]`, `[radio]`, `[internet]`, `[delivery]` and the trusted stations as `[[trust]]`
entries. Every setting has a default. `hm setup` writes the chosen settings; `hm keygen`
writes a commented starter file. The key itself stays in `station.key` (readable by you
only) and the web page's access token in `station.token`. Paths in the file are relative
to the file.

```toml
[station]
key = "station.key"

[radio]
kiss = "serial:/dev/ttyUSB0:57600"
beacon_minutes = 10

[internet]
listen = "0.0.0.0:4433"

[[internet.peers]]
station = "SO5KM"
address = "hm.example.org:4433"

[delivery]
radio_cost = 1.0
internet_cost = 2.0

[[trust]]
station = "SO5KM-1"
key = "8a1e…"
note = "Jan"
```

Every `hm` command reads `station.toml` from the current folder, or the file given with
`--config`. Command-line options override the file for one run (`hm node --help` names the
setting each one overrides) and the node logs which ones did. Unknown settings are errors,
so a typo is reported rather than ignored.

### Changing settings while the node runs

The node applies these at once, without a restart:

- **Trusted stations**: add or remove them on the web page, with `hm trust add/remove`, or
  by editing `[[trust]]`. A station taken off the list loses its internet link at once.
- **Delivery**: link costs and retry timing.
- **Internet peers**: which stations the node dials.
- **Radio**: everything under `[radio]` (TNC or sound card, PTT, key-up delay, channel
  access, turning the radio on or off). The node closes the old link and opens the new
  one; mail on its way over the old link is retried on the new one.
- **Beacon interval** and **grid locator**.

Changes made on the web page are written to `station.toml`, keeping its comments and layout.
The node also notices when the file is edited by hand, within a second or two. A file that
does not parse is logged and ignored, and the node keeps the settings it was using. The
internet listen address, the web address and the store are read at start-up; change them
in the file and restart. Settings given on the command line stay in force for that run,
even when the file changes; the web page lists them.

### The web page

`hm node` prints a link such as `http://127.0.0.1:8080/#token=…`. The token is also kept
beside the store (`station.token`); the page asks for it if you open the plain address.
The page has five views:

- **Chat**: 1:1 conversations by station, like a messenger. The left rail lists chats,
  stations **On frequency** (heard on the radio) and **Trusted** peers as one-click
  starts; type a callsign to open anyone else. Enter sends. The thread header shows
  trust, hearability, locator/distance and delivery path estimates when known.
  Badges distinguish pending, in-transit, delivered, failed, cancelled and received
  lines. A queued line can be dropped before delivery. Chats can be archived in the
  browser, or their inactive local history cleared; pending delivery is never erased
  by either action.
- **Mail**: messages with a subject and precedence, with an inbox and a sent log. Every
  chat line and mail item has a details view containing decoded metadata and the exact raw
  signed object, plus a control to delete an inactive local copy.
- **Bulletin**: group posts (`Address::Group`). On radio they broadcast once to listeners on
  frequency; over the internet they go to linked stations, and others can pull missed ones via
  holdings sync. No per-listener receipts; at most four publishes per hour and a small size cap.
  See [bulletin channels](docs/bulletin-channels.md).
- **Network**: link health (radio, internet, modem), per-station delivery estimates,
  the outbound queue (queued / in transit), stations heard on the radio (beacons,
  clock offset, offers, locators, distance and bearing), and trusted stations you can
  add or remove.
- **Settings**: everything the node applies without a restart.

The page stays up to date by itself: the node tells it what changed over a server-sent
event stream (`/api/events`). A chat line between two stations linked over the internet
arrives within a second; by radio it takes as long as the channel does. Chat and mail are
both stored and forwarded: a line to a station out of reach waits and goes out when a link
comes up. The node retries undelivered messages with growing delays (1 minute doubling to
an hour, 12 attempts, set in `[delivery]`). History stays in `station.db` until you delete
the local item or clear a chat; queued and in-transit messages are protected from history
cleanup.

Every 10 minutes (`beacon_minutes`, 0 for none) the node sends a signed beacon on the
radio: its callsign and key, its grid locator (`locator = "JO89xi"` under `[station]`),
whether it has internet links, and the stations it has heard in the last hour. The status
page lists every station heard, with the distance and bearing to those that give a locator,
and for those that beacon whether their key matches the one you trust. A beacon never adds a trusted station; a key that
differs from the trusted one is logged as a warning.

### The built-in modem

Without Direwolf, the node runs its own AFSK 1200 modem on a sound card:

```sh
hm audio-devices                                   # list sound cards
hm node --audio default --ptt vox
hm node --audio "USB Audio" --ptt cm108:/dev/hidraw0     # AIOC or Digirig
hm node --audio "USB Audio" --ptt rigctld              # CAT through Hamlib's rigctld
hm node --audio "USB Audio" --ptt rts:/dev/ttyUSB0
```

or in `station.toml`: `audio = "USB Audio"` and `ptt = "cm108:/dev/hidraw0"` under `[radio]`.

It sends the same AX.25 UI frames as the KISS path, so stations on the built-in modem and
stations on Direwolf work together. It waits for a clear channel (p-persistent CSMA on its
carrier detect: `persist`, `slottime_ms`), sends each transfer burst in one key-up, and
releases PTT on every exit path.

With `framing = "il2p"` under `[radio]`, it sends the same frames in IL2P, the framing of
NinoTNC and Direwolf 1.7: Reed–Solomon parity repairs up to 8 bad bytes in each block, so
frames get through far more noise. On a simulated channel, 40 frames of ~60 bytes at a
full-band SNR of −4 dB: 20 arrived as AX.25, all 40 as IL2P; at −6 dB, none against 30.
`framing = "auto"` sends IL2P to stations that said they decode it (every hm station on the
built-in modem does, and says so in its OPEN) and AX.25 to everyone else, beacons included.
The modem always decodes both. Tested against Direwolf both ways: Direwolf decodes all of
our IL2P frames, and we decode more of `gen_packets -I 1` than Direwolf itself (95 against
94 of 100 in rising noise).

### Radio, internet, or both

A node can reach other stations by radio, over the internet, or both:

```toml
# A home station with radio that also keeps an internet link to a server
[[internet.peers]]
station = "SO5KM"
address = "hm.example.org:4433"
```

```toml
# A server without a radio that trusted stations connect to
[radio]
enabled = false

[internet]
listen = "0.0.0.0:4433"
```

For a quick try the same works from the command line: `hm node --peer
SO5KM=hm.example.org:4433`, or `hm node --no-radio --listen 0.0.0.0:4433`.

Internet links are QUIC connections authenticated with the station keys themselves:
by default only mutually trusted stations connect, and each link is bound to a
callsign. A listener may set `internet.open_hub = true` to accept any dialer that
presents a valid station certificate (and its SYNC); homes must still trust the hub.
There is no certificate authority and no central server; any node can listen, dial, or both.

For direct neighbours, recent delivery evidence still determines whether radio or
internet is tried first. For a farther destination, the node builds a Bayesian contact
graph from configured schedules, signed beacons, live links and delivery evidence. It
chooses the route with the best modeled chance of meeting the deadline while respecting
contact capacity, airtime, storage and `relay.max_hops`.

Payload is not flooded. Routine traffic has one active custody next hop and tries
alternatives sequentially. Urgent traffic may use a second edge-disjoint route only when
the modeled probability gain exceeds `relay.urgent_min_gain`; it never has more than two
active copies. Relays are opt-in:

```toml
[relay]
enabled = true       # accept custody and forward toward another station
mailbox = true       # retain traffic for an intermittently connected recipient
max_holdings = 256
max_bytes = 16777216
max_hops = 8
airtime_budget_secs = 300
urgent_min_gain = 0.05
control_airtime_fraction = 0.02
```

Relay custody requires a verified origin in the local trust list (that list is
also the RF-authorization allowlist). The final destination need not be listed
at every hop. Removing the origin from trust stops any queued relay holding from
being forwarded. Before any packet-radio or ARQ-modem transmission, the node
re-checks that the end-to-end origin is this station or a trusted station;
Internet handoffs stay on mutual TLS and are not gated the same way. Custody
moves only on a verified next-hop receipt. Direct delivery of an unknown sender
is still shown as unverified, but no automatic end-to-end receipt is queued for
it (so RF is not used to acknowledge an unauthorized origin).

Signed contact deltas use Trickle suppression. Pairwise FILTER → OFFER → WANT exchanges
reconcile holdings without broadcasting payload, and radio control traffic is capped at
2% of the exact rolling airtime window by default. An authenticated custody receipt means
only that the next relay now holds the bundle. The origin marks it **Delivered** only
after a signed kind-5 receipt from the final destination returns, normally over the
reverse route. Message bodies can use codec 1, a bounded zstd stream with the pinned
shared dictionary in `hm-bundle`.

Run the deterministic comparison against bounded-copy and flood baselines with:

```sh
cargo run -q -p hm-sim --example routing_compare
```

It prints delivery, latency, payload/control airtime, storage, copy count, failures,
fairness and probability calibration. Epidemic routing is intentionally retained as an
upper-bound baseline: it can deliver more in some partitions, but at substantially
greater airtime and storage cost.

### Host a protected Internet-only node

The web/API listener and station-to-station listener are separate:

- The web/API is HTTP with a random 192-bit bearer token. Keep it on loopback and expose it
  only through HTTPS, a VPN or an SSH tunnel.
- Station links are QUIC over UDP with mutual authentication by station identity keys.
  The listener rejects stations not present in `[[trust]]`.

For a **core node** (hub that home stations dial), use a separate config so it does not
touch your home station files:

```sh
hm setup --config core.toml   # option 4; writes core.toml, core.key, core.db
hm --config core.toml node
```

Option 4 writes this shape: radio off, listen on `0.0.0.0:4433`, relay and mailbox on.
You can also write it by hand:

```toml
[station]
key = "core.key"
store = "core.db"
http = "127.0.0.1:8080"

[radio]
enabled = false
beacon_minutes = 0

[internet]
listen = "0.0.0.0:4433"
open_hub = true                 # any station may dial in; homes still trust this hub

# Optional: a node this server dials. Accepted connections are bidirectional,
# so a public listener does not need a peer entry for every client.
[[internet.peers]]
station = "OTHER-1"
address = "other.example.net:4433"

[relay]
enabled = true
mailbox = true
```

Exchange `hm whoami` output over a channel you trust, then add the other identity on both
nodes with `hm trust add "CALL KEY"`. Point DNS at the server and allow **UDP 4433** through
its firewall. A home station connects with:

```toml
[[internet.peers]]
station = "HUB-1"
address = "node.example.net:4433"
```

QUIC is bidirectional once either side connects. If a home node cannot accept inbound UDP,
it can dial the default public core (`SA0KAM-0` / `34.51.161.47:4433`); no automatic NAT
traversal or public peer directory exists yet. Do not put UDP 4433 through an HTTP reverse
proxy.

For remote web access, keep port 8080 private and terminate HTTPS separately. For example,
Caddy can add a second authentication layer in front of the node:

```caddyfile
node.example.net {
    basic_auth {
        operator {$HM_WEB_PASSWORD_HASH}
    }
    reverse_proxy 127.0.0.1:8080
}
```

Generate the password hash with `caddy hash-password`, set `HM_WEB_PASSWORD_HASH` for
Caddy, and expose only Caddy's TCP 443 (and TCP 80 if used for certificate issuance).
The node's bearer token is still required after Basic authentication. The token file is
owner-only on Unix; the node now refuses a weak or group/world-readable token file.
To revoke browser access, stop the node, remove `station.token`, and restart it to generate
a new token.

An SSH tunnel avoids exposing the web service at all:

```sh
ssh -L 8080:127.0.0.1:8080 user@node.example.net
```

Then open the token-bearing URL at `http://127.0.0.1:8080`. Station messages remain
cleartext inside their signed bundles, as stated at the top of this README: QUIC protects
the link, but trusted relays and anyone with filesystem access to the store can read them.

### Four relaying stations on one laptop

This creates an internet-only chain `SA0KAM → SM0R1 → SM0R2 → SO5KM-1`. QUIC has the
same authenticated bundle and control behavior as a radio link, so this is the quickest
way to exercise multi-hop routing and end-to-end receipts. It does not simulate RF loss;
use `hm-sim` or the integration tests for that.

Build the binary, make four independent station directories, and exchange all four
identities:

```sh
cargo build -p hm-cli
HM=target/debug/hm
mkdir -p lab/{a,r1,r2,b}
$HM --config lab/a/station.toml  keygen --call SA0KAM
$HM --config lab/r1/station.toml keygen --call SM0R1
$HM --config lab/r2/station.toml keygen --call SM0R2
$HM --config lab/b/station.toml  keygen --call SO5KM-1

A="$($HM --config lab/a/station.toml whoami)"
R1="$($HM --config lab/r1/station.toml whoami)"
R2="$($HM --config lab/r2/station.toml whoami)"
B="$($HM --config lab/b/station.toml whoami)"
for pair in \
  "lab/a/station.toml|SA0KAM" \
  "lab/r1/station.toml|SM0R1" \
  "lab/r2/station.toml|SM0R2" \
  "lab/b/station.toml|SO5KM-1"
do
  cfg=${pair%%|*}; self=${pair#*|}
  for identity in "$A" "$R1" "$R2" "$B"; do
    [ "${identity%% *}" = "$self" ] ||
      $HM --config "$cfg" trust add "$identity"
  done
done
```

In both `lab/r1/station.toml` and `lab/r2/station.toml`, change the existing relay
section to:

```toml
[relay]
enabled = true
mailbox = true
```

Start these in four terminals, in the shown order:

```sh
# B
target/debug/hm --config lab/b/station.toml node --no-radio \
  --http 127.0.0.1:8104 --listen 127.0.0.1:4204 --beacon-minutes 0

# R2
target/debug/hm --config lab/r2/station.toml node --no-radio \
  --http 127.0.0.1:8103 --listen 127.0.0.1:4203 \
  --peer SO5KM-1=127.0.0.1:4204 --beacon-minutes 0

# R1
target/debug/hm --config lab/r1/station.toml node --no-radio \
  --http 127.0.0.1:8102 --listen 127.0.0.1:4202 \
  --peer SM0R2=127.0.0.1:4203 --beacon-minutes 0

# A
target/debug/hm --config lab/a/station.toml node --no-radio \
  --http 127.0.0.1:8101 --listen 127.0.0.1:4201 \
  --peer SM0R1=127.0.0.1:4202 --beacon-minutes 0
```

Each process prints its token-bearing web URL. Open A's URL on port 8101 and B's on
8104, wait until the three links appear under Network, then send from A to `SO5KM-1`.
A's line passes through **Queued/In transit** and becomes **Delivered** only after B's
signed receipt traverses the chain back. Stop R2 before sending to observe queuing and
sequential retry; restart it to complete delivery. “Drop queued” in Chat or Mail cancels
a message that is still locally queued.

For a three-node smoke test, use only `a`, `r1` and `b`: start B as above, start R1
with `--peer SO5KM-1=127.0.0.1:4204`, then start A with
`--peer SM0R1=127.0.0.1:4202`. Keep relaying enabled only on R1. The same delivery-state
rule verifies that A received B's end-to-end receipt rather than only R1's custody receipt.

### Several stations under one callsign

Every SSID is a station of its own, so you can run more than one node, for example a home
station and a server:

```sh
hm --config home/station.toml keygen --call SA0KAM-1     # one folder and key per station
hm --config server/station.toml keygen --call SA0KAM-2
hm --config home/station.toml whoami                     # SA0KAM-1 <key>
```

Mail to SA0KAM-2 goes to that node only. A `[[trust]]` entry with an SSID names exactly
that station; one without (as from a key made with `--call SA0KAM`) covers every SSID
that has no entry of its own, for a key used with `ssid = …` under `[station]`. A node that
only uses the internet does not transmit, so it needs no licence; any name of up to 9
letters, digits, `-`, `/` or `.` works (`KAMHOME`), but use your callsign on anything
with a radio.

### The API

| Method | Path | |
| --- | --- | --- |
| GET | `/api/status` | callsign, key, radio and internet state, estimated delivery rate per station and link, stations heard on the radio with their beacons |
| GET | `/api/messages?direction=in\|out\|all&peer=CALL&kind=chat\|mail&limit=n` | newest first, with delivery state and link; `peer` gives one conversation |
| GET | `/api/messages/{id}` | decoded metadata and the exact raw signed object as hex |
| GET | `/api/events` | server-sent events, one `data:` line naming what changed: `message`, `status` or `settings` |
| POST | `/api/send` | `{"to", "text", "subject"?, "precedence"?}` → `201 {"id"}`; without a subject it is a chat line |
| DELETE | `/api/messages/{id}` | cancel a queued outbound message; a second delete, or deleting inactive/received history, removes the local copy |
| DELETE | `/api/conversations/{peer}` | delete inactive local chat history while preserving queued and in-transit messages |
| POST | `/api/read/{id}` | mark an inbound message read |
| GET | `/api/trust` | trusted stations with their notes, and the file they are saved to |
| POST | `/api/trust` | `{"line": "SO5KM-1 8a1e…", "note"?}` (as `hm whoami` prints it) → `201` |
| DELETE | `/api/trust/{station}` | stop trusting a station → `204` |
| GET | `/api/settings` | the settings in use: `live` ones, and those that take a restart |
| PATCH | `/api/settings` | any of `beacon_minutes`, `radio_cost`, `internet_cost`, `modem_cost`, `retry_first_secs`, `retry_max_secs`, `retry_attempts`, `peers` (`[{"station", "address"}]`), `locator` (`""` for none), `radio` (any `[radio]` fields) → the new settings; saved to `station.toml` |

Every `/api` request needs `Authorization: Bearer <token>`. The API is plain HTTP and
listens on localhost by default; to use the web interface from another machine, put it
behind HTTPS (a reverse proxy) or an SSH tunnel so the token never crosses a network in
clear.

## On the air with Direwolf

Tested so far against a simulated KISS TNC (see `crates/hm-cli/tests/e2e.rs`); the first
on-air test is next.

1. Run Direwolf with `MODEM 1200`, your `PTT` setting and `KISSPORT 8001` (the default).
   Use a packet or digital channel from your band plan, not the APRS frequency.
2. On each station, create a key and share the line `hm whoami` prints:

   ```sh
   hm keygen --call SA0KAM
   hm whoami                    # give this line to the other station
   hm trust add "SO5KM 8a1e…"   # and add theirs
   ```

3. Receiving station:

   ```sh
   hm listen --ssid 1
   ```

4. Sending station:

   ```sh
   hm send --to SO5KM-1 --text "73 de SA0KAM"
   hm send --to SO5KM-1 --subject "Sked" --text "40m 7.047 at 19Z?" --precedence priority
   ```

On this path every hm frame is an AX.25 UI frame from your callsign to `HMNET`, so each
transmission carries your station identification, and Direwolf's own CSMA (`PERSIST`,
`SLOTTIME`) handles channel access. Callsigns must fit AX.25: at most 6 letters or digits
plus SSID 0–15. If transfers time out on a slow or busy channel, raise `guard_ms`; if your
TNC's key-up delay differs from 300 ms, set `txdelay_ms` to match (both under `[radio]`,
or `--guard` and `--txdelay` for one run).

## A hardware TNC on a serial port

`--kiss` also takes a serial port, for TNCs such as a NinoTNC, Mobilinkd, TNC-Pi or a
TNC-2 in KISS mode:

```sh
hm node --kiss serial:/dev/ttyUSB0:57600
hm send --kiss /dev/ttyACM0 --to SO5KM-1 --text "via a hardware TNC"   # 9600 Bd
hm listen --kiss COM3                              # Windows
```

A hardware TNC keys the radio and waits for a clear channel itself, so on opening the port
`hm` sends it the key-up delay, persistence and slot time (`txdelay_ms`, `persist`,
`slottime_ms`) as KISS parameters. The TNC must already be in KISS mode.

## HF and FM through VARA, Mercury or ARDOP

An ARQ modem program is a third way to reach stations, next to the radio and the internet:
VARA HF or FM, Mercury (which speaks VARA's host interface) or ARDOP. The modem does its
own error correction and retries, and hm uses the connection it makes the way it uses an
internet link: each bundle goes over it whole, and the next hop answers with a signed
custody receipt. The origin marks the message **Delivered** only when the destination's
separate signed end-to-end receipt returns, just as it does on the other bearers.

```toml
[modem]
enabled = true
kind = "vara"        # or "ardop" (port 8515); Mercury: "vara" with its ports
host = "127.0.0.1"
port = 8300          # command port; data is on the next one
bandwidth = 2300     # VARA HF 500, 2300 or 2750; ARDOP 200 to 2000; 0 leaves it
ptt = "none"         # the modem keys the radio; or "rts:/dev/ttyUSB0", "cm108:…", "rigctld"
```

The node registers its callsign with the modem and listens. To deliver, it calls the
station, sends every bundle waiting for it and hangs up; calls from other stations are
answered. The modem carries one connection at a time, so deliveries to other stations wait
their turn. How mail is shared between radio, internet and modem follows the costs
(`modem_cost = 1.5` by default, between radio and internet) and how well each has been
delivering lately. The status line shows whether the modem program is reachable and whom
it is connected to. Changing `[modem]` takes a restart.

## Measured (simulator, 1200 bd, 300 ms TXDELAY)

The simulated channel sends frames as the built-in modem does: AX.25 UI header,
frame check, flags and bit stuffing included (within 0.02% of the modulator's
airtime). SNR is measured in a 3 kHz bandwidth.

| Scenario | Result | Reproduce |
| --- | --- | --- |
| 1 kB, 10% frame loss both ways, 10,000 trials | 100% delivered, 0 duplicates, every receipt verified; latency p50 9.3 s, p95 23.9 s | `HM_XFER_TRIALS=10000 cargo test -p hm-xfer --release --test sim exit_criterion_1kb -- --nocapture` |
| 5 kB, clean link | hm headers and preambles 9.6%, OPEN, OFFER and ACK with receipt 2.2%, TXDELAY and TXTAIL 2.7%; AX.25 framing and bit stuffing 9.9%; 72.6% of airtime is useful payload | `cargo test -p hm-xfer --release --test sim exit_criterion_overhead -- --nocapture` |
| 2 kB, bursty loss (Gilbert–Elliott, ~12% mean) | 100/100 delivered | `cargo test -p hm-xfer --release --test sim bursty -- --nocapture` |
| Two hidden senders to one node, no CSMA | 30/30 both delivered, last within 141 s | `cargo test -p hm-xfer --release --test sim two_senders -- --nocapture` |
| 2 kB over the modem's measured loss at 7 / 8 / 9 dB SNR | 100/100 delivered at each; latency p50 25.1 / 17.5 / 17.5 s | `cargo test -p hm-xfer --release --test sim measured_modem -- --nocapture` |
| Four stations to one hub, 1.5 kB each, 9 dB, all hear each other | without CSMA: last delivery p50 251 s, 4.8 overs per object, 153 receptions lost to collisions per run; with CSMA: p50 126 s, 1.8 overs per object, 44 lost, all from stations keying up within the 125 ms carrier-detect delay of each other | `cargo test -p hm-xfer --release --test sim busy_channel -- --nocapture` |
| Receiver never keys up during an over, 8 kB at 15% loss | 0 frames talked over in 40 runs | `cargo test -p hm-xfer --release --test sim nobody_talks -- --nocapture` |
| Modem frame loss vs SNR, 48 kHz, white noise | 38% of 40-byte frames lost at 5.5 dB, 40% of 360-byte frames at 7 dB, none at or above 9.5 dB; table in `crates/hm-sim/src/afsk_1200.rs` | `HM_WRITE_CURVE=1 cargo test -p hm-sim --release --test afsk afsk_1200_curve -- --ignored` |
| Modem carrier detect | 68–101 ms after key-up at 7–20 dB SNR | `cargo test -p hm-sim --release --test afsk carrier_detect -- --nocapture` |
| Modem vs Direwolf 1.8.1, `gen_packets -n 100` at 11–48 kHz | 246 frames decoded vs 236 for Direwolf's better profile (104%) | `cargo test -p hm-modem-afsk --release --test modem -- --nocapture` (needs `direwolf` installed) |
| Modem vs Direwolf 1.8.1, held out: tilt ±6 dB/octave, SNR down to −4 dB | 333 vs 328 (102%) | same |
| Modem interop | Direwolf decodes 50/50 of our frames, clean and at 12 dB SNR | same |
| 5% undetected frame corruption | 33/40 delivered (92% over 400 runs), none corrupt | `cargo test -p hm-xfer --release --test sim heavy_corruption -- --nocapture` |

The wire format is in [SPEC.md](SPEC.md), with test vectors verified by an
independent Python implementation.

## Build and test

Requires Rust 1.90 or newer.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo install --path crates/hm-cli   # installs the `hm` command
```

## Verification

Windows and macOS are slow on CI, so the tests run there only for a release tag
(`git tag v0.1.0 && git push origin v0.1.0`) or when the ci workflow is run by hand
(Actions > ci > Run workflow); everything else runs on Linux.

| What | How | Every pull request and push to main (Linux) | Nightly |
| --- | --- | --- | --- |
| Test vectors | `cargo run -q -p hm-bundle --example vectors \| python3 tools/check_vectors.py` (needs `pip install blake3 pynacl cbor2`) | yes | |
| Simulator vs independent oracle | `HM_SEEDS=10000 cargo test -p hm-sim --release --test oracle` | 100 seeds | 10,000 seeds |
| Simulated channel vs the modem | airtime of AX.25 frames against the modulator's output; carrier-detect delay against the demodulator; the frame-loss table re-measured (`cargo test -p hm-sim --release --test afsk -- --include-ignored`) | airtime, carrier detect | loss table |
| Decoder mutations (no panic, no forgery) | `HM_MUTATIONS=1000000 cargo test -p hm-bundle --release --test mutations` | 5,000 | 1,000,000 |
| Coverage-guided fuzzing | `cd fuzz && cargo +nightly fuzz run <target>`; targets: frame, ack, envelope, bundle, binding, callsign, kiss, ax25, xfer, beacon | compile only | 10 min per target |
| Cross-platform determinism | pinned trace hash of a reference simulation | Linux; Windows and macOS on release tags | |
| End to end over TCP | fake KISS TNC relaying frames (with drops and APRS noise) between `hm listen` and `hm send` processes | yes | |
| KISS over serial | pseudo-terminals as serial TNCs: channel-access parameters on opening, frames both ways, the port released on close, `hm send` and `hm listen` delivering through two serial TNCs | Linux; macOS on release tags | |
| Built-in modem link | stations on a virtual radio channel in real time: carrier sense defers to a busy channel, PTT only around transmissions, one key-up per burst, two nodes exchanging mail | yes | |
| Internet links | QUIC stations on localhost: delivery with verified receipts, rejection, impostors and wrong server keys refused, redial after restart | yes | |
| Station nodes | `hm node` instances driven only through the HTTP API: radio delivery, restart, store-and-forward, internet-only nodes, radio failing over to the internet | yes | |

The oracle test builds random two-channel networks with every fault type and
replays the simulator's log through channel rules written independently of the
simulator, bit stuffing and carrier sense included. The mutation test checks that any mutated message which still
verifies carries exactly the original signed bytes.

## Design rules

- Protocol crates are `no_std` + `alloc` and never read clocks, sleep or do I/O.
  Time and randomness are passed in, so the simulator and the daemon run identical code.
- Signed objects are forwarded byte-for-byte. Ids and signatures cover the raw
  signed bytes, so old nodes can route and verify messages written by newer software.
- The simulator is deterministic: same seed, same run, on every platform.
- Performance claims come from the simulator or on-air tests, never from estimates.

## Licence

MIT or Apache-2.0, at your option.
