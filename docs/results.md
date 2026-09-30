# Results

Everything here is measured in the simulator ([simulation](simulation.md)),
and each row says how to reproduce it. On-air trials are next
([plan](on-air-test-plan.md)).

## Whole stations on HF

Real stations (decisions, radio machine, transfer engine, beacons, control
plane, a store each) on simulated 300 bd HF whose paths open and close
through the day and fade while open.

### A week around the Baltic

Five stations, 12 messages a day between random pairs, two pairs that never
hear each other; 7 days, 4 seeds, 240 messages, all deliverable over the
openings. `cargo run --release -p hm-node --example hf_days -- 7 4 10`

| Version | Delivered | Behind the oracle, p50 / p90 | Channel busy |
| --- | --- | --- | --- |
| first whole-station run | 172 / 240 | | |
| beliefs per path, misses of unheard stations | 216 / 240 | 1 h / 30 h | 51 % |
| forecast departures, priced airtime, silence stops a transfer | 213 / 240 | 2.1 h / 31.5 h | 16.9 % |
| reach, holding when nothing is worth it, censored custody evidence, receipts not resent | 239 / 240 | 2.7 h / 26.6 h | 7.7 % |
| receding horizon, relays around the origin | 240 / 240 | 1.7 h / 16.1 h | 8.3 % |
| relays release at handoff, expected airtime, 32-byte HF symbols | 240 / 240 | 2.1 h / 15.1 h | 7.4 % |
| relays plan on the mean | 240 / 240 | 1.7 h / 15.9 h | 6.6 % |

Where the airtime went in the first run: data 27 %, session opens 19 %,
mostly attempts into closed paths. Now: beacons 3.4 %, data about 2 %,
control and ACKs about 1.5 %.

### As the network grows

`world::scattered`: stations at random in a square, paths by distance, each
station sending 4 messages a day to random others; 4 days, seed 1. "Busy"
is each station's neighbourhood (its own frames and those of the stations it
has a path to), mean and worst; beacon, control and data are seconds on the
air per station per hour.

`cargo run --release -p hm-node --example hf_scale -- 5,10,20,40 4 800 4 1`
(all within 800 km: every pair has a path at some hours)

| Stations | Delivered | Latency p50 / p90 | Busy, mean / worst | Beacon | Control | Data |
| --- | --- | --- | --- | --- | --- | --- |
| 5 | 40 / 40 | 3.0 h / 7.6 h | 7.8 % / 7.8 % | 23.9 s | 16.7 s | 15.9 s |
| 10 | 80 / 80 | 3.8 h / 14.8 h | 27.6 % / 27.6 % | 16.1 s | 41.8 s | 41.5 s |

`... -- 5,10,20,40 4 0 4 1` (the square grows with the network, 360 km × √n)

| Stations | Delivered | Latency p50 / p90 | Busy, mean / worst | Beacon | Control | Data |
| --- | --- | --- | --- | --- | --- | --- |
| 5 | 40 / 40 | 2.8 h / 13.7 h | 8.2 % / 8.2 % | 24.1 s | 17.8 s | 17.1 s |
| 10 | 78 / 80 | 7.3 h / 37.6 h | 46.3 % / 51.3 % | 16.0 s | 83.7 s | 85.1 s |

The ten-station dense network went from 78 % busy to 28 % over the changes
of this round: relays stopped resending what was delivered (their timers
could only fire on messages whose receipt went back to the origin, not
through them), stopped copying each other's holdings over the radio, and
stopped passing messages from relay to relay on the strength of a lucky
draw. Beacons take less airtime per station as the network grows.

### What is still wrong

- **Long, lossy paths.** In the sparse network 57 % of attempts end in no
  answer, and fading loses more data airtime (10 % of the channel) than gets
  through (6.8 %). Paths of 600–1,000 km at 12 ± 3 dB and 1 Hz Doppler are
  hard at 300 bd; the handoff estimates on them run optimistic.
