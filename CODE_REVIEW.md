# hm-net — critical design & code review

Reviewed commit `c65ab0d` (branch `main`, up to date with origin) on 2026‑09‑28.
Scope: whole workspace (13 crates, ~33k LoC Rust), `SPEC.md`, README, CI.
Toolchain: rustc/cargo 1.94.1.

The verdict up front: **the architecture is genuinely good** — clean, dependency‑ordered,
`no_std`+`alloc` sans‑IO protocol crates with a deterministic simulator and independent
test vectors. That is a strong foundation and better than most amateur‑radio stacks.
The weaknesses are concentrated in three places: **(1) the tree does not currently pass
its own CI**, **(2) the transfer/congestion behaviour has a positive‑feedback loop that is
dangerous on exactly the slow HF links this project targets**, and **(3) "HF‑primary" is a
stated goal that the code and simulator do not actually model or configure** — everything is
tuned for VHF 1200 bd.

---

## 0. Reproduced failures on `main` (fix before anything else)

| # | What | Evidence |
| --- | --- | --- |
| **F1** | `cargo clippy --workspace --all-targets -- -D warnings` **fails**: `clippy::nonminimal_bool` at `crates/hm-xfer/src/lib.rs:1393`. | Ran locally; also the reason the public CI `lint`/`test` runs are red on this branch's history. |
| **F2** | `cargo test --workspace` **fails**: `hm-cli` test `four_internet_nodes_relay_end_to_end_without_flooding` times out; relays reject with `"not addressed to SM0R1; relay disabled"`. Deterministic (reproduced twice). | GitHub CI run #64 (`c65ab0d`): `test (ubuntu, stable)` and `test (ubuntu, 1.90)` both **failure**. |
| **F3** | `hm-cli` test `mail_goes_through_vara` is **flaky** (timed out once, passed once) on the simultaneous‑call path. | Local runs. |

CI has actually been red on `main` for most recent commits (runs #59, #60, #62, #63, #64
failed; only #61 green). The project's own rule — *"performance claims come from the
simulator or on‑air tests"* and *"cargo clippy … -D warnings"* — is not being enforced on
merge. **Recommendation: make `main` a protected branch that requires green `lint`+`test`.**

### F1 fix (one line)
`crates/hm-xfer/src/lib.rs:1392‑1402`, the `CloseReason::TooLarge` guard:

```rust
            CloseReason::TooLarge
                if self
                    .peers
                    .get(&from)
                    .is_none_or(|(open, _)| o.len <= open.max_object) =>
```

### F2 root cause — two sources of truth for relay settings
Commit `78ed73e` moved relay config into the live‑settings object (`Live.relay`) and made
the acceptance gate read `live.get().relay` (`accept.rs:162`). But `NodeConfig.relay` still
exists and is still read for the **advertised flags** (`sync.rs:149`,
`scheduled_advert`). The integration test sets `NodeConfig.relay` and leaves `Live.relay`
defaulted, so relays now come up disabled → the whole four‑hop delivery stalls.

In production `Live::from_config` and `NodeConfig.relay` happen to be built from the same
`config.relay`, so they *usually* agree — but they can drift: when an operator toggles relay
off/on live (API/web), the gate follows immediately (good) while **startup scheduled CONTACT
adverts keep advertising the old relay/mailbox flag** (`scheduled_advert` reads `cfg.relay`).
That means a node can advertise itself as a relay in the contact graph after relaying is
turned off, attracting custody it will then refuse.

**Fix:** delete `NodeConfig.relay` and have every advertised‑flag path read
`live.get().relay` (as the beacon path in `radio.rs:213` already does). Then fix the test to
drive relay through `Live`. One field, one source of truth.

### F3 — ARQ simultaneous call
`arq.rs:530‑566`: when both stations dial in the same second, the non‑winner pushes its
`req` onto `waiting` and the connection becomes "theirs". Whether the queued call is ever
serviced depends on hang‑up/redial timing, so the round‑trip test sometimes times out. A
real VARA/ARDOP modem arbitrates this in hardware, but the fake modem in the test does not,
so the test is racy. Add explicit collision arbitration (lower callsign yields, or bounded
retry of `waiting`) and make the test deterministic.

---

## 1. Architecture & code quality (the good part)

- **Crate hierarchy is exemplary.** `hm-core → hm-wire → hm-ident → hm-bundle →
  hm-xfer/hm-route → hm-store → hm-cli`, plus leaf bearers. Protocol crates are
  `no_std + alloc`, never touch clocks/IO, and drive through a sans‑IO `Machine`. The
  simulator and daemon run identical protocol code — this is the right design for a
  research protocol and it is executed well.
- **Wire codecs are uniformly defensive**: fixed‑layout `decode` reject trailing bytes,
  bound counts, and round‑trip. Every one has a `decode_never_panics` proptest. Callsign
  base‑40 packing correctly rejects interior zero digits. Ed25519 uses `verify_strict`.
  Envelope ids/sigs cover raw bytes so forward compatibility actually holds (there is a test
  proving a future field survives relay+verify). This is careful work.
- **Determinism discipline** (own RNG, pinned trace, independent Python vector checker,
  mutation + oracle tests) is well above typical.

**Structural smells (maintainability, not bugs):**
- `coordinator.rs` (1267 lines, one giant `tokio::select!` loop), `api.rs` (1173),
  `control.rs` (710), `arq.rs` (746) are large; the coordinator loop mixes routing,
  bearer selection, custody, sync and status. Consider extracting the per‑due‑message
  delivery decision into a testable function that takes `(record, graph, links)` and returns
  an action, so it can be unit‑tested without spinning up four nodes over TCP.
- `index.html` is 2432 lines inline in the crate. Fine for now; will hurt later.
- `hm-route` and `hm-store` are `std` while README says "protocol crates are `no_std`".
  They are policy/persistence, not wire, so this is defensible — but the README sentence
  overclaims; tighten it.

---

## 2. Protocol / reliability / congestion — the substantive findings

Ordered by how much they matter for an HF store‑and‑forward network.

### P1 — Loss‑estimator + burst‑sizing form a congestion *amplifier* (highest priority)
`hm-xfer`: on every missing ACK, `on_ack_timeout` drives the per‑peer loss estimate toward
100 % (`*p = (*p*7 + 1000*3)/10`, `lib.rs:950`); `burst_size` then uses that estimate to send
**more** redundant symbols next over. A shared channel that is merely *busy* (not lossy)
produces ACK timeouts → inflated loss → bigger bursts → more airtime → more collisions →
more timeouts. That is textbook congestion collapse, and it bites hardest on a slow,
half‑duplex, shared HF channel with hidden terminals.

`hear_traffic` mitigates *only* when the interfering traffic is decodable hm DATA frames
addressed elsewhere (it pushes the ACK deadline out). It does **not** defer for: SSB/CW/other
users, hm frames that failed FEC (the common case in the noise you care about), or a KISS TNC
holding the over while its own CSMA waits. In all those cases a busy channel still looks like
loss.

**Fix direction:** separate "no ACK because lost" from "no ACK because I never got to
transmit / channel busy". Only feed *observed* per‑over loss (the `sent`‑vs‑`got` path in
`on_ack`, which is already correct) into the estimate; do **not** let a bare timeout inflate
loss. Add real channel‑busy sensing (carrier detect / KISS TX‑complete) before starting the
ACK timer, and back off *burst size* — not just retry timing — as congestion signal, i.e.
AIMD on `n`, not only exponential backoff on when.

