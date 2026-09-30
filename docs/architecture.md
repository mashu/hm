# Architecture

The code is a stack of small crates, each owning one concern and depending
only on those below it. Everything that decides is written as a state
machine without I/O, so the station that runs on the air and the station
that runs in the simulator are the same code.

## Layers

```text
                         hm-cli          the `hm` command: daemon, setup, web page and API
                           │
             ┌─────────────┼───────────────┬──────────────┐
          hm-node        hm-net          hm-rig        hm-modem-afsk
     station decisions   internet (QUIC)  sound cards,   our AFSK 1200 modem
     and radio machine                    PTT
             │
   ┌─────────┼─────────┬──────────┬──────────┐
 hm-route  hm-xfer   hm-store   hm-bundle   hm-bearer
 routing   transfer  custody,   messages,   KISS, AX.25
           engine    storage    receipts
   │         │
   └──── hm-model ───┘          beliefs: links, custodians, the channel
             │
   hm-core, hm-wire, hm-ident   time, machines, frames, callsigns, keys
```

`hm-sim`, the simulator, sits beside the stack: it runs any of the machines
on simulated channels (see [simulation](simulation.md)).

| Crate | Owns |
| --- | --- |
| `hm-core` | `Millis`, the sans-IO `Machine` trait, station inputs and outputs with radio ports, a deterministic RNG |
| `hm-wire` | base-40 callsigns, the frame header, ACK, OFFER, OPEN, CLOSE, beacons, SYNC messages, object ids |
| `hm-ident` | Ed25519 identities, signed envelopes, callsign binding records |
| `hm-bundle` | messages: build, seal, open, verify; end-to-end receipts; compressed bodies |
| `hm-model` | what a station believes: link paths, custodians, the channel; population priors; Thompson draws; burst sizing (`no_std`) |
| `hm-route` | the contact graph (contacts, known links, potential links) and route choice by expected utility |
| `hm-xfer` | the RaptorQ transfer engine: sessions, overs, ACKs, signed transfer receipts, broadcast with repair |
| `hm-store` | the persistent store on `redb`: inbox, outbox, relay holdings, custody records, beliefs |
| `hm-bearer` | KISS framing and AX.25 UI encapsulation for Direwolf and hardware TNCs |
| `hm-net` | internet links: QUIC with TLS 1.3 on the station keys, one stream per bundle |
| `hm-modem-afsk` | an AFSK 1200 modem in pure Rust (`no_std`) |
| `hm-rig` | sound cards through `cpal`; PTT by rigctld, serial RTS/DTR, CM108 GPIO or VOX |
| `hm-node` | the station: `Node` (decisions), `Radio` (the radio side), `Station` (both, for simulators) |
| `hm-cli` | the `hm` command; the daemon's shells around `hm-node`'s machines |
| `hm-sim` | the discrete-event radio simulator |

## Machines without I/O

A `Machine` (`hm-core`) takes inputs (a frame heard, a timer, a command) at a
time it is told, and returns outputs (frames to transmit, events, the next
time it wants to be woken). It never reads a clock, sleeps, or touches a
socket. Time and randomness are passed in.

`hm-node` has two:

- **`Node`**: the station's decisions. It holds the store, the beliefs, the
  contact graph and the control plane. In: radio events, transfer results
  over the internet or modem, SYNC messages, settings, ticks. Out: commands
  (send this bundle to that station over this bearer, send this SYNC frame,
  tell the radio we hold something). Its modules are split by concern:
  `deliver` (planning and handing on), `handoff` (transfers under way and
  what their endings teach), `custody` (receipts, reclaims, notices),
  `links` (internet links and SYNC), `radio` (what the radio reports).
- **`Radio`**: the radio side, an `hm_core::Machine`. It owns the transfer
  engine (`hm-xfer`), the beacon schedule, the table of stations heard, the
  channel belief, and SYNC frames waiting for the control budget. In:
  frames. Out: frames, and events for the node (received, delivered, failed,
  heard, channel busy).

`Station` joins the two into one machine, which `hm-sim` runs for days of
simulated HF in seconds. The daemon (`hm-cli node`) is a set of shells
around the same two machines: a radio thread feeds `Radio` from the TNC or
the built-in modem and carries out its frames; the coordinator feeds `Node`
and carries out its commands over the internet, the modem and the web page.

```text
  TNC / sound card ──frames──▶ Radio ──events──▶ Node ◀── internet, modem, API
                  ◀──frames───       ◀─commands─      ──▶ store, beliefs
```

## Where each decision lives

| Decision | Where | Page |
| --- | --- | --- |
| is this path within reach, open now, open later | `hm-model::link` | [models](models.md) |
| send now, wait, or go through a relay; which bearer | `hm-route::routing` | [routing](routing.md) |
| how many symbols in the next over; stop a silent transfer | `hm-model::erasure`, `hm-xfer` | [transfer](transfer.md) |
| how long the origin waits for the end-to-end receipt | `hm-model::custodian` | [custody](custody.md) |
| how often to beacon; what control traffic fits the channel | `hm-node::control` | [control plane](control-plane.md) |
| whether to accept custody of a bundle | `hm-node::accept` | [custody](custody.md) |

## Design rules

- Protocol crates are `no_std` + `alloc` where they can be, and never read
  clocks, sleep or do I/O. The simulator and the daemon run identical code.
- Signed objects are forwarded byte for byte. Ids and signatures cover the
  raw signed bytes, so old nodes route and verify what newer software wrote.
- One model per thing believed in, and every decision reads it. No second
  estimate of the same quantity elsewhere.
- The simulator is deterministic: same seed, same run, on every platform.
- Performance claims come from the simulator or from on-air tests, never
  from estimates. Numbers are in [results](results.md).
