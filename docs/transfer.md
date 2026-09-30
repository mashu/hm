# Transfer over the radio

`hm-xfer` moves one whole object (a signed bundle) from one station to
another over a half-duplex radio link. It is a sans-IO machine: frames and
time in, frames and events out.

## One over, one answer

```text
sender   ──[OPEN]─[OFFER]─[DATA]─[DATA]─ … ─[DATA]──▶
receiver ◀──────────────────────────────[ACK: still need n][OPEN]──
sender   ──[DATA]─ … ─[DATA]──▶
receiver ◀──[ACK: done + signed receipt]──
```

- **OFFER** names the object (hash, length, symbol size, precedence).
- **DATA** frames carry RaptorQ symbols (RFC 6330). The receiver needs any K
  of them, K = ⌈length / symbol size⌉, so a lost frame is never sent again:
  the next over carries fresh repair symbols. Each DATA frame says how many
  frames remain in the over, so the receiver knows when it may answer.
- **ACK** says how many symbols the receiver still needs. The final ACK
  carries a receipt: the receiver's Ed25519 signature over the session and
  the object's id. Anyone may hear a broadcast channel; only the receiver's
  key proves who got the object.
- **OPEN**, before the first transfer to a peer, says what each side offers
  (feature bits) and the largest object, symbol and number of parallel
  transfers it accepts. It rides in the first over and next to the first ACK,
  so it costs no turnaround.
- **CLOSE** refuses a transfer (too large, busy until a given time, or
  refused) rather than leaving the sender to retry into silence.

The object hash is checked after decoding; a mismatch discards the decoder
state rather than deliver bad data. Completed transfers are remembered for a
while, so a lost final ACK brings a new ACK, never a second delivery. An
object the receiver already holds is acknowledged at once.

## How many symbols in the next over

Each over costs a turnaround (key-up, the ACK, the next OFFER) and each frame
its airtime. The burst is the one that minimises the expected airtime to
finish, found by dynamic programming over how many symbols remain, under the
Beta-binomial predictive of how many frames will arrive given the link's
frame-loss belief ([models](models.md)). A fountain code makes a short over
cheap to top up, so the optimum is usually smaller than a burst sized to
succeed with high probability, and larger when turnarounds are long. Each
ACK's count goes back to the link model.

## Opening with a probe

On a path the station doubts is open, the first over is a probe: the OFFER
and a symbol or two. A probe costs its airtime whatever happens and saves
the rest of the burst if the path is closed; opening with the burst saves a
turnaround (our key-up and the peer's ACK) if it is open. So the first over
is a probe when

```text
(1 − P(open)) · (burst − probe)  >  P(open) · turnaround
```

in airtime, with `P(open)` the link model's belief at the moment of sending.
A link believed open, as one the station knows nothing about is, opens with
the burst.

## When nothing comes back

An over with no answer is evidence that the path is closed, weighed by how
likely silence was were it open (an ACK lost to a fade, a collision). The
sender follows whether the path is open through the transfer
([`hm_model::Openness`](models.md#is-it-open-now)): hearing the peer says it
is open then; between observations the chance relaxes toward the daily
pattern over the path's correlation time, so a path heard a minute ago is
very likely still open, one heard an hour ago less so; each silent over is
Bayes' rule on the two states. (Hearing the peer once used to make the path
certain to be open for the rest of the transfer, and silence after it
counted for nothing: a sender whose session had opened went on sending into
a closed path for half an hour.)

After each silent over, another is sent only if it is worth it in two ways.
It must be worth its airtime `c` (in units of what completing the transfer
is worth):

```text
P(open) · P(answer | open)  ≥  c
```

And it must be worth sending now rather than stopping and sending again once
the peer is next heard, when the path is known to be open. The over's
airtime is wasted if the path has closed; stopping costs, if the path is
still open, the wait `w` (the value the message loses until the peer is
next heard, which the node works out from its forecast of hearing it) and
reopening the transfer `r` (an OPEN, a key-up, and the symbols the receiver
already has, which it forgets when kept waiting):

```text
c · (1 − P(open))  ≤  P(open) · (w + r)
```

Mail loses little by waiting a beacon interval, so it stops once the path
is about as likely closed as open, and the station waits to hear the peer
([routing](routing.md#or-when-the-next-hop-is-heard)); urgent traffic loses
much by waiting and keeps trying; a transfer nearly done is not given up
lightly.

After a missed ACK the next over is a short probe (one or two symbols),
after a random exponential backoff in units of the last over's airtime; a
window that shrinks on silence and grows on progress keeps a sender that
keeps missing ACKs from filling the channel.

## Frame and symbol sizes

On 1200 bd VHF, symbols are up to 200 bytes. Below 1200 bit/s, symbols are
sized so a DATA frame takes about two seconds on the air: on 300 bd HF, 32
bytes. HF fades come every second or so at 0.5–1 Hz Doppler spread, and a
frame is lost if it meets one, so a shorter frame survives more often, down
to where its header outweighs its symbol. The best size depends on the path:
on one simulated path at 20 dB and 0.5 Hz, 64-byte symbols move 2 kB sooner
(p50 199 s against 271 s); over a week of five stations on paths down to
13 dB and 1 Hz, 32-byte symbols lose a third less airtime to fades. If fades
come as a Poisson process in time, a frame of duration `d` survives with
`e^{−λd}`, and the symbol that minimises airtime per byte grows as the loss
falls (about 140 bytes at 10 % frame loss, 70 at 30 %, 40 at 60 %); choosing
it per path from the loss belief is the next step. Overs on slow links may
last up to 60 seconds: fewer key-ups and ACK round trips for the same
symbols.

## Bulletins

A bulletin (a group post) goes out as a broadcast: no ACKs, which would storm
the channel. Its burst is sized for the number of listeners the channel
belief expects. A listener that could not decode it asks for repair symbols
with a NACK after a random delay, suppressed if it hears another listener
ask first, so one repair over serves them all.

## Airtime limits

A key-up limit protects the transmitter's final amplifier, which heats while
keyed and cools only while idle. The engine keeps a bucket of airtime as
large as the longest key-up allowed (`[radio] max_keyup_secs`, 20 s by
default): every frame of ours is taken from it (overs, answers, and the
node's beacons and control frames), and it refills at half the rate of time
passing, only once the transmitter has fallen silent. An over waits until
the bucket covers it, and none is larger than the bucket, so frames sent
one after another cannot run into a longer key-up; an answer (an ACK, a
CLOSE) never waits, since the station waiting for it would give up. The
node's beacons and control frames also wait for its own frames to go out
rather than follow straight on. In simulation at 300 bd the longest key-up
came down from about 35 s to about 22 s (an answer can add a few seconds to
a full one); half the key-ups are under 4 s.

Channel access (carrier sense) is below the engine: on the KISS path the TNC
does it; the built-in modem has its own p-persistent CSMA.
