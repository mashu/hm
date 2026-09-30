# hm-net as a federated, congestion-free, low-latency alternative to Winlink — a critical assessment

> **A snapshot.** This assessment was written against the code at the time and is kept as a record. The current design is described in
> [the documentation](../README.md); what has changed since is in
> [results](../results.md).

Assessed on branch `claude/admiring-bell-k8drix`, 2026‑09‑29, after the fixes in
[the code review](code-review.md). Numbers marked *measured* come from the simulator running the real
protocol code, with the command to reproduce them. Everything else comes from reading the
code. §6 lists what this round changed; "before" means commit `5ff3fef`.

## Verdict

**The data plane is state of the art for amateur radio. The control plane collapsed the
way Meshtastic does, and is now bounded. Federation and the HF physical layer are still
the gaps between hm and a Winlink replacement.**

- *Moving one message is excellent.* Payload is never flooded. It moves hop by hop under
  signed custody, in fountain‑coded bursts sized by AIMD congestion control, with a hash
  check on every decoded object and a destination‑signed end‑to‑end receipt. A short
  chat crosses one VHF hop in 2.5 s (*measured*). No other amateur system combines
  custody, end‑to‑end proof of delivery, erasure coding and cleartext signatures.
- *Keeping the network informed did not scale, and taught nothing.* With N stations in
  one radio area, control traffic took 19% of a VHF channel at 10 stations, 40% at 20
  and 75% at 40. On HF 300 bd it took more than the channel can carry at 40
  (*measured*, §3.1). Every contact advert went on air already expired, so it added
  nothing to what beacons had already said. Seven separate causes contributed; all are
  fixed in this round. Control traffic now stays at **2.3–4.2% of the channel for any
  number of stations**, with the same link knowledge.
- *It is not yet federated the way Winlink users need.* There is still no way to find
  where an unknown callsign collects its mail, and every relay must hold every origin's
  key. This round added a default route to internet gateways, so a radio‑only station
  can at least reach the internet core. The signed binding records with home mailboxes
  exist in `hm-ident`, but nothing distributes or uses them yet. Today hm is a set of
  trust islands with an excellent transport, not yet a federation.
- *HF is the stated primary medium, but the built‑in HF physical layer is 300 bd AFSK.*
  A 150‑byte chat takes a median of 39 s at 20 dB SNR on a moderately fading path
  (*measured*). Modern open HF modems do far better at far lower SNR. The ARQ modem
  bearer (VARA, ARDOP, Mercury) helps, but only by giving up the broadcast and fountain
  advantages that make hm interesting.

---

## 1. What "state of the art" has to mean here

Measurable targets for a federated RF messaging network that replaces Winlink without
Meshtastic's congestion:

| # | Requirement | Target |
| --- | --- | --- |
| T1 | Control airtime bounded by the channel, not by the station count | ≤ 5% of a shared channel in total, for any number of stations in range |
| T2 | Payload transmissions per delivered message | O(hops on the path), never O(stations) |
| T3 | Latency of a short message | one VHF hop < 5 s p50; one HF hop < 60 s p50 at modest SNR; multi‑hop ≈ sum of hops |
| T4 | Delivery to an offline station | the message waits at a mailbox the recipient will contact, not on the sender's node |
| T5 | Federation | no central server or authority; any station can reach a callsign it has never heard of; operators choose whom they trust |
| T6 | Identity and regulation | cleartext, signed, station ID on air, third‑party traffic controllable per relay |
| T7 | Robustness | custody, end‑to‑end proof, crash safety, no silent loss |
| T8 | HF efficiency | physical layer within a few dB of the best open HF modems |
| T9 | Interoperability | internet e‑mail gateway; a path from Winlink users |

## 2. Where hm stands against the alternatives

● strong, ◐ partial, ○ weak or absent. "hm" is this branch.

