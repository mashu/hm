# hm-net: probabilistic design, architecture and robustness assessment

Scope: every place the stack estimates, predicts or decides under uncertainty;
whether those parts share one generative story or are separate heuristics;
code structure; airtime waste; routing and state bugs; and whether tests and
simulations cover HF conditions (bands that close for hours, fading, links
that are usually *not* there). Measured on `main` at 3dad604 plus the fixes on
`claude/admiring-bell-k8drix` (fc91dc0, ffd46df, and the coordinator fix).

---

## 0. Verdict

| Question | Answer |
|---|---|
| Did the merge break anything? | No. `ci` green on main; 347 tests pass, 0 fail; clippy and fmt clean; the measurement suites give numbers in line with earlier runs. One nightly job was red (stream fuzz target had no seed corpus); fixed. Found and fixed one leak in the coordinator (§3.10). |
| Is the Bayesian core sound? | **Partly.** The Beta-Bernoulli link evidence with forgetting is a real generative model. The *decisions* built on it are not: route choice ranks by a product of 10th percentiles, then by arrival, which makes forecasts badly miscalibrated (Brier 0.67) and makes CGR wait days for a slightly "safer" slot (§3.2, §3.3). |
| The biggest gap for HF | **Nothing predicts when a closed link will open again.** The graph learns P(success \| hour) but only for contacts that exist now or were configured by hand. On a four-station HF chain whose hops open 3 h/day at different hours, the router as deployed delivers **0 %**; the same router given the contact plan delivers 75–88 %; epidemic flooding delivers 100 % (§3.1). |
| Thompson sampling | Dead code. `Chooser::choose` has had no caller since 2f80608; configured bearer costs are ignored (§3.5). |
| Fewer, better models? | Yes. Today the same link is modelled three times with three memories (7 d, 1 h, and an in-memory EWMA), and a dozen hand-set probabilities stand in for priors. Three generative models cover everything: **link** (availability × erasure), **custodian** (does a node that took custody deliver), **channel** (who is contending). Every decision becomes a posterior functional plus a cost (§5). |
| Ground-up or patchwork? | Lower crates are ground-up (sans-IO `Machine`, deterministic RNG, typed wire formats, fuzzed parsers). The node layer is patchwork: a 1,200-line `async fn coordinator` holds policy, I/O and persistence together, with fallbacks that accreted fix by fix (§7). |
| Airtime waste | Yes, avoidable: blind radio attempts to unheard stations on closed bands, retries on a clock that ignores propagation, over-long overs from a fixed 0.9 target (+11–17 %), ACK timeouts treated as congestion during fades, 96 B of key and signature in every beacon (76 % of a minimal beacon) (§4). |
| Are HF conditions tested? | Frame and transfer level: well (Watterson fading, hourly band table, collisions, hidden terminals, partitions, clock drift). Network level over days: **not at all**. The whole node cannot run in simulated time, so no test runs the real stack through a day of HF openings (§8). |

---

## 1. What was verified

* **CI on main** (merge 3dad604): `ci` passed.
* **Nightly**: all jobs passed except `fuzz stream, 10 minutes`, which failed at
  start with `The required directory "corpus/stream" does not exist`. Added
  seven seeds (each message type and a five-message session), checked that
  every seed parses to whole messages that re-encode byte for byte (fc91dc0).
* **Full suite** (`cargo test --workspace --release`): 347 passed, 0 failed,
  6 ignored (measurements).
* **Measurements re-run** (ignored tests): HF/VHF transfer sweeps on Watterson
  channels, one-hop chat latency, busy beacon network, control-plane load by
  network size. Numbers used below.
* **New measurement**: `BayesianCgrLive` and `examples/routing_diurnal.rs`
  (ffd46df), below.

---

## 2. Inventory: what estimates, predicts or decides

