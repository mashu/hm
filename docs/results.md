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
| departures on hearing the next hop | 240 / 240 | 1.5 h / 14.4 h | 6.8 % |
| the open state followed through a transfer, stopping to wait for the peer | 240 / 240 | 1.8 h / 15.2 h | 6.4 % |
| the receipt due after the trip there and back | 240 / 240 | 1.7 h / 16.0 h | 6.2 % |

Where the airtime went in the first run: data 27 %, session opens 19 %,
mostly attempts into closed paths. Now: beacons 3.4 %, data about 2 %,
control and ACKs about 1.5 %.

### As the network grows

`world::scattered`: stations at random in a square, paths by distance, each
station sending 4 messages a day to random others; 4 days, seed 1. "Busy"
is each station's neighbourhood (its own frames and those of the stations it
has a path to), mean and worst; beacon, control and data are seconds on the
air per station per hour.

`cargo run --release -p hm-node --example hf_scale -- 5,10,20 4 800 4 1`
(all within 800 km: every pair has a path at some hours)

| Stations | Delivered | Latency p50 / p90 | Busy, mean / worst | Beacon | Control | Data |
| --- | --- | --- | --- | --- | --- | --- |
| 5 | 40 / 40 | 2.8 h / 12.0 h | 8.4 % / 8.4 % | 23.6 s | 18.7 s | 18.2 s |
| 10 | 80 / 80 | 7.3 h / 17.8 h | 24.5 % / 24.5 % | 16.2 s | 36.1 s | 35.9 s |
| 20 | 158 / 160 | 13.6 h / 32.2 h | 91.5 % / 91.5 % | 7.9 s | 75.9 s | 80.9 s |

`... -- 5,10,20 4 0 4 1` (the square grows with the network, 360 km × √n)

| Stations | Delivered | Latency p50 / p90 | Busy, mean / worst | Beacon | Control | Data |
| --- | --- | --- | --- | --- | --- | --- |
| 5 | 40 / 40 | 3.1 h / 33.2 h | 7.7 % / 7.7 % | 24.2 s | 15.5 s | 15.6 s |
| 10 | 80 / 80 | 9.1 h / 38.1 h | 28.7 % / 31.3 % | 16.8 s | 48.2 s | 47.6 s |
| 20 | 121 / 160 | 17.9 h / 54.5 h | 53.3 % / 82.2 % | 13.6 s | 70.0 s | 73.4 s |

Over this round the ten-station networks went from 27.6 % busy (dense)
and 46.3 % (sparse) to 24.5 % and 28.7 %, and the sparse twenty from 104
delivered at 76 % busy (124 % at the worst station, frames overlapping) to
121 at 53 %: planning that scales (a four-day run of twenty stations took
more than twenty minutes, now three to eight), senders that stop probing a
path gone quiet, and origins that no longer resend what is only slow. (The
dense twenty was measured just before the last of these.) Beacons take less airtime per
station as the network grows.

**How long the transmitter is keyed.** A station's transmitter is on
about 1 % of the time in the Baltic week (some 45 s an hour) and 3 % in the
sparse ten, in many short key-ups: median 4.5–5 s (mostly beacons), 90th
percentile about 8 s, longest about 33 s (a full burst of 16 two-second
frames at 300 bd). The seconds-per-hour columns above are sums over an
hour, not single transmissions; "busy" counts every station a station
hears, not its own transmitter.

### What is still wrong

- **Two-hop horizon.** Beacons tell each station of its two-hop
  neighbourhood only. In the sparse twenty, where most pairs are three to
  five hops apart, stations beyond that guess: they try the destination
  directly or a relay that might hear it, and the 39 messages not delivered
  were each tried by up to eight stations, none of which could reach the
  destination. What is missing is a bounded summary of who reaches whom,
  spread within the control budget (a probabilistic distance vector).
- **A dense channel of twenty.** Twenty stations sharing one 300 bd channel
  keep it busy 90 % of the time: about as much data airtime is lost to
  fades and collisions as gets through. Carrier sense persistence is fixed
  (p = 0.25), where the number of stations contending is known and could
  set it.
- **300 bd AFSK is a poor HF mode.** It has no forward error correction of
  its own and its symbols are short against multipath. The simulated
  numbers are for it because it can be simulated; on the air, HF traffic
  belongs on an ARQ modem (VARA HF, ARDOP), which hm-net supports as a
  bearer ([modems](operating/modems.md)).
- **Key-ups up to half a minute.** Bursts of 16 frames and frames queued
  behind one another can keep the transmitter on for 30 s at 300 bd; a cap
  on a single key-up, with a rest after it, belongs in the radio layer.
- **One symbol size for every path.** 32-byte symbols suit fading paths and
  are slower on good ones (see below); the size should follow each path's
  loss belief ([transfer](transfer.md#frame-and-symbol-sizes)).
- **Receipts are messages.** An end-to-end receipt is routed like any
  message, and on a busy HF network can take hours to reach the origin.

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