| | Winlink | Packet BBS (FBB/BPQ) | Meshtastic | MeshCore | Reticulum + LXMF | JS8Call | **hm** |
| --- | --- | --- | --- | --- | --- | --- | --- |
| No central operator (T5) | ○ CMS is central | ● | ● | ● | ● | ● | ● |
| Addressing unknown stations (T5) | ● CMS knows every mailbox | ◐ hierarchical `@BBS.#region` | ◐ flood | ◐ flood to discover | ● announces, path requests | ○ | ◐ default route to gateways; no directory (§3.3) |
| Payload never flooded (T2) | ● | ◐ bulletins flood by design | ○ managed flood | ◐ flood to discover, then routed | ● | ◐ | ● |
| Control traffic bounded (T1) | ● little control traffic | ● | ○ | ◐ | ● announce cap per interface | ◐ heartbeats | ● after this round (was ○) |
| Custody and end‑to‑end proof (T7) | ◐ session ACK | ○ | ○ implicit ACK | ◐ | ◐ link receipts | ○ | ● |
| Erasure coding against fading | ○ (ARQ in the modem) | ○ | ○ | ○ | ○ | ○ | ● RaptorQ |
| Reliable group messages | ○ | ◐ flood | ◐ flood | ◐ | ◐ | ◐ | ◐ one hop, with repair (§3.8) |
| Signed cleartext, legal on ham bands (T6) | ◐ passwords, no signatures | ○ | ○ encrypted by default | ○ encrypted | ○ encrypted by default | ○ | ● |
| HF physical layer (T8) | ● Pactor/VARA (proprietary) | ○ 300 bd | ○ LoRa only | ○ LoRa only | ◐ any, via interfaces | ● very robust, very slow | ◐ 300 bd native; VARA/ARDOP/Mercury as ARQ bearers |
| Delay tolerance, scheduled contacts | ○ | ◐ forwarding schedules | ○ | ○ | ◐ propagation nodes | ◐ | ● contact graph |
| Internet e‑mail gateway (T9) | ● | ◐ | ○ | ○ | ○ | ○ | ○ |

The takeaway: hm has the best data plane in the table. Its gaps are where Winlink's
central servers buy it simplicity (addressing, key management, e‑mail), and where
Reticulum's announce design buys it on‑demand paths.

---

## 3. Critical findings

### 3.1 Control‑plane congestion collapse (critical; fixed, `eaaf879`)

*Measured* with `cargo test -p hm-cli --release --test control_load -- --ignored --nocapture`.
N stations that all hear each other run the daemon's control policy on one channel with
p‑persistent CSMA. Each column is that traffic's share of channel time. "links" is the
share of radio links between stations that stations know of, from beacons or adverts.

**Before** (`5ff3fef`, the same harness on the old policy):

| N | VHF: beacons | contacts | filters | channel | links known (adverts added) | HF 300 bd: channel |
| --- | --- | --- | --- | --- | --- | --- |
| 5 | 1.2% | 8.3% | 1.5% | 9.9% | 100% (0%) | 13.0% |
| 10 | 2.8% | 14.5% | 5.1% | 19.1% | 100% (0%) | 27.6% |
| 20 | 6.8% | 27.5% | 11.7% | 40.1% | 88% (0%) | 59.4% |
| 40 | 13.4% | 58.1% | 20.4% | 75.1% | 48% (0%) | 118.7%: more than the channel |

**After** (one station in four holding mail, one scheduled contact each):

| N | VHF: beacon every | beacons | contacts | filters | channel | schedules known | links known | HF: beacon every | channel |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 5 | 10 min | 1.3% | 0.4% | 0.8% | 2.3% | 100% | 100% | 21 min | 3.6% |
| 10 | 14 min | 2.0% | 0.9% | 0.8% | 3.7% | 100% | 100% | 50 min | 3.1% |
| 20 | 34 min | 2.0% | 1.2% | 0.5% | 3.6% | 100% | 92% | 121 min | 4.2% |
| 40 | 68 min | 2.0% | 1.3% | 0.1% | 3.3% | 70% | 51% | 239 min | 4.1% |

With every station holding mail (the worst case for pulls), VHF stays at 2.9–3.7%. A
gateway's internet adverts never reach the air. A regression test holds the budgets at
10 and 30 stations.

The causes, each confirmed in the code, and what was done:

1. **Every refresh was new content.** A live advert used `start = now`, and the Trickle
   key includes `start`. Each 10‑minute refresh therefore started a new entry at the
   5‑second minimum interval, and SPEC's "a stable network becomes quiet" never held.
   *Fixed:* an advert keeps its identity. A refresh that says the same thing replaces
   the held copy without resetting the interval.
2. **N² origination.** Every station adverted every station it heard, repeating what
   beacons already said. *Fixed:* live contacts go to the internet core only, where
   gateways need them. On air, beacons are the topology.
3. **Synchronised Trickle timers.** The "random" moment in each interval was a hash of
   the advert and the second it was heard. Every station that heard it at once sent at
   once, collided, and never suppressed. *Fixed:* each station salts its moments with
   its callsign.
4. **The budget was per station, not per channel.** N stations at 2% each spend N × 2%.
   *Fixed:* the stations heard share one budget. A frame longer than a station's share
   still goes out, after enough quiet to keep its average within the share.
5. **A FIFO backlog longer than the information's life.** The queue held about an hour
   of frames for adverts valid 20 minutes, so everything went out expired. *Fixed:* a
   newer frame replaces an older one about the same thing; expired frames and frames
   heard on air from others are dropped.
6. **Pairwise holdings polls were N².** Every station pulled from every neighbour every
   5 minutes. *Fixed:* beacons carry a new "holding" flag, and stations pull only from
   stations that set it, at most every 30 minutes. Holders push anyway.
7. **Internet topology leaked onto the air.** A gateway broadcast every internet advert
   it learned. *Fixed:* adverts learned over the internet, and internet contacts, never
   go on air.

Meshtastic's congestion comes from every node rebroadcasting every packet. hm never did
that with payload, but its control plane had the same shape: per‑station traffic
multiplied by the station count, then again by the neighbour count.

**Still open:** a beacon digest saying *whom* held mail is for, so only those stations
pull. Also on‑demand path discovery beyond two radio hops (§4.4): pure‑radio routes
longer than two hops now rely on scheduled contacts and gateways.

### 3.2 Beacons (major; rate fixed, size open)

A beacon carries the station's 32‑byte key, a 64‑byte signature and up to 16 heard
stations: 126–238 bytes, 1.3–2.1 s at 1200 bd, 4.3–7.4 s at 300 bd. *Fixed:* the
interval now grows so that all beacons together stay within 2% of the channel (table
above), and liveness and heard lists follow the interval. *Open:* the key is redundant
(receivers verify only against keys they already trust), so an 8‑byte fingerprint would
do. The 16‑station heard list caps link knowledge at about half in a 40‑station area.
Both are wire changes.

### 3.3 Federation (critical for replacing Winlink; partly fixed)

- **No address resolution.** A message to a callsign with no known path waits until one
  appears. `BindingRecord.homes` (the mailboxes that hold mail for a callsign) exists,
  but nothing distributes or reads it. Winlink's CMS answers "where does W1ABC collect
  mail?" centrally; hm needs a federated equivalent, like DNS MX records (§4.2).
- **Default route: fixed** (`eaaf879`). A station without internet links and no known
  path hands the message to a relaying internet gateway it can reach. An integration
  test sends radio‑only → gateway → internet‑only and gets the receipt back.
- **Trust does not federate.** A relay must find every *origin's* key in its own trust
  list (SPEC §11.4). Attestations exist in `hm-ident`, but nothing checks them against
  issuers, and there is no key revocation.
- **Messages wait on the sender's node.** For EmComm this is backwards. A message should
  move towards the recipient's home mailbox as soon as any path to it exists.

### 3.4 HF physical layer (major; open)

*Measured*: one 150‑byte chat on a 300 bd HF path with Watterson fading (0.5 Hz Doppler
spread), until its receipt is back, took p50 84 s / p95 545 s at 17 dB, 39 s / 157 s at
20 dB, and 21 s / 87 s at 24 dB
(`cargo test -p hm-xfer --release --test sim chat_latency -- --ignored --nocapture`). These use the built‑in 1200 bd
modem's loss curve, perhaps 6 dB pessimistic for a true 300 bd modem. AFSK at 300 bd
still needs SNRs that HF rarely gives. Open multicarrier modes (codec2's `datac*` modes
as FreeDATA uses them, ARDOP, Mercury) decode near 0 dB or below, at several times the
rate. The fountain transfer engine is exactly what such a broadcast‑capable modem needs.