| # | Where | What | Generative story? | Verdict |
|---|---|---|---|---|
| M1 | `hm-route/graph.rs` | Link success ~ Beta(2+s, 1+f), exponential forgetting, half-life 7 d | Yes: Bernoulli handoffs, discounted conjugate update | Good core |
| M2 | same | 24 UTC-hour buckets; other hours count 0.25 | Partly: flat pooling is a hand-set prior, not smooth in hour | Replace with a smooth diurnal prior (§5) |
| M3 | same | A beacon heard = 0.25 of a success; heard lists decay `0.5^(age/live)` | No: a 100-byte frame getting through is a different event from a transfer succeeding | Separate likelihoods for different observations |
| M4 | same | Route P = Π 10th-percentile per edge; rank by −log P, then arrival | No: quantiles don't multiply; the objective is not an expected utility | §3.2, §3.3 |
| M5 | `coordinator.rs` | Fabricated contacts: radio 30 %, modem 70 %, internet 99 %, beacon 80 %, heard 70 % × freshness | No: fixed numbers acting as priors, written into the graph as observations | Priors belong in the model, per link class, learned hierarchically |
| M6 | `choose.rs` | Bearer choice by Thompson sampling on Beta(2,1), half-life 1 h | Yes | **Dead code**; its evidence is a second copy of M1 with another memory |
| M7 | `hm-xfer` | Frame loss: EWMA `p ← 0.7p + 0.3·observed` per ACK; ×0.9 on delivery; in memory | No: a 2-frame and a 40-frame over weigh the same; forgotten on restart; not shared with routing | Beta posterior on erasures fed by the same ACK counts |
| M8 | `hm-xfer` | Burst size: smallest n with P(Bin(n, 1−p̂) ≥ need) ≥ 0.9 | Plug-in: ignores estimate uncertainty and fade burstiness; 0.9 is arbitrary | Beta-binomial predictive, minimise expected airtime (§3.7) |
| M9 | `hm-xfer` | No ACK → random exponential backoff (×2, up to 32×), halve window | No: one response for fade, collision and closed band | Classify the cause from evidence (§3.7) |
| M10 | `hm-xfer` | Broadcast overs sized for 30 % loss; 2 rounds; NACK spread over 3 ACK times; suppress after 2 heard | Mixed: NACK suppression is standard (NORM/SRM); 30 % is a constant | Size from listeners' posterior loss |
| M11 | `control.rs` | Beacon interval so beacons take ≤ 2 % of channel; live window 2.5 × interval | Budget is a sound constraint; the window is a heuristic | P(open \| silence for τ) from the link model |
| M12 | `hm-store` | Retry 1, 2, 4 … 60 min, 12 attempts, then hold; suspect timer 24 h; grace 6 h | No: wall-clock schedules ignoring propagation | Retry at predicted openings; suspect when P(lost) justifies a duplicate |
| M13 | config | CSMA p = 64/256, slot 100 ms | Fixed | p ≈ 1/E[contenders] from the channel model |
| M14 | `coordinator.rs` | Cooldown for a failed contact: `delay_after(1)` clamped 5–300 s, per message | No | Falls out of M1 once failures update the posterior |
| M15 | `routing.rs` | Second route for urgent mail if it adds ≥ 0.05 probability | Threshold on a miscalibrated number | Expected-utility test with airtime cost |
| M16 | `control.rs` | Trickle for adverts (k, Imin, Imax), sync queue 64, radio pull 30 min | Trickle is analysed (RFC 6206) | Keep |

Sixteen places. Two have a generative story (M1, M6) and two more are
analysed standard algorithms (NACK suppression in M10, Trickle in M16). Of the
two Bayesian models, one is dead code and the other feeds a decision rule
that is not Bayesian (M4).

---

## 3. Findings

### 3.1 Nothing predicts when a closed link opens — measured

A contact enters the graph three ways: an operator schedule, a beacon or link
heard *now* (live for 20 min or 2.5 beacon intervals), or a neighbour's advert
(valid 2 h). The hourly Beta buckets say how good a link is *at a given
hour*, but no code turns them into "B–C opens tomorrow at 16 UTC". So a
multi-hop route through a link that is closed right now does not exist.

`examples/routing_diurnal.rs`: stations 0–1–2–3, each hop open 3 h/day
(15-minute slots, 70 % each), 300 bd, mail both ways every 2 h, TTL 3 days,
seven days. `CGR (live)` knows every contact open at the moment across the
whole network, which is more than a real station knows.

