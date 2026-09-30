# Routing: when, which way, and whether at all

A station holding a message decides three things at once: whether to spend
airtime on it now, which way to send it (straight to the destination,
through a relay, over the radio, the internet or a modem), and when. It is
one decision, taken by expected utility, in `hm-route`.

## What the graph holds

- **Contacts**: windows in which a path is stated or seen to carry bytes.
  An operator's schedule, an internet session that is up, a peer's signed
  advert.
- **Known links**: paths the station has seen exist, from beacons (the
  sender reaches us; each station it lists reaches the sender) and from
  sessions. When a known link will be open is the beliefs' forecast, not a
  window in the graph.
- **Potential links**: paths that could be tried though never seen open. The
  destination may be in radio range, unheard; an ARQ modem can call any
  station; an internet gateway may reach it through the internet core. When
  nothing known leads to the destination (beacons tell of two hops around,
  and it is further), a relay the station reaches may hear it. Each
  potential link's chance is the beliefs' about that path, which start from
  the population's.

## The value of a route

A route delivers with probability `P`: every hop's link completes, each
custodian accepts, and each intermediate custodian does its part. It arrives
at `a`, and costs airtime and money on the hops it reaches:

```text
U(r) = P · u(a) − C
C    = Σ_h P(reach hop h) · cost_h
cost_h = attempt(bearer) + price(bearer) · airtime_h
airtime_h = (bytes · E[1/(1 − ε_h)] + overhead) · 8 / rate
price(radio) = radio_cost / 6000 per second × 1 / (1 − busy)
```

`u` is the value of delivering at `a` relative to delivering now: it falls
linearly to nothing at the message's expiry, or halves every ten minutes for
urgent traffic. Costs are in units of one delivered message's value;
`[delivery] radio_cost = 5` means a minute of airtime on a quiet channel
costs 5 % of a message. The `1 / (1 − busy)` factor is congestion pricing:
airtime is dearer the busier others keep the channel, by the factor by which
a queue's delay grows with its load, so when the channel fills every station
holds back what is not worth the crowd. The airtime of a hop is its expected
airtime, frames lost to fades counted.

## Holding is always possible

Keeping the message and doing nothing now is worth zero and costs nothing.
So a route is taken only if its utility is above zero. A link so doubtful
that its airtime is worth more than its chance is not tried, and the message
waits for a better time or a better route (`RouteError::NotWorthIt`). The
planner looks again after a quarter of an hour, or when the destination is
heard.

## When: forecast departures

Besides the contacts the graph holds, every known link can be taken later:
at departures every 15 minutes up to 36 hours ahead, each with the chance
the beliefs forecast for that moment. A doubtful link now, or a likely one
in the morning through a relay: the one of greater expected utility wins,
and a route that departs later tells the station to wait. Only departures
likelier than every earlier one on the same link are tried (the upper
envelope): a custodian can always hold, so arriving earlier with a better
chance dominates.

A plan for later is a forecast, and forecasts improve: the message is
planned again at least every 15 minutes while it waits (a receding horizon),
and at once when the station it waits for is heard.

## The search

The planner is an A* search over partial routes (labels). Each label's bound
`P·u(arrival) − C` can only fall as it is extended, so the first complete
routes out of the queue are the best, and the search ends at the first label
whose bound is not above zero: nothing through it, or through anything after
it, is worth its cost.

- **Dominance.** A label is dropped when one already expanded at the same
  station, through the same first hop, arrived no later, with no more risk,
  cost or airtime, and with no more stations ruled out on the way.
  Keeping first hops apart keeps alternatives through other neighbours for
  failover and urgent copies.
- **Loops.** A route may not pass a station the message has been through,
  nor its origin.
- **Limits.** Hop count, the message's airtime budget and a label budget
  bound the search. When the label budget runs out, the routes found so far
  are used.
- **Cost.** Labels live in an arena, each pointing at the one it extends, so
  extending copies nothing. Each known link's chance at each departure on
  the forecast grid is computed once per plan and shared by every label that
  leaves over it.

## Failing over, and urgent copies

A route that fails is not the end: the custodian tries another. A failed
first hop is known within the transfer; a loss further on only when the
origin's receipt timer fires, much later. So the ranking counts the first:

```text
U(r) + (1 − p₁) · U(best other way still open once r's first hop has failed)
```

A cheap, likely radio hop with the internet to fall back on can beat going
to the internet straight away, and a slow sure route can beat a fast
doubtful one that has no fallback.

Urgent traffic may go two edge-disjoint ways at once, when the time saved
outweighs the cost of the second copy (compared by utility, not by a
threshold on the gain in probability).

## Who explores

The origin plans with one Thompson draw from the beliefs, so paths and
relays little is known about get tried in proportion to the chance they are
best; the end-to-end receipt, or its absence, teaches it which were. A draw
in which nothing is worth trying holds the message only if the posterior
mean agrees, so a station never heard, as likely out of reach as not, still
gets one cheap try.

Relays plan on the posterior mean. A relay never learns how its choice ended,
so exploring would only add noise, and relays that each drew afresh among
many neighbours passed messages on to whichever looked best in their draw,
hop after hop (see [results](results.md) for what that cost).

## Handing on

When the chosen route leaves now, the station hands the message to the first
hop: a transfer over the radio ([transfer](transfer.md)), a stream over the
internet, or a modem call. Whatever happens teaches the beliefs: the frames
the ACKs count, whether the handoff completed, whether the custodian
accepted. After a failed attempt the message is planned again after the
retry gap (a minute, doubling up to an hour), with the failure now in the
beliefs; a relay that has tried its limit gives the message back to the
station before it ([custody](custody.md)).