### 3.5 Latency (good on VHF; acceptable on HF)

- *Measured*, one VHF hop, 150‑byte chat: p50 2.5 s clean; 2.5 s (p95 6.5 s) at 10%
  loss; 5.7 s (p95 22.8 s) at 30% loss. That is the frame, TXDELAY and one ACK
  turnaround, about as good as 1200 bd allows.
- Multi‑hop is store‑and‑forward: a full transfer and custody receipt per hop, plus up
  to 1 s of the coordinator's tick. That is the right trade on lossy radio.
- The end‑to‑end receipt is a whole signed bundle routed back, roughly doubling a short
  chat's airtime. Batching receipts, or carrying them in the custody ACK when the
  destination is the next hop, would halve it (open).
- A message whose retries are used up is held until it expires. It is tried again every
  `max_delay_secs` (1 h by default), or at once when its destination is heard or links
  (`f0a6ac5`).

### 3.6 Frame and signature overhead (compact header fixed, `092afff`)

Every frame carried an 18‑byte hm header inside a 16‑byte AX.25 header, naming the
source twice. *Fixed:* to a station that advertises it, a frame now goes to the
station's own AX.25 address with a 6‑byte header, and the receiver rebuilds the rest
from the AX.25 addresses. That saves 12 bytes a frame, about 11% of a DATA frame's
airtime with 64‑byte HF symbols, and it is negotiated like IL2P. Every bundle, receipt,
beacon and advert still carries a 64‑byte Ed25519 signature: the price of cleartext
authentication, and worth it.

### 3.7 Channel access (moderate; open)

p‑persistent CSMA with carrier detect is the right default, and the transfer engine
backs off on missed ACKs (AIMD). Hidden terminals still collide freely; on HF, scheduled
contacts are the practical answer, and the contact graph models them. Precedence only
reorders the queue: an urgent message cannot pre‑empt an over already on air.

### 3.8 Group messages (repair fixed, `b6af040`; multi‑hop open)

A bulletin was one broadcast over, and listeners that missed symbols could not ask for
more. *Fixed:* a listener that ends an over short asks the sender at a random moment.
It stays quiet if it hears two others ask for as much, and asks again if no repair
comes. The sender answers with fresh fountain symbols, which help every listener that
is short. *Measured*, 10 listeners each losing 25% of frames, a 2 kB bulletin: one
publish reached 72% of listeners; with repair it reached 99%, for 37% more airtime and
about six requests in all. *Open:* bulletins still reach only one radio hop. Multi‑hop
dissemination should reuse Trickle and this repair.

### 3.9 Security

Strong where it matters: every bundle, beacon, advert and receipt is signed; custody
moves only on a verified receipt. Open internet hubs no longer let a stranger claim
another station's callsign (`da9236b`), and a compact frame's source is always its AX.25
source. Known and documented: CTRL/ACK frames on the radio are unsigned, so a forged
CLOSE can end a transfer but never lose a bundle. Missing: key revocation and
issuer‑based trust (§3.3).

### 3.10 Interoperability (open)

There is no internet e‑mail gateway and no bridge to Winlink (B2F) users. For EmComm
adoption both matter more than protocol elegance. A hub‑side SMTP gateway, with the same
sender allow‑lists Winlink uses, is the minimum.

### 3.11 Limits of these measurements

The control‑plane harness mirrors the daemon's policy using the daemon's own components
(`Trickle`, `ControlBudget`, `SyncQueue`, beacon and budget functions), not the whole
daemon. It models one radio area without hidden terminals, and loss‑free links, so
collisions are the only losses. HF latency uses the 1200 bd modem's loss curve under
fading. The numbers show the shape (bounded versus N‑proportional) reliably; absolute
values will move on air.

---

## 4. Target architecture