### P2 — "HF‑primary" is not modelled or configured anywhere
The README leads with HF, but:
- `hm-xfer::Config` has exactly one constructor, `vhf_1200` (1200 bps, 200‑byte symbols,
  300 ms TXDELAY, 1.5 s guard). There is no HF profile (300 bd, long key‑up, long guards,
  small symbols, tight duty cycle). Everything downstream inherits VHF‑1200 timing.
- `hm-sim` channel loss models are Bernoulli, Gilbert‑Elliott, an operator‑supplied
  `Hourly[24]` table, and a **measured AFSK‑1200‑in‑white‑noise** curve. There is **no HF
  channel model**: no multipath/delay spread, Doppler/frequency spread, Watterson‑style
  fading, flutter, or NVIS behaviour. The `Hourly` table is not grounded in any measurement.
- Symbol size is fixed at 200 bytes and the object is zero‑padded to `k·T`. A 119‑byte chat
  bundle (the common case) is padded to 200 bytes and sent as one 200‑byte symbol — **~40 %
  of the smallest, most frequent transmissions is zero padding.** On HF airtime that is
  expensive. Adapt `T` to `min(object_len, peer.max_symbol)` for small objects, or pack
  small objects without RaptorQ overhead.

So the headline reliability numbers ("100 % delivered", latency percentiles) are all
VHF‑1200 white‑noise results. They should not be read as HF performance, and the README's
"Measured" table should say so explicitly. **Add a Watterson (or at least a two‑path +
frequency‑selective‑fade) HF channel to `hm-sim` and an HF `Config`/`RadioParams` profile
before trusting any HF claim.**

### P3 — Head‑of‑line blocking: one active transfer per port
`hm-xfer` holds `active: Option<Outgoing>` — a single outbound transfer per radio port, with
a strict precedence queue. A single unreachable peer holds the port through `max_rounds`
(12) overs with exponential backoff (`lib.rs:945‑958`) before failing; on VHF‑1200 that is
minutes, on HF far longer. Meanwhile every other station's traffic waits. For a mailbox/relay
serving several intermittent peers this is a real availability problem. Consider interleaving
overs across peers, or yielding the port after N failed overs to a peer while its backoff
runs.