```
staggered windows (hops open 00-03, 08-11, 16-19 UTC)
algorithm       delivery    airtime     p50(h)    p95(h)   brier
CGR (live)          0.0%       383s        0.0       0.0       -
Bayesian CGR       75.0%      3659s       56.2      68.5   0.670
Epidemic          100.0%      5403s       32.5      50.5       -
Spray L=2           0.0%      1527s        0.0       0.0       -
PRoPHET-like       50.0%      3282s       44.0      54.0       -
MEED              100.0%      5597s       34.0      50.5       -

overlapping windows (06-09, 07-10, 08-11 UTC)
CGR (live)         50.0%      2177s       12.5      40.0   0.639
Bayesian CGR       91.7%      4029s       36.2      68.8   0.669
Epidemic          100.0%      4520s       12.0      34.2       -
MEED              100.0%      5033s       12.0      34.2       -
```

This is the typical HF case: gray-line and daytime paths on different bands
at different hours. As deployed, store-and-forward across hops that are never
open together works only if operators type in schedules. MEED, which learns
a mean waiting time per link, delivers everything, single-copy.

**Fix:** turn the link model into a *contact forecast*: for each link, the
posterior predictive P(open at t) over the next TTL, emitted as predicted
contacts with that probability (§5.3). CGR then routes over forecasts
exactly as it routes over schedules today.

### 3.2 The route objective makes latency a tie-breaker

`label_order` and `route_order` compare `risk_cost` (−log P) first and arrival
second. Any route with a slightly higher conservative P wins, even if it
arrives days later. Because evidence differs a little between hour buckets,
"slightly higher" is almost always true of *some* later slot, so CGR waits.
Experiment with arrival first, risk second (everything else the same):

