# hm-net

**hm-net** is experimental station software and a research protocol for
delay-tolerant amateur-radio messaging: mail, chat, forms and group bulletins
over HF and VHF/UHF packet, ARQ modems (VARA, Mercury, ARDOP) and
authenticated internet links. It is a **federated** network of peer
stations: every operator runs their own node and chooses whose keys to
trust. There is no central server. Content is in the clear, as amateur rules
require; every identity is authenticated with Ed25519.

`hm` is the command's working name until the project has a settled one.

## What it does differently

A shared radio channel is scarce, half-duplex, lossy, and open only some of
the time. hm-net treats airtime as the resource that matters:

- **Store and forward with custody.** A message moves one hop at a time,
  handed on only against a signed receipt; a path that is closed now is
  waited for, or gone around. The origin marks a message *Delivered* only on
  the destination's signed end-to-end receipt.
- **Beliefs, not snapshots.** Each station learns, by Bayes' rule, whether
  each path is within reach, when in the day it opens, how many frames it
  loses, and how well each custodian delivers, from beacons heard and
  missed, acknowledgements, and receipts that came back or did not; and it
  checks the chances it gives against how its handoffs ended, correcting
  them where its models err.
- **Decisions by expected utility.** Send now, wait for an opening, or go
  through a relay, over whichever bearer: the route whose chance times the
  value of arriving then, less its expected airtime, is best, and holding the
  message is always an option. Airtime is priced higher the busier the
  channel, so stations back off together.
- **Fountain-coded transfers.** RaptorQ bursts sized by the expected airtime
  to finish; a doubtful path is probed before it is sent a burst.
- **Bounded control traffic.** Beacons and control messages keep to a fixed
  share of the channel however many stations share it.

In simulation, five real stations on a week of fading, diurnal HF deliver
240 of 240 messages at 6 % channel occupancy, half within 1.4 hours of the
earliest possible, each transmitter keyed about 1 % of the time and never
for much more than 20 s at once; see [results](docs/results.md).

## Quick start

```sh
cargo install --path crates/hm-cli     # Rust 1.90 or newer
hm setup                               # key, station.toml, radio or internet
hm whoami                              # give this line to the other station
hm trust add "SO5KM 8a1e…"             # and add theirs
hm node                                # the station: open the web page it prints
```

[Setting up a station](docs/operating/setup.md) covers each step, the radio
(Direwolf, hardware TNCs, the built-in modem), ARQ modems and internet links.

## Documentation

Everything is in [`docs/`](docs/README.md):

- **Operating**: [setup](docs/operating/setup.md),
  [settings](docs/operating/settings.md),
  [web page and API](docs/operating/web-and-api.md),
  [radio](docs/operating/radio.md), [modems](docs/operating/modems.md),
  [internet](docs/operating/internet.md), [a lab of four
  stations](docs/operating/lab.md).
- **Design**: [overview](docs/overview.md),
  [architecture](docs/architecture.md), [models](docs/models.md),
  [routing](docs/routing.md), [transfer](docs/transfer.md),
  [custody](docs/custody.md), [control plane](docs/control-plane.md).
- **Evidence**: [simulation](docs/simulation.md),
  [results](docs/results.md), [testing](docs/testing.md).
- **Reference**: the [specification](docs/spec.md), with test vectors.

## Status

Wire version 0: the format and the crates still change. The protocol and the
daemon are implemented and tested in simulation and on localhost; the first
multi-hop on-air trials are next ([plan](docs/on-air-test-plan.md)), and
their measurements will tune the models and limits. The built-in web server
does not terminate TLS; use a reverse proxy, VPN or SSH tunnel for remote
access.

## Building

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

More in [testing](docs/testing.md).

## Licence

MIT or Apache-2.0, at your option.