### P4 — Header/airtime overhead is heavy for HF
Every on‑air frame carries an 18‑byte hm header **plus** a 16‑byte AX.25 UI wrapper (the
source callsign appears in both). At 300 bps that fixed ~34 bytes is ~0.9 s per frame before
any payload. The hm header duplicates the AX.25 addressing on the KISS/AFSK path. On a
dedicated HF profile you likely want a compact header mode that drops the redundant AX.25
addresses (identification can be met once per key‑up, not per frame), or a much larger symbol
size so the header amortises.

### P5 — IL2P has no end‑to‑end frame check; RS can miscorrect silently
The built‑in modem's IL2P path (`hm-modem-afsk/src/il2p.rs`) follows Direwolf 1.7 / NinoTNC:
Reed‑Solomon parity, **no trailing CRC**. The type‑1 header is protected by only RS(15,13)
(t=1). When errors exceed the correcting power, RS can *miscorrect* to a valid‑looking wrong
codeword, delivering a frame with a wrong callsign/session/length to the protocol. The
transfer layer catches wrong *payload* via the 32‑byte object hash (good, and tested), but a
miscorrected header sends a good symbol into the wrong `(src, session)` bucket, which later
fails the hash and triggers a **full decoder reset** (`Incoming::reset_decoder`) — wasted
airtime, and on a busy session a possible partial DoS. The AX.25/HDLC path is safe (CRC‑16).
Consider a small application‑level frame check on the IL2P path, or prefer HDLC when the link
budget allows. At minimum, document that IL2P trades integrity for FEC.

### P6 — Contact‑graph evidence is optimistic and game‑able
`hm-route`:
- **Beacons double‑count.** Each verified beacon does `record_delivery(...true)` for the
  origin *and* one success per station in its "heard" list, every 10 minutes. The same
  reception is therefore counted ~6×/hour, and there is **no negative evidence** for missed
  beacons — so a link that beacons but cannot pass bundles accrues a high posterior. Beacon
  reception ≠ bundle‑transfer success; weight them differently and add decay/absence as
  negative evidence. Also, `observe_beacon` sets `evidence.at = observed_at − age` for heard
  entries, which can move a link's evidence timestamp *backwards*.
- **Signed CONTACT adverts are self‑reported** with only `advertised_strength = 2`
  pseudo‑counts, but for a remote edge they are the *only* signal. A trusted‑but‑hostile (or
  buggy) node can advertise high success/capacity and pull custody toward itself — a
  blackhole. There is no first‑hand penalty when its handoffs then fail beyond the local
  edge. Cap advert influence and weight first‑hand evidence strictly above hearsay.
- **No band/frequency/mode/UTC‑hour bucketing by default** (`bucket_by_utc_hour = false`).
  HF reliability is dominated by diurnal propagation; without hour buckets the posterior
  averages a 20 m daytime opening with a dead nighttime band. For HF this should be on, and
  ideally keyed by band.

### P7 — Route search can throw away good routes it already found
`plan_routes`/`find_candidates` (`routing.rs`) is a best‑first label search with **no
dominance pruning**; on hitting `max_labels` (16 384) it returns `Err(SearchLimit)` and
**discards every route already found**. A realistic topology (a well‑connected internet core
plus a weak RF last hop) can blow the label budget and yield *no route* even though a good one
was in hand. Fix: keep Pareto‑dominant labels per `(station, arrival)` and, on hitting the
limit, return the best routes found so far instead of an error.

### P8 — Relay "Duplicate ⇒ custody accepted" can strand a bundle
`accept.rs:185` returns `Acceptance::Duplicate` when a record for the id already exists **in
any state** (including `Failed`, `Delivered`, `InTransit`), and `custody_accepted()` treats
Duplicate as success, so the sender is handed a signed custody receipt
(`hm-net serve_connection` signs on `Duplicate`) and stops holding the bundle. If the relay's
copy is terminal (`Failed`) nothing forwards it. The custody‑fail/suspect machinery is
designed to recover this, but it depends on custody‑fail delivery back over a possibly‑down
link and a 24 h suspect timer. Safer: on a re‑offer of a `Failed`/terminal holding, either
revive it (`Queued`, reset attempts) or reject so custody stays upstream.

### P9 — Retry policy is wall‑clock attempts, not deadline/contact‑driven
`hm-store::RetryPolicy` default: 12 attempts, 1 min doubling to 1 h → a message is `Failed`
after ~6 h of wall time regardless of its TTL. For DTN over HF where the next contact window
may be *tomorrow*, this abandons deliverable traffic. Retry should be driven by bundle TTL
and predicted contacts, not a fixed attempt count against wall time. (`reclaim_custody`
resets `attempts = 0`, so the count is at least bounded by TTL on the reclaim path — but the
origin outbox path is not.)

