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
| beacons naming the key by an 8-byte id, key-ups of at most 20 s | 240 / 240 | 1.3 h / 15.9 h | 6.2 % |
| handoff chances checked against how handoffs ended | 240 / 240 | 1.4 h / 15.4 h | 6.2 % |

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
| 5 | 40 / 40 | 2.3 h / 11.6 h | 7.6 % / 7.6 % | 22.2 s | 16.6 s | 16.0 s |
| 10 | 80 / 80 | 5.6 h / 19.5 h | 23.9 % / 23.9 % | 15.6 s | 35.7 s | 34.7 s |
| 20 | 158 / 160 | 8.9 h / 34.3 h | 77.6 % / 77.6 % | 8.2 s | 64.3 s | 67.1 s |

`... -- 5,10,20 4 0 4 1` (the square grows with the network, 360 km × √n)

| Stations | Delivered | Latency p50 / p90 | Busy, mean / worst | Beacon | Control | Data |
| --- | --- | --- | --- | --- | --- | --- |
| 5 | 40 / 40 | 3.4 h / 17.8 h | 7.6 % / 7.6 % | 22.4 s | 16.3 s | 16.3 s |
| 10 | 80 / 80 | 7.7 h / 29.6 h | 27.0 % / 29.4 % | 16.7 s | 45.1 s | 44.1 s |
| 20 | 119 / 160 | 12.9 h / 49.8 h | 45.2 % / 68.4 % | 14.1 s | 55.8 s | 57.3 s |

Checking the handoff chances against how handoffs ended (see below) took
the dense twenty from 94.3 % busy to 77.6 % and the sparse twenty from
65.2 % to 45.2 %, with the worst neighbourhood from 99.3 % to 68.4 %; the
smaller networks, which had little to correct, stayed where they were. One
seed of twenty stations varies a lot, so the sparse twenty was run on three:

| Sparse twenty, seeds 1 / 2 / 3 | Delivered | Busy, mean | Latency p50 |
| --- | --- | --- | --- |
| before | 127 / 134 / 141 (402) | 65.2 / 69.5 / 66.9 % | 19.6 / 11.7 / 12.6 h |
| handoff chances checked | 119 / 141 / 142 (402) | 45.2 / 60.5 / 57.8 % | 12.9 / 9.2 / 12.3 h |

The same messages delivered, sooner, with a fifth less of the channel.
Earlier in this round the ten-station networks went from 27.6 % busy
(dense) and 46.3 % (sparse) to about 24 % and 30 %, and the sparse twenty
from 104 delivered to 127: planning that scales (a four-day run of twenty
stations took more than twenty minutes, now four to eight), senders that
stop probing a path gone quiet, and origins that no longer resend what is
only slow. Capping key-ups at 20 s costs airtime where transfers are long
and paths poor (the twenties: more, shorter overs, each with its own key-up
and answer). Beacons take less airtime per station as the network grows.

**How long the transmitter is keyed.** A station's transmitter is on
about 1 % of the time in the Baltic week (some 45 s an hour) and 3 % in the
sparse ten, in many short key-ups: median 3.8 s (mostly beacons), 90th
percentile 7.7 s, longest about 22 s with the default 20 s cap on a key-up
(it was about 35 s before the cap: a full burst of 16 two-second frames at
300 bd, or overs, answers and beacons running on one after another). The
seconds-per-hour columns above are sums over an hour, not single
transmissions; "busy" counts every station a station hears, not its own
transmitter.

### Checking the chances

A station's handoff chances were overconfident, and at the low end badly.
Logging, in the sparse twenty (seed 1), the chance the beliefs gave each
radio handoff against whether the path carried it, and marking the pairs
the simulated world has no path between at all:

| Radio handoffs | Before: chance given | carried | After: chance given | carried |
| --- | --- | --- | --- | --- |
| to a station heard before | 45.8 % | 36.7 % | 40.7 % | 38.7 % |
| to a station never heard, with a path | 6.0 % | 0.2 % | 3.7 % | 1.4 % |
| to a station never heard, no path | 4.9 % | 0 | 2.7 % | 0 |

