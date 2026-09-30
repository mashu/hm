# What a station believes

Every estimate a station makes lives in one crate, `hm-model`, and every
decision reads it: the planner, the transfer engine, the custody timer, the
beacon interval. Each model is a generative story (what we think produces
the observations), a prior, and an update by Bayes' rule for each kind of
observation. Where the exact posterior is not tractable, the update is
assumed-density filtering: each part of a mixture gets the posterior
responsibility of its explanation.

Three things are believed in: **paths**, **custodians** and **the channel**.

## Paths

A path is two stations and a bearer. Both directions share one model:
propagation is reciprocal, and a handoff needs frames both ways (the object
one way, the ACK and receipt the other).

```text
r      ∈ {within reach, not}, fixed           a station too far never opens
π(t)   = P(open at t | r) by time of day       diurnal: logistic in the UTC hour
o_t    ∈ {open, closed}, Markov, relaxing to π(t) with correlation time T
ε      = frame loss while open                 Beta, with a fade dispersion ρ
h      = P(a handoff completes | open)         Beta
```

**Reach.** Some pairs never hear each other. Without a notion of reach, a
model of "open by hour" learns probabilities near zero slowly, and draws
from it keep finding hours worth a try. Each miss on a path never seen open
multiplies the odds of reach by `1 − d·P(open | reach)`, the chance it was
missed were it in reach (with `d` the chance a frame gets through when
open). A dozen misses at hours it would be open rule it out; anything seen
open puts it in reach for good.

**The daily pattern** `π(t) = σ(w·φ(h))` has an intercept and two harmonics
of the day, so one or two openings a day (a daytime band, a grey-line path)
are learned, and neighbouring hours share strength. The weights drift
(seasons, the solar cycle): a Gaussian belief over them is widened between
observations and sharpened by each, one Newton step per update (the Laplace
approximation of online Bayesian logistic regression).

**The state now** is a two-state filter. After an observation it relaxes
toward `π(t)` with correlation time `T`, which is itself learned: after each
observation `ln T` takes a small step up the gradient of that observation's
predictive log-likelihood. HF openings that last hours and VHF paths that
stay up for days end up with their own `T`.

**Frame loss** `ε ~ Beta(lost, got)`. Fading makes losses come in bursts, so
each over draws its own rate around `ε` with dispersion `ρ`, and the frames
that arrive in an over follow a Beta-binomial. That predictive, not a point
estimate, sizes bursts ([transfer](transfer.md)). The expected airtime to get
one frame through is `E[1/(1 − ε)] = (lost + got − 1)/(got − 1)`: a link
little is known about may be worse than it looks, and routes pay for that
([routing](routing.md)).

**Evidence.** Each kind of observation is a likelihood on this state, and
moves exactly the parts it bears on:

| Observation | open / closed | frame loss | handoff |
| --- | --- | --- | --- |
| a beacon or frame heard from the station | open | one frame arrived | |
| a neighbour's beacon says it heard the station | open (hearsay) | | |
| a beacon due and not heard, or a station a neighbour hears and we do not | closed, or open and lost | lost, if it was open | |
| the internet link up / down | open / closed | | |
| an ACK counting `got` of `sent` frames | open | `sent − got` lost | |
| a handoff that completed / failed | open / closed or failed | | completed / failed |

A refusal or a "busy" answer is evidence about the custodian, not the path:
the path carried the question and the answer.

### Is it open now?

Two decisions need the state now rather than a forecast: whether a transfer
under way should send another over, and whether to wait to hear the next hop
before sending at all. The path model keeps that state; a transfer takes a
copy of it (`hm_model::Openness`) and follows it as it goes:

```text
p(t + Δ) = π + (p(t) − π) · e^(−Δ/T)          nothing observed for Δ
p' = 1                                         the far end heard
p' = p (1 − a) / (p (1 − a) + 1 − p)           unanswered, where an open path answers with chance a
```

A transfer carries it from the moment it starts ([transfer](transfer.md)),
so what the sender learns from each over counts at once, and hearing the
peer makes the path certain to be open only at that moment.

**When the next hop is heard.** A station transmits about once a beacon
interval, at a moment unknown to us, and each transmission is heard if the
path is open then and the frame gets through. `first_hearing` gives the
chance the peer is heard within a window and the expected wait for it, from
the forecast of the path being open over the window
([routing](routing.md#or-when-the-next-hop-is-heard)).

## Custodians

A station that takes custody:

```text
a  = P(accepts custody | the handoff reached it)          Beta
r  = P(does its part | it accepted)                       Beta
ℓ  = how late the end-to-end receipt comes back, past the time the route
     planned the message to arrive:  ln max(ℓ, 1 min) ~ N(μ, 1/λ),
     (μ, λ) ~ Normal-Gamma
```

A message handed to a custodian reaches its destination with probability
`r·q`, where `q` is the rest of the route's chance, which the planner
predicted. **No receipt yet** is a censored observation, and a mixture: the
custodian dropped it (`1 − r`), the rest of the way failed (`r(1 − q)`), or
the receipt is still on its way (`r·q·(1 − F(ℓ))`). The custodian takes only
its share of the blame, `(1 − r) / (1 − r·q·F(ℓ))`: silence soon after the
planned arrival says little. A receipt that comes after all, even after
custody was reclaimed, counts as delivered with its true lateness, so the
lateness is learned from slow receipts too, not only from those quick enough
to beat the timer.

Measuring lateness from the route's planned arrival keeps what the planner
already knows out of the custodian's account: a route that waits for the
morning opening is not a slow custodian. Only the origin learns about
custodians; see [custody](custody.md) for the timer built on this model.

## The channel

```text
λ ~ Gamma                 stations active on the channel, drifting
N_window ~ Poisson(λ)     distinct stations heard in a window
β ~ Beta                  share of time others keep the carrier busy
```

An hour's half-life lets both follow the day. The number of stations
sharing the channel divides the beacon and control budgets
([control plane](control-plane.md)); how busy it is prices airtime
([routing](routing.md)) and sizes the wait for a clear channel.

## Priors from the population

A path never seen starts from the population of paths of its bearer: what
this station has learned about all its radio paths (or internet, or modem),
shrunk toward a weak hyperprior (empirical Bayes). The same holds for
custodians. So a new neighbour is expected to behave like the ones already
known, not like a number written into the code; and on a network where
most pairs never hear each other, a new pair starts out doubtful.

## Draws and means

Decisions read the beliefs in one of two ways:

- **A Thompson draw**: one plausible world sampled from the posterior, the
  same draw for every question within one plan. Links and custodians little
  is known about get tried in proportion to the chance that they are the
  best. The origin of a message plans this way: it learns from the end-to-end
  receipt, or its absence, which choice was good.
- **The posterior mean**: relays plan on the beliefs as they are. A relay
  never learns how its choice ended (the receipt goes back to the origin),
  so it has nothing to explore for, and each relay drawing afresh among many
  would pass messages on to whichever looked best in its draw, hop after
  hop.

Costs are always expectations (`E[1/(1 − ε)]` for airtime): what a link costs
to use is not what is explored.

## Keeping and forgetting

Beliefs are saved in the store as versioned records and restored at start.
Evidence fades: frame counts with a two-day half-life, handoff outcomes over
a week, custodian behaviour over two weeks, the channel over an hour. A model
not observed for 120 days has returned to its prior and is forgotten. With a
fixed seed, planning and simulation are reproducible.
