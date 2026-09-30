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
sender keeps a posterior that the path is open, updated after every silent
over, and stops when another over is worth less than its airtime:

```text
P(open) · P(answer | open)  <  price of airtime · airtime of a probe
```

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

A duty-cycle budget with a burst allowance protects the transmitter's
finals. Channel access (carrier sense) is below the engine: on the KISS path
the TNC does it; the built-in modem has its own p-persistent CSMA.
