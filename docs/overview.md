# Overview

hm-net moves messages (chat, mail, forms, group bulletins) between amateur
stations over radio, the internet and ARQ modems, whichever reaches. This page
says what problem it solves, how, and what happens to one message on its way.
The other pages go into each part.

## The problem

A shared HF or VHF channel is:

- **Scarce.** A 300 bd HF channel carries a few hundred bytes a minute, for
  every station that shares it.
- **Half-duplex and shared.** While one station transmits, the others in
  reach wait or collide.
- **Intermittent.** An HF path opens and closes with the time of day and the
  ionosphere; a path open at noon may be closed all night, and two stations
  may never hear each other at all.
- **Lossy.** While a path is open it fades; frames are lost in bursts.

Protocols built for the internet assume the opposite: links that are up,
end-to-end round trips, and bandwidth to spare for retries and flooding. On
a radio channel those assumptions waste the resource that matters most, the
airtime, and get little delivered for it.

## The approach

Three ideas, used together:

1. **Store and forward with custody.** A message moves one hop at a time. A
   station hands it on only when the next one has taken it, verified by a
   signed receipt, and keeps trying until then. A path that is closed now is
   not a failure: the message waits for it, or goes another way. This is
   delay-tolerant networking (DTN).
2. **Beliefs, not snapshots.** Each station keeps probabilistic beliefs
   about every path it has heard of (is it within reach at all, is it open
   now, when in the day does it open, how many frames does it lose), about
   every station it has handed custody to (does it accept, does it deliver,
   how late), and about the channel (how many share it, how busy it is). The
   beliefs are updated by Bayes' rule from what the station sees: beacons
   heard and missed, the frames an acknowledgement counts, handoffs that
   completed or failed, receipts that came back or did not. See
   [models](models.md).
3. **Decisions by expected utility.** Whether to send now, wait for a path
   to open, or go through a relay is one decision: the route whose chance of
   delivering, times the value of arriving when it would, less the airtime
   it is expected to cost, is greatest. Holding the message is always
   possible and worth nothing, so a station never spends airtime on a
   chance not worth it. Airtime is priced higher the busier the channel, so
   stations back off together when it fills. See [routing](routing.md).

Everything else follows from these. Transfers over the radio use a fountain
code and size each burst by the expected airtime to finish
([transfer](transfer.md)). How long the origin waits for the end-to-end
receipt before sending again is an optimal-stopping decision on its beliefs
about the custodian ([custody](custody.md)). The control traffic (beacons,
contact adverts, holdings reconciliation) keeps to a fixed share of the
channel however many stations share it ([control plane](control-plane.md)).

It is a **federated** network: every operator runs their own station and
chooses whose keys to trust. There is no central server or directory.
Content is sent in the clear, as amateur rules require, and every identity
is authenticated with Ed25519 signatures.

## A message's life

SA0KAM writes to SO5KM-1, whom it has never heard; SM0R1, a relay, hears
both.

1. **Queued.** SA0KAM's station builds a signed bundle and stores it.
2. **Planned.** Its planner looks at what it believes: its own path to
   SO5KM-1 has never been open, and may be out of reach; the path to SM0R1
   is open most afternoons; SM0R1's beacons say it hears SO5KM-1 most
   evenings. Going through SM0R1 in the afternoon is worth most, so the
   message waits for the afternoon (or until SM0R1 is heard, whichever is
   first), and the plan is made again every quarter hour with what has been
   learned meanwhile.
3. **Handed on.** SM0R1 is heard. SA0KAM offers the bundle and sends
   fountain-coded symbols in bursts sized by the path's frame loss; SM0R1
   decodes the bundle, checks its hash and signature, stores it, and answers
   with a signed custody receipt. SA0KAM now waits for the end-to-end
   receipt, for as long as waiting pays.
4. **Relayed.** SM0R1 plans in turn with its own beliefs, and hands the
   bundle to SO5KM-1 in the evening. Its part ends with SO5KM-1's custody
   receipt.
5. **Delivered.** SO5KM-1 stores the message and sends a signed end-to-end
   receipt back to SA0KAM, routed like any message. When it arrives,
   SA0KAM's message shows **Delivered**, and SA0KAM has learned how SM0R1
   did as a custodian.

Had the receipt not come in time, SA0KAM would have taken custody back and
tried again, another way if one was better. Had SM0R1 been unable to deliver,
it would have said so with a custody-fail notice, and SA0KAM would have
taken the message back at once.

## Words used on these pages

| Word | Meaning |
| --- | --- |
| **bundle** | a signed message object, content-addressed by its hash |
| **custody** | responsibility for getting a bundle on; taken with a signed receipt |
| **custodian** | a station holding custody of a bundle that is not its own |
| **origin** | the station that wrote the bundle |
| **path** | two stations and a bearer; both directions share one belief |
| **bearer** | radio (packet), internet (QUIC), modem (ARQ modem such as VARA) |
| **contact** | a window in which a path is stated or seen to carry bytes |
| **known link** | a path the station has seen exist; when it opens is forecast |
| **over** | one burst of frames from a sender, answered by one ACK |
| **beacon** | a signed broadcast saying who a station is and whom it hears |
| **end-to-end receipt** | a bundle signed by the destination, confirming delivery |
