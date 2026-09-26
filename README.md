# hm-net

A federated store-and-forward network for amateur radio mail, chat, forms and
bulletins over VHF/UHF and HF. Written in Rust; no central server; content in
clear, identities signed.

`hm` is a working prefix until the project has a name.

## Status: Phase 0 complete, Phase 1 in progress

| Crate | What it does |
| --- | --- |
| `hm-core` | `Millis`, the sans-IO `Machine` trait, station `Input`/`Output` with radio ports, deterministic RNG |
| `hm-wire` | Base-40 callsigns, 18-byte frame header, ACK payload, object ids |
| `hm-ident` | Ed25519 identities, signed envelopes, callsign binding records, attestations |
| `hm-bundle` | Messages: build, seal, open, verify; receipts; attachment references |
| `hm-sim` | Discrete-event simulator: multiple channels and radios per station, airtime with keyed-PTT bursts and AX.25/HDLC framing checked against the modulator, half-duplex, p-persistent CSMA with a measured carrier-detect delay, hidden-terminal collisions, Bernoulli / Gilbert–Elliott / hourly HF loss or the built-in modem's measured loss by SNR and frame length, outages, partitions, clock drift, corrupted frames, airtime split by purpose, delivery and latency metrics |
| `hm-bearer` | KISS framing (streaming decoder) and AX.25 UI encapsulation for Direwolf and hardware TNCs |
| `hm-xfer` | Fountain-coded (RaptorQ) transfer engine: OFFER + symbol bursts, ACKs with the missing count, hash-verified delivery, signed delivery receipts, duplicate suppression, per-sender resource limits, loss-adaptive burst sizing, random exponential backoff, airtime budget |
| `hm-store` | Persistent store on `redb` (pure Rust, crash-safe): content-addressed messages, inbox, outbox ordered by precedence, retries with exponential backoff, exactly-once across restarts |
| `hm-net` | Internet links: QUIC with mutual TLS 1.3 on Ed25519 station keys (only trusted stations connect), automatic redial, one stream per bundle with a signed receipt |
| `hm-modem-afsk` | Our own AFSK 1200 modem (Bell 202), pure Rust and `no_std`: HDLC framing and CRC, multi-slicer demodulator with per-tone AGC and PLL clock recovery, carrier detect |
| `hm-rig` | Radio hardware: sound cards through `cpal` (ALSA, CoreAudio, WASAPI); PTT by rigctld (Hamlib CAT), serial RTS/DTR, CM108 GPIO (AIOC, Digirig) or VOX; a virtual radio channel for tests |
| `hm-cli` | The `hm` command: `node` (the station daemon: radio and/or internet links, web interface, JSON API with access token, settings applied live), `keygen`, `whoami`, `trust`, `send`, `listen`, all set up by one `station.toml`; KISS links over TCP or a serial port, and a real-time driver |

Phase 1 still to do: an on-air test, IL2P framing and better decoding deep in noise for
the built-in modem, stream modems (Mercury, ARDOP, VARA) as radio bearers,
CTRL session open/close, and the Dioxus interface with a setup wizard. Relaying mail through
other nodes is Phase 2.

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
  its signature or receipt verified. Lines from other stations appear as they arrive.
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

For each message the node picks the link with the lowest expected cost: the link's
cost (`radio_cost = 1`, `internet_cost = 2` by default) divided by how reliably it has
delivered to that station lately. So mail goes by radio while radio delivers; if radio
keeps failing, the internet carries it until radio works again. The sent log shows which
link delivered each message. Mail to a station you have no working link to waits in
the queue; passing mail on through other nodes comes in Phase 2.

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
| 1 kB, 10% frame loss both ways, 10,000 trials | 100% delivered, 0 duplicates, every receipt verified; latency p50 9.0 s, p95 23.0 s | `HM_XFER_TRIALS=10000 cargo test -p hm-xfer --release --test sim exit_criterion_1kb -- --nocapture` |
| 5 kB, clean link | hm headers and preambles 9.2%, OFFER and ACK with receipt 1.9%, TXDELAY and TXTAIL 2.8%; AX.25 framing and bit stuffing 9.5%; 73.7% of airtime is useful payload | `cargo test -p hm-xfer --release --test sim exit_criterion_overhead -- --nocapture` |
| 2 kB, bursty loss (Gilbert–Elliott, ~12% mean) | 100/100 delivered | `cargo test -p hm-xfer --release --test sim bursty -- --nocapture` |
| Two hidden senders to one node, no CSMA | 30/30 both delivered, last within 199 s | `cargo test -p hm-xfer --release --test sim two_senders -- --nocapture` |
| 2 kB over the modem's measured loss at 7 / 8 / 9 dB SNR | 100/100 delivered at each; latency p50 26.1 / 17.2 / 17.1 s | `cargo test -p hm-xfer --release --test sim measured_modem -- --nocapture` |
| Four stations to one hub, 1.5 kB each, 9 dB, all hear each other | without CSMA: last delivery p50 231 s, 4.8 overs per object, 124 receptions lost to collisions per run; with CSMA: p50 131 s, 1.9 overs per object, 47 lost, all from stations keying up within the 125 ms carrier-detect delay of each other | `cargo test -p hm-xfer --release --test sim busy_channel -- --nocapture` |
| Receiver never keys up during an over, 8 kB at 15% loss | 0 frames talked over in 40 runs | `cargo test -p hm-xfer --release --test sim nobody_talks -- --nocapture` |
| Modem frame loss vs SNR, 48 kHz, white noise | 50% of 40-byte frames lost at 5.5 dB, 41% of 360-byte frames at 7 dB, none above 9.5 dB; table in `crates/hm-sim/src/afsk_1200.rs` | `HM_WRITE_CURVE=1 cargo test -p hm-sim --release --test afsk afsk_1200_curve -- --ignored` |
| Modem carrier detect | 68–101 ms after key-up at 7–20 dB SNR | `cargo test -p hm-sim --release --test afsk carrier_detect -- --nocapture` |
| Modem vs Direwolf 1.7, `gen_packets -n 100` at 11–48 kHz | 241 frames decoded vs 236 for Direwolf's better profile (102%) | `cargo test -p hm-modem-afsk --release --test modem -- --nocapture` (needs `direwolf` installed) |
| Modem vs Direwolf 1.7, held out: tilt ±6 dB/octave, SNR down to −4 dB | 322 vs 335 (96%); behind Direwolf deep in the noise | same |
| Modem interop | Direwolf decodes 50/50 of our frames, clean and at 12 dB SNR | same |
| 5% undetected frame corruption | 38/40 delivered, none corrupt | `cargo test -p hm-xfer --release --test sim heavy_corruption -- --nocapture` |

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