- **One symbol size for every path.** 32-byte symbols suit fading paths and
  are slower on good ones (see below); the size should follow each path's
  loss belief ([transfer](transfer.md#frame-and-symbol-sizes)).
- **Receipts are messages.** An end-to-end receipt is routed like any
  message, and on a busy HF network can take hours to reach the origin,
  whose timer may fire first.
- **Two-hop horizon.** Beacons tell each station of its two-hop
  neighbourhood only; beyond that it guesses, through relays that may know
  more.

## Transfers

Rows are VHF packet at 1200 bd with 300 ms TXDELAY unless they say HF. The
simulated channel sends frames as the built-in modem does: AX.25 UI header,
frame check, flags and bit stuffing included. SNR is in a 3 kHz bandwidth.
HF rows run at 300 bd over a fading path.

| Scenario | Result | Reproduce |
| --- | --- | --- |
| 150-byte chat, one hop | arrives p50 2.5 s clean, 2.5 s (p95 6.5 s) at 10 % loss, 5.7 s (p95 22.8 s) at 30 %; HF at 0.5 Hz fading: receipt back p50 77 / 39 / 22 s at 17 / 20 / 24 dB | `cargo test -p hm-xfer --release --test sim chat_latency -- --ignored --nocapture` |
| 1 kB, 10 % frame loss both ways, 10,000 trials | 100 % delivered, 0 duplicates, every receipt verified; p50 9.3 s, p95 23.9 s | `HM_XFER_TRIALS=10000 cargo test -p hm-xfer --release --test sim exit_criterion_1kb -- --nocapture` |
| HF: 2 kB, 0.5 Hz Doppler spread, 20 dB | 20/20 delivered, p50 271 s, 198 s of airtime (with 64-byte symbols 199 s and 166 s; with VHF sizes 365 s and 238 s) | `cargo test -p hm-xfer --release --test sim hf_link_sizing -- --nocapture` |
| 2 kB bulletin to 10 listeners, each losing 25 % | 99 % of listeners with repair requests (about six in all), 72 % with one publish; 37 % more airtime | `cargo test -p hm-xfer --release --test sim bulletin_repair -- --nocapture` |
| 5 kB, clean link | 72.6 % of airtime is payload; hm headers and preambles 9.6 %, OPEN, OFFER and ACK with receipt 2.2 %, TXDELAY and TXTAIL 2.7 %, AX.25 framing and bit stuffing 9.9 % | `cargo test -p hm-xfer --release --test sim exit_criterion_overhead -- --nocapture` |
| 2 kB, bursty loss (Gilbert–Elliott, about 12 % mean) | 100/100 delivered | `cargo test -p hm-xfer --release --test sim bursty -- --nocapture` |
| Two hidden senders to one node, no CSMA | 30/30 both delivered, last p50 93 s, max 143 s | `cargo test -p hm-xfer --release --test sim two_senders -- --nocapture` |
| 2 kB over the modem's measured loss at 7 / 8 / 9 dB | 100/100 delivered at each; p50 25.1 / 17.5 / 17.5 s | `cargo test -p hm-xfer --release --test sim measured_modem -- --nocapture` |
| Four stations to one hub, 1.5 kB each, 9 dB, all hear each other | without CSMA: last p50 205 s, 5.4 overs per object, 136 receptions lost to collisions; with CSMA: 72 s, 1.8 overs, 39 lost | `cargo test -p hm-xfer --release --test sim busy_channel -- --nocapture` |
| Receiver never keys up during an over, 8 kB at 15 % loss | 0 frames talked over in 40 runs | `cargo test -p hm-xfer --release --test sim nobody_talks -- --nocapture` |
| 4 kB, 5 % undetected frame corruption | 40/40 delivered, none corrupt | `cargo test -p hm-xfer --release --test sim heavy_corruption -- --nocapture` |

## Control traffic

| Scenario | Result | Reproduce |
| --- | --- | --- |
| Beacons, contact adverts and holdings pulls of 5–40 stations sharing one channel | 2.3–3.7 % of a VHF channel and 3.1–4.2 % of an HF one, whatever the number of stations; before this design 10–75 % of VHF and more than all of HF at 40 stations | `cargo test -p hm-cli --release --test control_load -- --ignored --nocapture` |

## The built-in modem

| Scenario | Result | Reproduce |
| --- | --- | --- |
| Frame loss vs SNR, 48 kHz, white noise | 38 % of 40-byte frames lost at 5.5 dB, 40 % of 360-byte frames at 7 dB, none at or above 9.5 dB; table in `crates/hm-sim/src/afsk_1200.rs` | `HM_WRITE_CURVE=1 cargo test -p hm-sim --release --test afsk afsk_1200_curve -- --ignored` |
| Carrier detect | 68–101 ms after key-up at 7–20 dB | `cargo test -p hm-sim --release --test afsk carrier_detect -- --nocapture` |
| vs Direwolf 1.8.1, `gen_packets -n 100` at 11–48 kHz | 246 frames decoded vs 236 for Direwolf's better profile (104 %) | `cargo test -p hm-modem-afsk --release --test modem -- --nocapture` (needs `direwolf`) |
| vs Direwolf, held out: tilt ±6 dB/octave, SNR down to −4 dB | 333 vs 328 (102 %) | same |
| Interoperability | Direwolf decodes 50/50 of our frames, clean and at 12 dB | same |