Of 10 360 radio handoffs over the four days, 3 390 went to stations that
could not be reached at all, a few dozen tries each over days; after, 1 049
of 5 415. The path model explained each failure as the path being closed
just then, and between tries the chance crept back up toward the daily
pattern; an exact model with the same one-bump prior would have done much
the same. Most such tries came after ten failures or more on the same pair,
a fifth of them from Thompson draws made afresh every quarter of an hour.
A station makes only a few dozen handoffs a day to stations it has never
heard, so its record of them is still short after four days: the chances it
gives them are still above what comes true, but by a factor of seven rather
than seventy.

So chances are now checked against the station's own record, separately for
paths seen open and paths only inferred ([models](models.md#checking-the-chances)).
What was tried on the way there:

| Sparse twenty, seed 1 | Delivered | Busy |
| --- | --- | --- |
| before | 127 / 160 | 65.2 % |
| one logistic map (Platt scaling) for all handoffs | 89 / 160 | 30.7 % |
| a logistic map per kind, seen and not | 118 / 160 | 40.8 % |
| bins per kind | 139 / 160 | 54.1 % |
| bins per kind, shrunk toward each other (kept) | 119 / 160 | 45.2 % |
| the same, custodians' retries counted in a route's chance | 107 / 160 | 62.1 % |

One map for all handoffs could not tell a dead pair from a weak live one and
cut both; a logistic map per kind stretched the seen paths' chances to 99.9 %
(68 % carried) at the top. Bins correct each band of chances by its own
record; shrinking them toward each other lets the few dozen handoffs a day a
station makes to paths never heard teach every band at once (without it,
a band given 3.6 % came down only to 1.7 %, where 0.2 % came true). Over
three seeds the kept version delivers what the model did before on a fifth
less of the channel (above).

**Custodians' retries in a route's chance.** A route counts each hop past
the first as one try, though each custodian tries again until the message
expires. Counting the tries (the chance a custodian has handed the message
on by each departure, tries less than an hour apart counted as correlated)
made nearly every link over a day and a half likely, so the bound on what a
route through a station can be worth stopped pruning: planning took three
times as long and ran into the search limit, and fewer messages moved. Not
kept.

### What is still wrong

- **Where the sparse twenty loses messages.** Every message not delivered
  had a path, of one or two hops, within the two-hop neighbourhood each
  station learns from beacons: knowing the topology is not what is missing.
  The paths are long (600 to 1 000 km, open a fifth of the day and three
  fifths of the night) and 300 bd frames seldom survive them: one station
  decoded about 5 % of its neighbours' beacons over two days, and none of one
  neighbour's. Such a path is little better than none at 300 bd.
- **What telling more did.** Beacons tell each station of its two-hop
  neighbourhood only. We tried sampled reach entries: each beacon carrying
  up to three "I reach D with chance p in about t" statements from the
  station's own models, weighted by chance and by how long since it last
  told them, within the bytes the key id freed, taken by listeners as
  stated probabilities. Over three seeds of the sparse twenty it delivered
  372 of 480 against 396 without, at 54 % busy against 58 %: messages moved
  toward the stations that claimed a way and then stalled at them. Knowing
  more was not what was missing, so it was not kept.
- **A dense channel of twenty.** Twenty stations sharing one 300 bd channel
  keep it busy 78 % of the time. Carrier sense persistence is fixed
  (p = 0.25), where the number of stations contending is known and could
  set it.
- **300 bd AFSK is a poor HF mode.** It has no forward error correction of
  its own and its symbols are short against multipath. The simulated
  numbers are for it because it can be simulated; on the air, HF traffic
  belongs on an ARQ modem (VARA HF, ARDOP), which hm-net supports as a
  bearer ([modems](operating/modems.md)).
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
