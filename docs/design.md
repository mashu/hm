# Design decisions, and what is new

This page says what hm-net contributes beside the systems it will be
compared with, how far its models can be trusted, and why the station's web
interface is built the way it is. The other pages describe each part; this
one weighs them.

## What is new

Each ingredient is known. Store and forward with custody is delay-tolerant
networking (the Bundle Protocol). Routing over contacts is contact graph
routing (CGR). Probabilistic routing on encounter history is PRoPHET.
Fountain codes, Bayesian online learning, Thompson sampling and isotonic
calibration are textbook. What hm-net adds is putting them together into
**one probabilistic model of the network that every decision reads**, on
shared amateur HF, and checking that model against its own outcomes:

| | Winlink | APRS | Meshtastic | DTN (CGR / PRoPHET) | hm-net |
| --- | --- | --- | --- | --- | --- |
| Who carries a message | a gateway to a central server | every digipeater in reach (flood) | every node, up to a hop limit (flood) | custodians along a route | custodians along a route, chosen per message |
| What a route is chosen on | the operator picks a gateway | none | none | a contact plan known in advance (CGR); an encounter score (PRoPHET) | a contact plan **forecast** from learned beliefs about each path: within reach, open now, open when in the day, how lossy |
| Is a path's chance a probability | no | no | no | no (PRoPHET's score is a heuristic) | yes, and **calibrated**: every chance given is checked against how the handoff ended, and corrected where the models err |
| What sending costs | nothing | nothing | nothing | nothing | airtime, priced by how busy others keep the channel; holding the message is an option worth nothing, so nothing is sent that is not worth its airtime |
| When a relay goes silent | the server knows | nobody | nobody | a fixed timer | an optimal-stopping timer from the predictive distribution of how late that custodian's receipts come |
| Why it did what it did | a log | nothing | nothing | a log | every belief, the evidence that moved it, and every routing decision, live on the station's web page and API |

Put as claims:

1. **A contact plan that is learned, not given.** HF paths open and close
   with the sun. CGR needs the plan in advance; hm-net learns it per path
   (a dynamic Bayesian logistic regression on the hour of day, a two-state
   openness filter with a learned persistence, and a reach probability for
   paths never heard), and plans over the forecast ([models](models.md),
   [routing](routing.md)).
