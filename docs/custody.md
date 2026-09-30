# Custody and delivery

A message moves one hop at a time, and at every moment exactly one station
is responsible for getting it on: its custodian. This page is about who that
is, how responsibility passes, and how a station knows a message arrived.

## Taking custody

A station offered a bundle decides at its acceptance gate (`hm-node::accept`):

- addressed to it: store it, and answer with a signed end-to-end receipt to
  the origin (queued like any message);
- for another station: take custody only if relaying is on, the origin's
  signature verifies, the hop limit and the route so far allow it (no loops),
  and the holdings limits have room; otherwise refuse, or say "busy until".

Taking custody means storing the bundle durably, then answering with the
transfer's signed receipt. A handoff counts only with a **verified** receipt:
an unverified one never moves custody.

## Who waits for what

- **The origin** waits for the destination's end-to-end receipt. It keeps a
  suspect timer: if no receipt has come by then, it takes custody back and
  sends again, another way if one is better. When the timer fires is an
  optimal-stopping decision on its beliefs about the custodian
  ([models](models.md)):

  ```text
  G(τ) = V·L·a·u(τ) − A·(L + p·S(τ))
  ```

  Resending at `τ` past the route's planned arrival rescues a lost message
  (probability `L = 1 − p`, with `p` the custodian's chance of doing its part
  times the rest of the route's) by the other way (chance `a`), worth `V`
  copies scaled by `u(τ)`, the value left at `τ`; it costs a copy's airtime
  `A` whenever no receipt came, including when the first copy was only
  slow (`p·S(τ)`, with `S` the chance the receipt is later than `τ`). `τ*`
  maximises `G`; when nothing makes it positive the origin does not resend.
  Waiting pays while receipts are still likely to arrive, and stops paying
  as the rescue loses value.
- **A relay's part ends** with the next custodian's verified custody
  receipt. Relays never see the end-to-end receipt (it goes back to the
  origin, not through them), so a timer at every relay could only resend
  what was delivered; and fed nothing but silence, their beliefs about
  custodians would teach them to resend ever sooner. The origin's timer
  covers a custodian that loses a message. (This is how DTN custody transfer
  works: a custody signal releases the previous custodian.)
- **Nothing answers a receipt** or a custody-fail notice end to end, so for
  those too custody passing on ends the station's part.

## When a custodian cannot deliver

A plain relay that has tried its limit gives up and sends a signed
**custody-fail notice** to the station it had the message from. That station
takes it back at once, even if its part had ended, and plans again around
the station that failed. A station offered custody anew of something it had
handed on takes it on again: the sender believes it undelivered. One that
saw the end-to-end receipt pass through answers "already have it", and does
nothing more.

A **mailbox** relay does not give up: it holds messages for stations rarely
in reach until they expire, and delivers when the destination is heard.

## States

```text
             ┌── handed to the destination ─────────────────┐
Queued ── custody taken by a relay ──▶ InTransit (origin) ──┼── end-to-end receipt ──▶ Delivered
   ▲              │                        │                 │
   │              │ (relay)                └ timer fires ────┘ ─▶ Queued (reclaimed)
   │              ▼
   │        DeliveredUnconfirmed (handed on)
   │              │ custody-fail notice, or offered anew
   └──────────────┘
Queued ── retries used up (plain relay) ──▶ Failed, custody-fail to the previous station
InTransit ── expired without a receipt ──▶ DeliveredUnconfirmed
```

## Duplicates

The store keeps a bundle at most once by its content id. A duplicate offer
is acknowledged (the sender learns the receiver has it) without a second
delivery. A message received again was sent again because its sender never
saw the receipt: it is answered with a new receipt, but only once the first
one is no longer on its way, so duplicates do not multiply receipts.

## Learning from outcomes

When the end-to-end receipt arrives, the origin records how late it was
against the route's plan, and credits the first custodian it handed to (its
copy had the head start), even if custody was reclaimed meanwhile. When a
timer fires, the silence is recorded as censored evidence: the custodian
takes only its share of the blame. See [models](models.md#custodians).