| scenario | risk first (today) | arrival first |
|---|---|---|
| staggered | 75.0 %, p50 56 h | 87.5 %, p50 36 h |
| overlapping | 91.7 %, p50 36 h | **100 %, p50 12 h** (= epidemic's latency, one copy) |

Neither lexicographic order is right. The decision a custodian faces is:
hand over now on route r, or wait. The principled objective is expected
utility, e.g. `P(delivered before expiry) · value − λ · E[airtime] − μ · E[delay]`,
where P counts the fallback contacts a failed hop still has, not one path.

### 3.3 Quantiles compound, and forecasts are miscalibrated

Per-edge 10th percentiles multiplied along a path are far below both the
10th percentile of the product and the posterior mean (Monte Carlo, 40k draws):

| edge posterior | hops | Π q10 (used) | q10 of product | mean |
|---|---|---|---|---|
| fresh beacon, no evidence: Beta(3.6, 1.4) | 1 | 0.456 | 0.457 | 0.720 |
| | 2 | 0.208 | 0.264 | 0.518 |
| | 3 | 0.095 | 0.159 | 0.373 |
| | 4 | 0.043 | 0.098 | 0.269 |
| 20 ok / 2 failed, advert 0.9 | 3 | 0.509 | 0.575 | 0.685 |

So a fresh three-hop path is scored at a quarter of its expected success, and
multi-hop is penalised more the longer it is. The routing benchmark measures
the result: route forecasts have Brier score 0.64–0.67, worse than always
answering 0.5 (0.25). Switching to the median fixes calibration (0.27–0.37)
but not the objective (§3.2). Pessimism was meant to protect against
over-trusting thin evidence; the Bayesian way to do that is to integrate over
the posterior (posterior predictive) or sample from it (Thompson), not to
take a low quantile per factor.

### 3.4 One Beta, many kinds of evidence

`record_delivery(…, false)` is called for:

* a handoff that failed on the air (link evidence: right);
* **the local radio going down** (`RadioEvt::Down` fails every radio flight):
  a TNC reboot marks every RF link bad;
* a peer that **rejected** the bundle (policy) or answered **busy**: the link
  worked;
* a **suspect timer** firing (no end-to-end receipt in 24 h): evidence about
  the custodian and everything beyond it, not the first hop.

And `true` for a beacon heard (0.25), an internet link coming up, and a
handoff. A generative model has one likelihood per observation type (§5.2);
that separation alone stops a busy mailbox from looking like a dead link.

### 3.5 Three models of the same link

| model | memory | fed by | used for |
|---|---|---|---|
| graph evidence (M1) | half-life 7 d, persisted | handoffs, beacons, links, suspect timer | routing |
| chooser (M6) | half-life 1 h, persisted | handoffs | **nothing** (`choose` has no caller; `estimate` for status only) |
| xfer loss (M7) | EWMA, in memory | ACK counts | burst sizing |

The ACK counts (the richest data: *how many* of *how many* frames got
through) never reach routing; routing's view never reaches the transfer
engine's first burst to a peer. One link posterior would serve all three.

### 3.6 Fixed probabilities posing as observations

The coordinator writes contacts with `success_permyriad` 7000 (modem, for
every destination with mail, every time mail is due), 3000 (radio to an
unheard station), 9900 (internet); beacons use 8000 and heard lists
7000·2^(−age/live). These enter `conservative_probability` as two
pseudo-observations. They are priors, but set by hand, identical for every
station, and re-asserted as fresh observations each time. The modem ones also
stay in the graph and can be used by other messages' routes to stations the
modem has never reached. A hierarchical prior (per bearer class, learned from
all links of that class) replaces all of them with numbers the data sets.

### 3.7 Transfer layer

* **Loss estimate (M7)**: equal weight per ACK whatever the sample size;
  reset on restart; ignores fade correlation. A Beta posterior on erasures,
  updated with (lost, got) counts and discounted, is as cheap and is correct.
* **Burst size (M8)**: need 10 symbols at 20 % loss, 2.9 s frames (HF 300 bd).
  The 0.9 rule sends 15. Minimising expected airtime (a short over leaves a
  small remainder for the next, which the fountain code makes cheap):

  | ACK turnaround | optimal first over | 0.9 rule costs |
  |---|---|---|
  | 1 s | 11 | +17 % airtime |
  | 3 s | 12 | +11 % |
  | 10 s | 13 | +4 % |
  | 30 s | 14 | +0 % |

  And when loss is bursty (fades), the 0.9 rule does not deliver 0.9: with
  per-over success drawn from a Beta with mean 0.8, P(≥10 of 15) is 0.91,
  0.85, 0.81 for concentration 50, 10, 4. A Beta-binomial predictive with the
  fade concentration learned per link gives the right n in both directions.
* **ACK timeout (M9)**: backoff doubles up to 32× on every missing ACK,
  deliberately, because larger overs would worsen congestion. On HF the usual
  cause is a fade, which lasts seconds. Measured one-hop chat on a 0.5 Hz
  Watterson channel: receipt back p50 84 s, **p95 545 s** at 17 dB; 39 s / 157 s
  at 20 dB. The cause is inferable: carrier sensed during our wait (collision
  or hidden terminal), SNR trend in the peer's last ACKs (fade), band
  posterior (closed), time since anything was heard from the peer (gone).
  A small posterior over {fade, contention, closed} picks retry-soon,
  back-off, or park-until-forecast.

### 3.8 Timers that ignore propagation

Retry backoff (1→60 min, 12 attempts, then hourly until expiry), suspect
timer 24 h, grace 6 h, live window 2.5 beacon intervals, holdings pull every
30 min. On HF all of these should key off the link forecast: retry at the
next predicted opening (plus wake-on-hear, which exists and is exactly a
Bayesian update to "open"); declare a station out of reach when P(open |
silent for τ) is low, not after a fixed multiple; fire the suspect timer when
the posterior probability that the custodian lost the bundle, given its
delivery-time distribution, makes a duplicate worth its airtime.

### 3.9 The control budget versus HF windows

Control-plane load test (bounded to ~2–4 % of the channel at every size,
which is the congestion property we wanted):

```
HF 300 bd   N   beacon every   channel   schedules known   links known
            5     21 min        3.6 %         100 %            100 %
           20    121 min        4.2 %          70 %             94 %
           40    239 min        4.1 %          50 %             53 %
```

With 40 stations sharing an HF channel, each beacons every 4 h. A band that
opens for 3 h may pass without a beacon from a given station. Liveness
learned from beacons alone cannot track HF on a busy channel; this is where
the forecast (§3.1) and passive evidence (every frame heard is evidence a
link is open, but only beacons reach the graph today) must carry the load.
Beaconing should also be spent where it is informative: not when the band is
predicted closed, more when the posterior is uncertain.

### 3.10 State bugs found

* **Fixed:** `failed_contacts` (per-message cooldowns) was never pruned, and
  the lookup `entry(id).or_default()` inserted an empty map for every due
  message, so a long-running node kept one entry per message ever handled.
  Now looked up without inserting and pruned when cooldowns end.
* Fabricated modem contacts persist and are reusable by other messages (§3.6).
* Local radio loss is recorded as link evidence (§3.4).
* `Chooser` persists and restores evidence that nothing reads for decisions.

Checked and fine: transfer session IDs (random u16, unique per sender, and
receivers key by sender); fabricated contacts are never advertised (only
beacon-heard radio links and internet links are claimed).

---

## 4. Airtime waste

| Waste | Size | Remedy |
|---|---|---|
| Radio attempts to stations not heard (fabricated 30 % contact) and retries during closed hours | an OFFER + probes + backoff per attempt, every retry, all night | Attempt only when P(open now) · P(success) · value > airtime cost |
| Over-long first overs (0.9 target) | +11–17 % of payload airtime at short turnaround | Expected-airtime burst sizing (§3.7) |
| Misread fades as congestion | not airtime, but minutes of idle channel while the link is up | Cause posterior (§3.7) |
| Key (32 B) + signature (64 B) in every beacon | 2.6 s of a 3.4 s minimal beacon at 300 bd (76 %); 40 % with 16 heard | Send the key on request or every Nth beacon (receivers with the key in trust need a fingerprint only); research option: TESLA-style delayed-key MACs (10–16 B) for most beacons, signature periodically |
| Heard lists repeated unchanged | 7 B per entry, up to 112 B per beacon | Send only changes since the last beacon, with a periodic full list |
| Broadcast overs sized for a fixed 30 % loss | too many on good paths, too few on bad | Size from the listeners' loss posterior |
| Second route for urgent mail on a 0.05 gain of a miscalibrated P | a full duplicate transfer | Expected-utility test with calibrated P |

---

## 5. Fewer, better models

### 5.1 Three models

```
Link  (from, to, bearer)   is it open, and how well does it carry frames when open?
Node  (station)            when it takes custody, does the bundle get through?
Channel (frequency)        how many stations contend, how busy is the carrier?
```

Everything today is an approximation to one of these.

### 5.2 Generative story: link

```
hour-of-day h(t), day d(t)
openness state   o_t ∈ {closed, open}, a two-state Markov chain whose
                 hazards depend on the hour:
                   logit P(open at t+Δ | closed at t) = θ_up · φ(h(t))
                   logit P(closed at t+Δ | open at t) = θ_down · φ(h(t))
                 φ(h) = [1, sin 2πh/24, cos 2πh/24, sin 4πh/24, cos 4πh/24]
                 θ drifts slowly (random walk / discount factor), so the
                 seasons and the solar cycle move the pattern
frame erasure    given open: frames lost with probability ε_t, Gilbert-Elliott
                 (good/fade) so losses come in bursts; ε ~ Beta, discounted
```

Observations and their likelihoods:

| Observation | Likelihood |
|---|---|
| Beacon from A heard at B | o=open and the frame survived: (1−ε) |
| Beacon from A expected (A's interval is known) but not heard | closed, or open and lost: P(closed) + P(open)·ε |
| A's beacon lists B as heard x min ago | link B→A was open at t−x and a frame survived; tempered by trust in A (hearsay) |
| Over of n frames, ACK says k arrived | open, Binomial/Beta-binomial(k; n, 1−ε) |
| No ACK | mixture over {fade, collision, closed} (channel model supplies collision) |
| Handoff complete / refused / busy | open (the link worked; refusal is node evidence) |
| Internet link up / down | open / closed, internet bearer |
| Local radio down | **no observation** of any link |

Inference: forward filtering on the two-state chain (cheap: 2×2 per update),
with the θ weights updated by a Laplace/extended-Kalman step or, simplest
first version, 24 Beta buckets per state transition smoothed by a circular
kernel over hours whose width is chosen by marginal likelihood (a dynamic
generalized linear model in West & Harrison's sense, with discount factors,
is the textbook frame). This replaces M1, M2, M3, M6, M7 and the live window.

### 5.3 Decisions as posterior functionals

| Decision | Today | From the model |
|---|---|---|
| Contacts for routing | now-heard, adverts, manual schedules | forecast: P(open) over [now, expiry] per link, as predicted contacts |
| Route | Π q10, risk then arrival | Thompson: sample θ, ε from the posterior, run CGR on the sample, act on its first hop; or maximise expected utility with P that counts fallback contacts |
| Bearer | dead Thompson chooser | same draw as routing, with configured costs in the utility |
| First burst to a peer | EWMA from past ACKs, 0.9 target | Beta-binomial predictive, minimise expected airtime |
| After a missing ACK | always back off | P(fade), P(contention), P(closed) → retry soon / back off / park until forecast |
| Retry time | 1, 2, 4 … min | next time P(open)·(1−ε) is high, or wake on hear |
| Station in reach? | heard within 2.5 intervals | P(open \| silence) |
| Suspect timer | 24 h | P(lost \| no receipt by t) under the node model, against the cost of a duplicate |
| Beacon interval | 2 % budget | 2 % budget as a hard cap; within it, beacon when the value of information is high (uncertain, plausibly open) |
| CSMA persistence | 64/256 | ≈ 1/E[contenders] from the channel model |
| Broadcast over size | 30 % loss | listeners' loss posterior |

### 5.4 Node and channel models

* **Node**: custody reliability ρ_n ~ Beta, discounted, updated by end-to-end
  receipts (success) and suspect expiries (failure); a delivery-time
  distribution (e.g. log-normal) per destination class for the suspect timer.
  Keeps a lossy custodian from being blamed on its radio link.
* **Channel**: contenders N ~ Poisson with Gamma rate, updated from distinct
  callsigns heard per window and carrier-busy fraction; drives CSMA p,
  beacon spacing, and the collision term in the ACK-timeout mixture.

### 5.5 Where a threshold is the right tool

Hard limits that are policy, not estimates: the channel budget (a fairness
and regulatory constraint), queue and store sizes, max hops, TTL, security
checks, AX.25 timing. Trickle's constants (an analysed algorithm). Keep them.

---

## 6. What is good

* Fountain-coded transfers with need-count ACKs and AIMD: loss-tolerant and
  congestion-safe; the right design for HF.
* A bounded control plane that stays at ~2–4 % of the channel at every size
  measured: the property Meshtastic lacks.
* Durable custody with signed receipts, suspect timers and custody-fail
  notices; wake-on-hear retries.
* Beta evidence with forgetting and hour buckets: the right starting point;
  §5 extends it rather than replacing it.
* Sans-IO transfer engine and channel simulator with Watterson fading, hourly
  band tables, collisions, hidden terminals, clock drift; 15 fuzz targets.

---

## 7. Code structure

| Layer | Crates | Assessment |
|---|---|---|
| Wire and bundles | hm-wire, hm-bundle, hm-ident | Ground-up: typed layouts, borrowed parsing, no panics on input, fuzzed |
| Engines | hm-core, hm-xfer, hm-route, hm-store | Ground-up: sans-IO `Machine`, deterministic RNG, typed errors. `hm-xfer/src/lib.rs` is 1,927 lines in one file; split into outgoing, incoming, broadcast |
| Bearers | hm-bearer, hm-modem-afsk, hm-net | Clean |
| Node | hm-cli `node/` | **Patchwork.** `coordinator` is one `async fn` of ~1,200 lines (235–1442) mixing route policy, fallbacks, fabricated contacts, tokio I/O, store writes and logging. The fallback chain (default route → optimistic radio → direct internet) and the three link models grew fix by fix |

Rust-level patterns are good (newtypes like `Millis` and `Callsign`, enums
for state, `Result` everywhere, no `unsafe` blocks; adding `#![forbid(unsafe_code)]` to each crate would make that a guarantee). The structural fix:

1. `hm-model`: the three models of §5, pure, no I/O, serialisable.
2. `hm-node`: the coordinator's logic as a sans-IO state machine implementing
   `hm_core::Machine` (inputs: radio/link/store/API events and time;
   outputs: commands). Routing, bearer and retry policy become functions of
   the model.
3. `hm-cli`: a thin tokio shell that feeds events in and carries commands out.

Step 2 is also what makes §8's missing tests possible.

---

## 8. Test and simulation coverage

| Condition | Frame / transfer level | Routing (contact trace) | Whole node |
|---|---|---|---|
| Fading (Watterson) | ✅ sweeps, chat latency | – | ❌ |
| Band opens/closes by hour | ✅ `Loss::Hourly` (beacons) | ✅ now, `routing_diurnal` | ❌ |
| Learning when links open | – | ❌ (live variant shows the gap) | ❌ |
| Collisions, hidden terminals | ✅ | – | ❌ |
| Partitions and healing | ✅ | – | ❌ |
| Clock drift and offsets | ✅ | – | ❌ (hour buckets use local clock) |
| Busy channel, control budget | ✅ `control_load` | – | ❌ |
| Mixed HF / VHF / internet failover | – | – | ⚠ real-time tests, minutes long |
| Restart mid-custody | ✅ store tests | – | ⚠ |
| Multi-day behaviour | – | ✅ 7-day traces | ❌ |
| Seasonal drift, solar events | ❌ | ❌ | ❌ |
| Lying trusted station (inflated heard lists) | – | ❌ | ❌ |

The hm-cli integration tests run the real daemon in real time against fake
TNCs (a "band dead" switch), so a day of HF is out of reach. Once the node is
a `Machine` (§7), hm-sim can drive N real nodes through weeks of diurnal
openings in seconds, deterministically, and the diurnal benchmark becomes a
test of the actual stack rather than of the router alone.

---

## 9. Is the network robust?

On a link that is up: yes. At 20 dB SNR with 0.5–1 Hz Doppler spread
nearly every HF transfer completes (at 17 dB and 1 Hz only about half do);
VHF chat arrives with p95 23 s at 30 % frame loss; the control plane stays
within its budget; custody survives restarts.

Across links that come and go, which is what HF is: not yet. Forwarding only
follows paths that exist now or were typed in; forecasts are pessimistic and
miscalibrated; retries and liveness run on wall-clock timers; failures of
different kinds pollute one estimate. Where an internet gateway is in reach
the default route hides this. On a pure-RF HF network it does not.

---

## 10. Plan

| # | Change | Effort | Effect |
|---|---|---|---|
| P1 | Separate observation types (§3.4): local radio down is no evidence; refusal/busy are node evidence; suspect expiry goes to the node model | S | Stops evidence pollution now |
| P2 | Remove fabricated contacts from the graph; use a per-bearer prior inside the model | S | No phantom paths; priors in one place |
| P3 | Route objective: posterior mean (or Thompson draw) per edge, expected-utility ranking with airtime and delay | M | Arrival-first alone: +8–13 pts delivery, p50 56 → 36 h and 36 → 12 h in the diurnal benchmark; calibrated forecasts |
| P4 | Link model v1: Beta erasure fed by ACK counts (replaces EWMA and chooser), persisted, shared with routing | M | One model, uses the best data |
| P5 | Link model v2: two-state availability with smooth diurnal hazards; forecast contacts for CGR; retries at forecast openings | L | Makes multi-hop HF store-and-forward work without manual schedules (0 % → target MEED's 100 %) |
| P6 | Burst sizing by expected airtime with Beta-binomial predictive | S | −4 to −17 % payload airtime |
| P7 | ACK-timeout cause posterior | M | HF p95 latency (545 s at 17 dB) |
| P8 | Beacon diet: key on request, heard-list deltas | S | ~30–75 % of beacon airtime |
| P9 | Coordinator → sans-IO `hm-node`; whole-node multi-day simulations in CI | L | Tests the real stack under HF conditions |
| P10 | Channel model → CSMA p and beacon spacing | M | Fewer collisions at N ≥ 20 |

Order: P1, P2 and P6 are small and independent. P3 and P4 next. P9 before
P5, so the forecasting model is built against a whole-node simulator that
can show it working over weeks.
