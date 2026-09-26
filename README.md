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
| `hm-sim` | Discrete-event simulator: multiple channels and radios per station, airtime with keyed-PTT bursts, half-duplex, hidden-terminal collisions, Bernoulli / Gilbert–Elliott / hourly HF loss, outages, partitions, clock drift, corrupted frames, airtime split by purpose, delivery and latency metrics |
| `hm-bearer` | KISS framing (streaming decoder) and AX.25 UI encapsulation for Direwolf and hardware TNCs |
| `hm-xfer` | Fountain-coded (RaptorQ) transfer engine: OFFER + symbol bursts, ACKs with the missing count, hash-verified delivery, signed delivery receipts, duplicate suppression, per-sender resource limits, loss-adaptive burst sizing, random exponential backoff, airtime budget |
| `hm-store` | Persistent store on `redb` (pure Rust, crash-safe): content-addressed messages, inbox, outbox ordered by precedence, retries with exponential backoff, exactly-once across restarts |
| `hm-net` | Internet links: QUIC with mutual TLS 1.3 on Ed25519 station keys (only trusted stations connect), automatic redial, one stream per bundle with a signed receipt |
| `hm-modem-afsk` | Our own AFSK 1200 modem (Bell 202), pure Rust and `no_std`: HDLC framing and CRC, multi-slicer demodulator with per-tone AGC and PLL clock recovery, carrier detect |
| `hm-rig` | Radio hardware: sound cards through `cpal` (ALSA, CoreAudio, WASAPI); PTT by rigctld (Hamlib CAT), serial RTS/DTR, CM108 GPIO (AIOC, Digirig) or VOX; a virtual radio channel for tests |
| `hm-cli` | The `hm` command: `node` (the station daemon: radio and/or internet links, web interface, JSON API with access token), `keygen`, `whoami`, `send`, `listen`; a KISS-over-TCP link and a real-time driver |

Phase 1 still to do: an on-air test, IL2P framing and better decoding deep in noise for
the built-in modem, stream modems (Mercury, ARDOP, VARA) as radio bearers, KISS over serial,
CTRL session open/close, and the Dioxus interface with a setup wizard. Relaying mail through
other nodes is Phase 2.

## Run a station

```sh
hm keygen --call SA0KAM
hm node --ssid 1 --trust trusted.txt      # radio through Direwolf on 127.0.0.1:8001
```

`hm node` prints a link such as `http://127.0.0.1:8080/#token=…`. Open it for the inbox,
the sent log with each message's delivery state, and a form to queue messages. The token
is also kept beside the store (`station.token`); the page asks for it if you open the
plain address. The node keeps every message in `station.db` and retries undelivered mail
with growing delays (1 minute doubling to an hour, 12 attempts).

### The built-in modem

Without Direwolf, the node runs its own AFSK 1200 modem on a sound card:

```sh
hm audio-devices                                   # list sound cards
hm node --trust trusted.txt --audio default --ptt vox
hm node --trust trusted.txt --audio "USB Audio" --ptt cm108:/dev/hidraw0     # AIOC or Digirig
hm node --trust trusted.txt --audio "USB Audio" --ptt rigctld              # CAT through Hamlib's rigctld
hm node --trust trusted.txt --audio "USB Audio" --ptt rts:/dev/ttyUSB0
```

It sends the same AX.25 UI frames as the KISS path, so stations on the built-in modem and
stations on Direwolf work together. It waits for a clear channel (p-persistent CSMA on its
carrier detect: `--persist`, `--slottime`), sends each transfer burst in one key-up, and
releases PTT on every exit path.

### Radio, internet, or both

A node can reach other stations by radio, over the internet, or both:

```sh
# A home station with radio that also keeps an internet link to a server
hm node --trust trusted.txt --peer SO5KM=hm.example.org:4433

# A server without a radio that trusted stations connect to
hm node --no-radio --trust trusted.txt --listen 0.0.0.0:4433
```

Internet links are QUIC connections authenticated with the station keys themselves:
only stations in the trust file can connect, and each link is bound to a callsign.
There is no certificate authority and no central server; any node can listen, dial, or both.

