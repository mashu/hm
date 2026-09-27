# hm-net

A federated store-and-forward network for amateur radio mail, chat, forms and
bulletins over VHF/UHF and HF. Written in Rust; no central server; content in
clear, identities signed.

`hm` is a working prefix until the project has a name.

## Status: Phase 0 complete; Phase 1 hardware validation open; Phase 2 relay software implemented

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
| `hm-store` | Persistent store on `redb` (pure Rust, crash-safe): content-addressed messages, inbox, outbox and relay holdings, custody records, cancellations, retries with exponential backoff, exactly-once across restarts |
| `hm-net` | Internet links: QUIC with mutual TLS 1.3 on Ed25519 station keys (only trusted stations connect), automatic redial, one stream per bundle with a signed receipt |
| `hm-modem-afsk` | Our own AFSK 1200 modem (Bell 202), pure Rust and `no_std`: HDLC framing and CRC, multi-slicer demodulator with per-tone AGC and PLL clock recovery, carrier detect |
| `hm-rig` | Radio hardware: sound cards through `cpal` (ALSA, CoreAudio, WASAPI); PTT by rigctld (Hamlib CAT), serial RTS/DTR, CM108 GPIO (AIOC, Digirig) or VOX; a virtual radio channel for tests |
| `hm-cli` | The `hm` command: `node` (the station daemon: radio and/or internet links, relay/mailbox, web interface, JSON API with access token, settings applied live), `keygen`, `whoami`, `trust`, `send`, `listen`, all set up by one `station.toml`; KISS links over TCP or a serial port, and a real-time driver |

Phase 1 still to do: the on-air test ([plan](docs/on-air-test-plan.md)), IL2P framing and better decoding deep in noise for
the built-in modem, stream modems (Mercury, ARDOP, VARA) as radio bearers,
and the Dioxus interface with a setup wizard. Phase 2 now provides opt-in
multi-hop relaying, mailbox custody, congestion-bounded control traffic, compressed
message bodies and destination-signed end-to-end delivery receipts. It has deterministic
simulation and localhost integration coverage; real RF deployment testing is still needed.

## Run a station

```sh
hm keygen --call SA0KAM-1                 # writes station.key and a starter station.toml
hm trust add "SO5KM-1 8a1e…"              # the line `hm whoami` prints on their side
hm node                                   # radio through Direwolf on 127.0.0.1:8001
```

### station.toml

Everything a station needs to know lives in one TOML file in the station's folder:
`[station]`, `[radio]`, `[internet]`, `[delivery]` and the trusted stations as `[[trust]]`
entries. Every setting has a default, and `hm keygen` writes a starter file listing them
with comments. The key itself stays in `station.key` (readable by you only) and the web
page's access token in `station.token`. Paths in the file are relative to the file.

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
The page has four views:

- **Chat**: conversations by station, like a messenger. Type a callsign to start one;
  Enter sends. Each line shows whether it was delivered, by radio or internet, and whether
  its signature or receipt verified. A queued line can be dropped before delivery. Lines
  from other stations appear as they arrive.
- **Mail**: messages with a subject and precedence, with an inbox and a sent log.
- **Stations**: stations heard on the radio (with their beacons, locators, distance and
  bearing) and the trusted stations, which you can add and remove.
- **Settings**: everything the node applies without a restart.

The page stays up to date by itself: the node tells it what changed over a server-sent
event stream (`/api/events`). A chat line between two stations linked over the internet
arrives within a second; by radio it takes as long as the channel does. Chat and mail are
both stored and forwarded: a line to a station out of reach waits and goes out when a link
comes up. The node keeps every message in `station.db` and retries undelivered ones with
growing delays (1 minute doubling to an hour, 12 attempts, set in `[delivery]`).

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
full-band SNR of −4 dB: 19 arrived as AX.25, all 40 as IL2P; at −6 dB, none against 25.
`framing = "auto"` sends IL2P to stations that said they decode it (every hm station on the
built-in modem does, and says so in its OPEN) and AX.25 to everyone else, beacons included.
The modem always decodes both. Tested against Direwolf both ways: Direwolf decodes all of
our IL2P frames, and we decode more of `gen_packets -I 1` than Direwolf itself (97 against
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
only trusted stations can connect, and each link is bound to a callsign.
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
8104, wait until the three links appear under Stations, then send from A to `SO5KM-1`.
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
| GET | `/api/events` | server-sent events, one `data:` line naming what changed: `message`, `status` or `settings` |
| POST | `/api/send` | `{"to", "text", "subject"?, "precedence"?}` → `201 {"id"}`; without a subject it is a chat line |
| DELETE | `/api/messages/{id}` | cancel a locally queued outbound message → `204`; delivered and inbound messages cannot be cancelled |
| POST | `/api/read/{id}` | mark an inbound message read |
| GET | `/api/trust` | trusted stations with their notes, and the file they are saved to |
| POST | `/api/trust` | `{"line": "SO5KM-1 8a1e…", "note"?}` (as `hm whoami` prints it) → `201` |
| DELETE | `/api/trust/{station}` | stop trusting a station → `204` |
| GET | `/api/settings` | the settings in use: `live` ones, and those that take a restart |
| PATCH | `/api/settings` | any of `beacon_minutes`, `radio_cost`, `internet_cost`, `retry_first_secs`, `retry_max_secs`, `retry_attempts`, `peers` (`[{"station", "address"}]`), `locator` (`""` for none), `radio` (any `[radio]` fields) → the new settings; saved to `station.toml` |

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
| 2 kB over the modem's measured loss at 7 / 8 / 9 dB SNR | 100/100 delivered at each; latency p50 26.7 / 17.5 / 17.5 s | `cargo test -p hm-xfer --release --test sim measured_modem -- --nocapture` |
| Four stations to one hub, 1.5 kB each, 9 dB, all hear each other | without CSMA: last delivery p50 251 s, 4.8 overs per object, 153 receptions lost to collisions per run; with CSMA: p50 126 s, 1.8 overs per object, 44 lost, all from stations keying up within the 125 ms carrier-detect delay of each other | `cargo test -p hm-xfer --release --test sim busy_channel -- --nocapture` |
| Receiver never keys up during an over, 8 kB at 15% loss | 0 frames talked over in 40 runs | `cargo test -p hm-xfer --release --test sim nobody_talks -- --nocapture` |
| Modem frame loss vs SNR, 48 kHz, white noise | 50% of 40-byte frames lost at 5.5 dB, 41% of 360-byte frames at 7 dB, none above 9.5 dB; table in `crates/hm-sim/src/afsk_1200.rs` | `HM_WRITE_CURVE=1 cargo test -p hm-sim --release --test afsk afsk_1200_curve -- --ignored` |
| Modem carrier detect | 68–101 ms after key-up at 7–20 dB SNR | `cargo test -p hm-sim --release --test afsk carrier_detect -- --nocapture` |
| Modem vs Direwolf 1.7, `gen_packets -n 100` at 11–48 kHz | 241 frames decoded vs 236 for Direwolf's better profile (102%) | `cargo test -p hm-modem-afsk --release --test modem -- --nocapture` (needs `direwolf` installed) |
| Modem vs Direwolf 1.7, held out: tilt ±6 dB/octave, SNR down to −4 dB | 322 vs 335 (96%); behind Direwolf deep in the noise | same |
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