1. **Two tiers.** An internet (and HF‑backbone) tier of *home hubs*, and radio areas at
   the edge. Hubs hold mail for their users, like e‑mail MX hosts, and gateways connect
   radio areas to hubs. No hub is special; any operator can run one.
2. **Federated directory.** A user's signed binding record (callsign → key, home hubs,
   sequence number), attested by issuers such as clubs or national societies, is
   replicated among hubs over the internet by gossip. A higher sequence supersedes
   under the same key (`SignedBinding::supersedes` now requires that); a new key needs
   an attestation. Relays trust *issuers*, not every user. Revocation is a binding with a
   higher sequence and a revoked flag.
3. **Routing.** Known contact path → contact‑graph routing. Otherwise → the
   destination's home hub from its binding. Otherwise → the best internet gateway in
   reach (the default route, now done). The home holds the message; the recipient pulls
   it over whatever bearer it next has (holdings SYNC exists), and the hub pushes when it
   hears the recipient (wake on contact exists).
4. **Radio control plane with a channel budget (done).** Beacons give one‑ and two‑hop
   topology, at a rate that follows the stations sharing the channel. Only stable
   information goes on air, by a correct Trickle, within a shared budget, and holdings
   are pulled only from stations that hold something. Next: a beacon digest of whom
   mail is held for, and Reticulum‑style on‑demand path requests beyond two hops.
5. **HF.** Codec2 OFDM data modes as a native bearer under the fountain engine, on
   scheduled contacts. ARQ modems stay as optional point‑to‑point bearers.
6. **Interop.** SMTP gateway at hubs; optional B2F bridge for Winlink clients.

## 5. How hm meets each requirement

| # | Before this round | Now | Still needed |
| --- | --- | --- | --- |
| T1 control bounded | ○ 75% of VHF at 40 stations, adverts teaching nothing | ● 2.3–4.2% for any N, VHF and HF | beacon holdings digest |
| T2 no payload flood | ● | ● | — |
| T3 latency | ● VHF, ◐ HF | ● VHF, ◐ HF; frames 12 bytes shorter | codec2 HF bearer; receipt batching |
| T4 offline delivery | ◐ held at the sender until expiry | ◐ + default route to gateways | home‑hub routing |
| T5 federation | ○ | ◐ default route | directory, issuer trust, revocation |
| T6 regulation | ● | ● | — |
| T7 robustness | ● | ● bulletins repaired too | — |
| T8 HF efficiency | ○ | ○ | codec2 bearer |
| T9 interop | ○ | ○ | SMTP gateway |

## 6. What this round changed

| Commit | Change | Effect (*measured* where marked) |
| --- | --- | --- |
| `5ff3fef` | Control‑plane load harness; one‑hop chat latency measurement | The before numbers above |
| `eaaf879` | Trickle identity, salted moments, radio/internet scope, shared channel budget, coalescing expiring queue, adaptive beacons, holding flag, default route; bulletins stored with their expiry | Control traffic 10–75% → 2.3–4.2% of the channel (*measured*); radio‑only stations reach internet‑only ones through a gateway (integration test) |
| `b6af040` | Bulletin repair: listener requests with suppression, fountain repair overs | 72% → 99% of listeners (*measured*) |
| `092afff` | Compact AX.25 frames, negotiated per peer; one stream parser for internet and ARQ links, fuzzed | 12 bytes less per frame; a control message refused with a long reason no longer breaks the sender's reply read |

## 7. Roadmap, in order of value

1. **Federated directory and home routing** (§3.3, §4.2–4.3). The single most
   important step from "a network of friends" to "a Winlink replacement".
2. **Codec2 OFDM HF bearer** (§3.4). It turns HF from marginal to competitive.
3. **SMTP gateway** (§3.10).
4. **Beacon changes**: a digest of whom held mail is for, and a key fingerprint instead
   of the key (§3.1, §3.2). One wire revision.
5. **Receipt batching / receipts in the custody ACK** (§3.5).
6. **Multi‑hop bulletins** with Trickle and the new repair (§3.8), and on‑demand path
   requests beyond two radio hops (§3.1).