2. **Chances that are checked.** The chances the models give are recorded
   against outcomes, in bands, and corrected by Bayesian binning with
   hierarchical shrinkage and an isotonic constraint. In simulation the
   uncorrected chance for paths never heard was seventy times too high; the
   correction took a sixth of the channel back for the same delivery
   ([results](results.md#checking-the-chances)).
3. **One decision rule for everything.** Send now, wait for a path to open,
   wait to hear a station, go through a relay, go two ways, or hold: the
   route with the greatest expected value of delivery, less its expected
   airtime at the current price. Congestion control falls out of the price.
4. **Explainable by construction.** Because every decision reads one model,
   the station can show the model: what it believes of each station and path
   (with credible intervals), the evidence as it arrives and what it moved,
   how its chances have come true, and why each message goes or waits.

What it is not yet: tested on the air. The numbers are from `hm-sim`, whose
channel is a model too ([simulation](simulation.md),
[on-air test plan](on-air-test-plan.md)).

## Are the models right, and are they enough?

The updates are Bayesian and, within their stated approximations, correct:
conjugate Beta updates with exponential forgetting for rates (frame loss,
handoffs, custody), a Gamma-Poisson count and a Beta occupancy for the
channel, a Normal-Gamma on log lateness (Student-t predictive) for receipts,
a Laplace-approximate (one Newton step) update for the daily pattern, and an
exact two-state Markov relaxation for "open now". Tests check each against
its closed form or against simulated ground truth.

Where they are approximations, and what that costs:

- **Hops are independent.** A route's chance is the product of its hops'.
  Paths that share an ionosphere are correlated (a geomagnetic storm closes
  them together), so multi-hop chances are optimistic in bad hours.
  Calibration corrects the average error, not the correlation.
- **Paths are symmetric.** One belief serves both directions. HF
  propagation is reciprocal, but noise and power at each end are not: a
  station with a loud neighbour hears less than it is heard.
- **The day is the only covariate.** The daily pattern drifts with seasons
  and the solar cycle but takes no space-weather input (solar flux, K
  index) and no propagation prediction (VOACAP) as prior. Both are natural
  extensions: a prediction would give a new path a far better prior than the
  population's.
- **Forgetting is fixed.** Half-lives (an hour for the channel, days for
  paths, a week for calibration) are chosen, not learned.
- **Loss beliefs start uncalibrated.** The frame-loss prior (15 %) is more
  hopeful than long HF paths; handoff chances are calibrated, frame loss is
  not yet ([results](results.md#what-is-still-wrong)).

Enough for what it decides? The measured gaps are elsewhere: on the
simulated sparse networks every lost message had a path the station knew of,
and the paths were too poor at 300 bd; on the dense ones collisions between
stations that cannot hear each other dominate, which no belief can prevent.
The models are adequate to the decisions; the radio mode is the limit.

## The web interface

### What it shows, and where

- **Network** is the situational view: a map of every station this one has
  heard of (from its own beacons heard, and those its neighbours report),
  with every path it believes in, coloured by bearer, as wide as the chance a
  handoff over it completes now, dashed where never seen open. Beside it,
  a table of the stations, and for the one picked: where it is, what its
  beacon says, each path to it with its day ahead, its learned daily
  pattern, frame loss and handoff chances with 90 % credible intervals, how
  it does as a custodian, the latest evidence about it, and the messages
  routed through it.
- **Beliefs** is the model view: the channel (how busy, how many share it,
  what a minute on the air costs now), why each message goes or waits (the
  route, each hop's chance, what it is worth after its airtime), the evidence
  as it comes in (runs of the same observation folded into one row, with the
  chance before and after), and how the chances given have come true
  (reliability diagrams, with the correction applied).
- **Settings** holds only what the operator sets: the station's own square
  (a map there too, to pick it), the radio (now with the **duty cycle**,
  which was fixed at 50 %, and the longest key-up), internet, modem,
  delivery, relay, the trusted stations and the node.

The map of the network is not in Settings: where the other stations are is
not something the operator sets. The map in Settings is for the one thing
that is: where this station says it is, fuzzed within a privacy range.
Trusted stations moved from the network view to Settings for the same
reason; the network view still shows whether each station's key is trusted.

### A map that needs no network

The earlier page loaded a map library from a CDN and tiles from a tile
server. That fails exactly when HF matters (the internet is down), and it
told a third party where the operator was looking. The map is now drawn by
the page itself: the Natural Earth 1:110m coastline (32 kB, built in by
`tools/land.py`), the Maidenhead grid over it (fields and squares, labelled
as you zoom), and pan and zoom by mouse, touch or keys. The page loads
nothing from anywhere else and runs under a content security policy of
`'self'` only. The coastline is coarse (about 10 km); a six-character square
is 5 km, so the grid, not the coast, is the reference.

### How it is built

- **A read-only projection of the node.** `hm_node::insight` is a set of
  plain view types; `Node::insight` builds them from the node's state and
  decides nothing. The routing decisions it shows are recorded where they are
  made (`node::decisions`, bounded, the oldest going first); the evidence is
  the beliefs' own journal (`hm_model::Beliefs::journal`, one bounded ring
  per kind of subject, so beacons missed by the hundred do not push out the
  rarer handoffs and receipts).
- **The node is asked, not shared.** The coordinator task owns the node.
  `GET /api/insight` sends it a question with a one-shot reply channel
  (request/reply over a bounded channel, the actor pattern); no lock is held
  on the node, and a client asking too fast waits for room.
- **Files, not a monolith.** The page is an HTML shell, one stylesheet and
  ES modules (`src/node/web/`), built into the binary and served as they
  are; there is no build step. A test checks that every module imported is
  served.
- **One store, views as functions of it.** `store.js` holds named slices
  (status, insight, messages, settings, trust), each loaded from the API and
  watched by the views that show it. A slice nobody shows is not fetched; a
  burst of changes while a request is out costs one more request; the node's
  event stream names what changed, and only what is shown is loaded again.
  Beliefs, which drift with time, are polled only while shown and in view.
- **No HTML from strings.** Everything is built with `h()`, which sets text
  as text: most of what the page shows came over the air. Styles go through
  the CSSOM, so no inline style is needed.
- **Settings as data.** Every setting is described once (its place in the
  API, its label, its limits) and `Form` builds, fills, checks and collects
  them.
- **Charts by the rules.** One blue ramp for chances, three categorical hues
  for the bearers, status colours only for status and never alone (always
  with an icon and a label), a tooltip on every mark, a legend where there
  are two series, and light and dark each stepped for their own surface.