For each message the node picks the link with the lowest expected cost: the link's
cost (`--radio-cost 1`, `--internet-cost 2` by default) divided by how reliably it has
delivered to that station lately. So mail goes by radio while radio delivers; if radio
keeps failing, the internet carries it until radio works again. The sent log shows which
link delivered each message. Mail to a station you have no working link to waits in
the queue; passing mail on through other nodes comes in Phase 2.

### The API

| Method | Path | |
| --- | --- | --- |
| GET | `/api/status` | callsign, key, radio and internet state, estimated delivery rate per station and link |
| GET | `/api/messages?direction=in\|out&limit=n` | newest first, with delivery state and link |
| POST | `/api/send` | `{"to", "text", "subject"?, "precedence"?}` → `201 {"id"}` |
| POST | `/api/read/{id}` | mark an inbound message read |

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
   hm whoami >> trusted.txt   # collect the other station's line the same way
   ```

3. Receiving station:

   ```sh
   hm listen --ssid 1 --trust trusted.txt
   ```

4. Sending station:

   ```sh
   hm send --to SO5KM-1 --text "73 de SA0KAM"
   hm send --to SO5KM-1 --subject "Sked" --text "40m 7.047 at 19Z?" --precedence priority
   ```

On this path every hm frame is an AX.25 UI frame from your callsign to `HMNET`, so each
transmission carries your station identification, and Direwolf's own CSMA (`PERSIST`,
`SLOTTIME`) handles channel access. Callsigns must fit AX.25: at most 6 letters or digits
plus SSID 0–15. If transfers time out on a slow or busy channel, raise `--guard`; if your
TNC's key-up delay differs from 300 ms, set `--txdelay` to match.

## Measured (simulator, 1200 bd, 300 ms TXDELAY)

| Scenario | Result | Reproduce |
| --- | --- | --- |
| 1 kB, 10% frame loss both ways, 10,000 trials | 100% delivered, 0 duplicates, every receipt verified; latency p50 8.1 s, p95 21.2 s | `HM_XFER_TRIALS=10000 cargo test -p hm-xfer --release --test sim exit_criterion_1kb -- --nocapture` |
| 5 kB, clean link | headers and preambles 10.2%, OFFER and ACK with receipt 2.1%, TXDELAY 2.9%; 81.5% of airtime is useful payload | `cargo test -p hm-xfer --release --test sim exit_criterion_overhead -- --nocapture` |
| 2 kB, bursty loss (Gilbert–Elliott, ~12% mean) | 100/100 delivered | `cargo test -p hm-xfer --release --test sim bursty -- --nocapture` |
| Two hidden senders to one node, no CSMA | 30/30 both delivered, last within 153 s | `cargo test -p hm-xfer --release --test sim two_senders -- --nocapture` |
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

| What | How | Every push | Nightly |
| --- | --- | --- | --- |
| Test vectors | `cargo run -q -p hm-bundle --example vectors \| python3 tools/check_vectors.py` (needs `pip install blake3 pynacl cbor2`) | yes | |
| Simulator vs independent oracle | `HM_SEEDS=10000 cargo test -p hm-sim --release --test oracle` | 100 seeds | 10,000 seeds |
| Decoder mutations (no panic, no forgery) | `HM_MUTATIONS=1000000 cargo test -p hm-bundle --release --test mutations` | 5,000 | 1,000,000 |
| Coverage-guided fuzzing | `cd fuzz && cargo +nightly fuzz run <target>`; targets: frame, ack, envelope, bundle, binding, callsign, kiss, ax25, xfer | compile only | 10 min per target |
| Cross-platform determinism | pinned trace hash of a reference simulation | Linux, Windows, macOS | |
| End to end over TCP | fake KISS TNC relaying frames (with drops and APRS noise) between `hm listen` and `hm send` processes | yes | |
| Built-in modem link | stations on a virtual radio channel in real time: carrier sense defers to a busy channel, PTT only around transmissions, one key-up per burst, two nodes exchanging mail | yes | |
| Internet links | QUIC stations on localhost: delivery with verified receipts, rejection, impostors and wrong server keys refused, redial after restart | yes | |
| Station nodes | `hm node` instances driven only through the HTTP API: radio delivery, restart, store-and-forward, internet-only nodes, radio failing over to the internet | yes | |

The oracle test builds random two-channel networks with every fault type and
replays the simulator's log through channel rules written independently of the
simulator. The mutation test checks that any mutated message which still
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
