# Simulation

Every claim about how hm-net behaves on the air is measured in a
deterministic simulator before it is tried on the air. `hm-sim` runs the real
state machines (the transfer engine, whole stations) on simulated radio
channels; same seed, same run, on every platform.

## The simulated channel

`hm-sim` models the physics of one or more radio channels:

- **Channels and ports.** Each channel has its own bitrate and TXDELAY; a
  station attaches radios (ports) to channels. Channels never interfere.
- **Airtime.** `txdelay + txtail + ((frame + phy overhead) × 8 + stuffed bits) / bitrate`.
  Frames queued back to back follow without a new key-up. The AX.25 UI
  framing, frame check, flags and bit stuffing of the built-in modem are
  counted, within 0.02 % of the modulator's airtime.
- **Half-duplex.** A radio that is transmitting hears nothing on its channel.
- **Collisions.** Two transmissions overlapping at a receiver destroy each
  other, hidden terminals included.
- **Loss.** Per directed link: Bernoulli, Gilbert–Elliott bursts, an hourly
  table for band openings, the built-in modem's measured loss by SNR and
  frame length, or that modem under flat HF fading: Watterson-style, a
  Gaussian Doppler spectrum (CCIR 520 spreads of 0.1–1 Hz), the same fade
  both ways, each frame judged at the weakest SNR it meets.
- **Channel access.** Off (the radio keys up when asked) or p-persistent
  CSMA on carrier detect, after a measured detection delay, from stations the
  radio can hear only.
- **Faults.** Stations down and up, links cut and restored, clock offset and
  drift, frames delivered with undetected bit errors.

An independent oracle (`crates/hm-sim/tests/oracle.rs`) replays the
simulator's log through channel rules written separately, bit stuffing and
carrier sense included, for thousands of random networks.

## Whole stations for days

`hm_node::Station` is a whole station: its decisions (`Node`) and its radio
side (`Radio`, with the transfer engine, beacons and control plane) as one
machine, with a store in memory. `crates/hm-node/tests/support/world.rs`
puts stations on a simulated HF channel:

- each path between two stations opens and closes through the day: in each
  UTC hour it is open with that hour's probability, and tends to stay as it
  was (openings last);
- while open it fades (Watterson, per path);
- messages are queued at random times between random stations; many pairs
  have no direct path and need relays;
- an **oracle** that knows the opening schedule finds each message's
  earliest possible arrival over it (paths that open later, relays that
  hold), with no airtime limit. "Behind the oracle" is how much later than
  that a message arrived.

Two scenarios:

- `world::baltic`: five stations around the Baltic. Near paths open by day
  most days (NVIS), long ones mostly at night; two pairs never hear each
  other.
- `world::scattered(n, side, per_station, days, seed)`: `n` stations at
  random over a square `side` km wide; whether two hear each other, and when,
  follows their distance (NVIS within 250 km, some of the time up to 600 km,
  mostly at night up to 1,000 km, never beyond). Each station sends the same
  number of messages a day, so the load each station offers stays the same
  as the network grows.

## Running them

```sh
# A week of the Baltic: 4 seeds, nodes taking stock every 10 s.
cargo run --release -p hm-node --example hf_days -- 7 4 10

# Networks of 5 to 40 stations, 4 days, in an 800 km square
# (0 for a square that grows with the network).
cargo run --release -p hm-node --example hf_scale -- 5,10,20,40 4 800 4 1

# One station's log lines (anything containing the text):
HM_LOG=LA1CCC: cargo run --release -p hm-node --example hf_days -- 7 1 10

# Every frame one station sends another, seconds into the run:
HM_TRACE='LA1CCC>ES1EEE' cargo run --release -p hm-node --example hf_days -- 7 1 10
```

What they print:

- **delivered**, of those sent and of those possible at all;
- **latency** and **behind the oracle**, median and 90th percentile;
- **channel busy**: the share of time any station transmits (`hf_days`), or
  each station's neighbourhood: its own frames and those of the stations it
  has a path to, mean and worst (`hf_scale`);
- **airtime by frame kind** (beacons, control, data), and **by fate** at
  the station a frame was for: delivered, lost to a fade (`LostChannel`),
  to a collision, to the receiver's own transmitter, or `Unheard` (no path
  to it then).

Two short scenarios run in CI (`crates/hm-node/tests/days.rs`): a station
reaching one it never hears through a neighbour, and reproducibility (the
same seed gives the same run).

## Routing baselines

`hm-sim::routing` compares the planner with epidemic routing, Spray-and-Wait,
a PRoPHET-like scheme and MEED on the same contact traces
(`cargo run --release -p hm-sim --example routing_compare`).