### P10 — Broadcast/bulletin transfer has no repair or suppression
Bulletins are one over (`BROADCAST_MAX_ROUNDS = 2`) at a fixed 30 % assumed loss, no NACK,
no receiver‑driven repair. Fountain coding is *ideal* for reliable multicast (listeners could
request K‑of‑N repair with NORM/SRM‑style timer suppression), but that is unused. On a lossy
HF broadcast, listeners that miss symbols simply don't get the bulletin until they pull it
over unicast holdings sync. Acceptable for now, but the fountain code is the right tool to do
this well later.

---

## 3. Security / trust observations (mostly by‑design, a few worth noting)

- **Cleartext by design** is a deliberate amateur‑radio constraint (encryption is illegal on
  ham bands in most regions) — correct, and clearly documented. Signatures authenticate.
- **Unauthenticated CTRL/ACK on the RF path.** `hm-bearer::unwrap` does not check that the
  AX.25 source equals the hm header source, and OPEN/CLOSE/ACK‑need are unsigned. A forged
  `CLOSE Refused` for an in‑progress `(peer, session)` makes the sender give up
  (`Failure::Refused`); a forged tiny‑`max_object` OPEN makes the next transfer fail
  `TooLarge` "at once". This is inherent to an open broadcast channel and the *payload*
  custody is protected by receipts, but a jammer can cheaply disrupt sessions. Worth a note
  in SPEC §7 and, if feasible, treating a CLOSE/refused as advisory (retry later) rather than
  terminal.
- **Open‑hub identity is self‑asserted.** With `internet.open_hub`, a dial‑in's callsign
  comes from its own certificate CN and its receipts are then verified against that same
  self‑presented key (`hm-net`). That is fine for *next‑hop custody* semantics (the receipt
  proves "the entity I just talked to holds the bytes") but the hub cannot bind that to a
  *known* operator, and there is no per‑cert rate limiting — an open hub is DoS‑able by many
  certs / many 1 MiB streams (`serve_connection` spawns unbounded per‑stream tasks). Add
  per‑peer connection/stream/byte quotas on the open‑hub path.
- **`SignedBinding::supersedes` compares only callsign + seq**, not key — correct as used
  (records are self‑signed so a higher seq must be signed by the same key to verify), but the
  method name invites misuse; assert same‑key in the doc/contract.

---

## 4. Test / fuzz / CI coverage gaps

- Nightly fuzz matrix omits the **`ctrl`** and **`il2p`** targets even though the targets
  exist (`nightly.yml:42`). IL2P is exactly the RS/miscorrection surface that most needs
  fuzzing. Add both.
- No fuzz/property target for: the **SYNC** messages (contact/filter/want/offer), the
  **`HMR1` routing wrapper** (`hm-wire::route`), the **hm-net/ARQ stream framing**
  (`HMD0/HMC0` length‑prefixed reader), or the **HTTP/JSON API**.
- No test exercises the P1 congestion loop (many senders, busy channel, watch burst sizes
  grow). The `busy_channel` sim measures collisions but not the loss‑estimate feedback.
- Excellent existing coverage worth keeping: oracle replay, 1e6 decoder mutations, Direwolf
  interop, deterministic cross‑platform trace.

---

## 5. Prioritised recommendations

**Must‑fix (tree is red / correctness):**
1. F1 clippy one‑liner; F2 collapse `NodeConfig.relay`↔`Live.relay` to one source of truth;
   F3 deterministic ARQ collision handling. Then protect `main` on green CI.

**High (reliability on the target medium):**
2. P1 — stop letting bare ACK timeouts inflate loss; add real channel‑busy sensing; AIMD on
   burst size. This is the single most important protocol change for HF.
3. P2 — add an HF `Config`/`RadioParams` profile and a Watterson‑style HF channel to
   `hm-sim`; re‑run the "Measured" table and label VHF vs HF. Adapt symbol size for small
   bundles.
4. P7 — return best‑found routes on search‑limit; add Pareto pruning.

**Medium:**
5. P3 head‑of‑line blocking; P6 evidence double‑count / advert trust weighting + HF hour
   bucketing; P9 TTL/contact‑driven retry; P8 terminal‑holding re‑offer handling.

**Lower / hardening:**
6. P4 compact HF header; P5 IL2P integrity note or app‑level check; P10 multicast repair;
   §3 open‑hub quotas and unauthenticated‑CTRL note in SPEC; §4 fuzz targets.

Nothing here undermines the core design — the layering, sans‑IO discipline, signed‑custody
model and deterministic testing are the hard parts and they are done well. The gap is between
the *VHF‑1200 reality the code is tuned and measured for* and the *HF‑primary mission the
README states*, plus a currently‑red CI that is hiding a real relay‑config regression.
