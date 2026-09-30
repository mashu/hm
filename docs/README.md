# hm-net documentation

Start with the [overview](overview.md): what the network is for, the three
ideas it rests on, and the life of one message. Then follow the path that
fits what you want to do.

## Run a station

1. [Set up a station](operating/setup.md): install, create a key, trust
   another station, start the node.
2. [Settings](operating/settings.md): `station.toml`, and what changes while
   the node runs.
3. [The web page and the API](operating/web-and-api.md).
4. Bearers: [packet radio](operating/radio.md) (Direwolf, hardware TNCs, the
   built-in modem), [ARQ modems](operating/modems.md) (VARA, Mercury,
   ARDOP), [the internet](operating/internet.md) (links, servers, open hubs).
5. [Four relaying stations on one laptop](operating/lab.md).
6. [Group bulletins](bulletin-channels.md).

## Understand the design

| Page | What it answers |
| --- | --- |
| [Architecture](architecture.md) | which crate does what; machines without I/O; where each decision lives |
| [Models](models.md) | what a station believes about paths, custodians and the channel, and how evidence updates it |
| [Routing](routing.md) | whether to send, when, which way: expected utility, forecasts, holding, who explores |
| [Transfer](transfer.md) | moving one object over the radio: fountain-coded overs, burst sizing, probing, silence |
| [Custody](custody.md) | who is responsible for a message, how that passes, how delivery is known |
| [Control plane](control-plane.md) | beacons, the channel budget, contact adverts, holdings reconciliation |
| [Design decisions](design.md) | what is new beside Winlink, APRS, Meshtastic and DTN; how far the models can be trusted; why the web interface is built as it is |

## Check the claims

| Page | What it holds |
| --- | --- |
| [Simulation](simulation.md) | the simulated channel, whole stations for days, and how to run them |
| [Results](results.md) | what has been measured, with the command for each number, and what is still wrong |
| [Testing](testing.md) | building, the test suites, CI, fuzzing |
| [On-air test plan](on-air-test-plan.md) | the first trials on real radios |

## Reference

- [Specification](spec.md): frames, bundles, receipts, SYNC, with test
  vectors checked by an independent implementation.

## History

Reviews made along the way, each a snapshot of the code at the time. The
pages above describe the current design.

- [Code review](assessments/code-review.md)
- [Protocol assessment](assessments/protocol.md): hm-net against Winlink,
  Meshtastic and the DTN literature.
- [Model assessment](assessments/model.md): every estimate and decision, and
  the plan that rebuilt them on one set of models.
