# The control plane

Stations need to know who is around, whom they hear, and what others hold.
On a shared radio channel that knowledge must cost a bounded share of the
channel however many stations share it, or it crowds out the messages it
exists to move. Flooding meshes fail exactly here. The rules
(`hm-node::control`):

## Beacons

A beacon is a signed broadcast: who the station is, which stations it has
heard lately and when (up to 16), and flags (relays, keeps a mailbox, has an
internet link, holds something for others). Beacons are the radio topology:
each gives every listener its one-hop neighbourhood, and the neighbour's
list its two-hop one. A beacon names the key it is signed with by an 8-byte
id, not the key itself: keys are never learned from beacons, and a listener
checks the signature with the key it trusts for the station. One from a
station whose trusted key has another id is reported, not believed. (The
full key made every beacon 24 bytes longer, most of a second at 300 bd.)

Beacons go out every `beacon_minutes` (10 by default), or further apart when
many stations share the channel: all their beacons together keep to 2 % of
it. Each station counts the stations sharing the channel (a Gamma-Poisson
belief, not a raw count that jumps with every beacon, see
[models](models.md#the-channel)) and stretches its own interval to
`stations × beacon airtime / 2 %`. Beacons due and not heard are evidence
that a path has closed; a station a neighbour hears and we do not is
evidence that its path to us is closed or out of reach.

## One budget for the rest

Everything else the control plane sends on the air (contact adverts,
holdings reconciliation) comes out of one budget: 2 % of the channel over a
rolling hour, split evenly among the stations sharing it, so together they
never spend more. A SYNC frame waiting for the budget is replaced by a newer
one about the same thing, and dropped once what it says has expired (at
most 64 wait).

## Contact adverts

A contact advert is a signed statement that a path carries bytes at stated
times, with a stated chance. Operators' scheduled contacts go on air by
**Trickle**: each station picks its own moment in the interval (5 seconds,
doubling up to an hour while nothing changes), and one that hears the same
advert from others twice before its moment stays silent. Live contacts (who
hears whom right now) go only to the internet core, where gateways need them
to route into radio areas; on the air they would repeat what the beacons say,
once per pair of stations.

## Holdings reconciliation

A station that has been away pulls what others hold for it, pairwise:

```text
A → B   FILTER   a Bloom filter of what A already has (1 % false positives)
B → A   OFFER    the ids B holds for A that are not in it, in pages
A → B   WANT     the ones A asks for
B → A            the bundles, by the transfer engine, with custody
```

Over the radio, a station pulls only from one whose beacon says it holds
something, at most every half hour, and only its own mail and bulletins.
Over the internet, a relay also collects what it could carry on. On the
radio that would copy every holding to every relay in reach, against the
routes; a relay that hears a station tries what waits for it anyway.

## Numbers

With a hand-built model of this policy (`crates/hm-cli/tests/control_load.rs`),
control traffic for 5 to 40 stations that all hear each other took 2.3–3.7 %
of a VHF channel and 3.1–4.2 % of an HF one, whatever the number of
stations; before this design, 10–75 % of VHF and more than all of HF at 40
stations. With whole stations in the simulator, each station's beacon
airtime falls as the network grows (24 s an hour each at five stations, 16 s
at ten); see [results](results.md).
