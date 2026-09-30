# Four relaying stations on one laptop

This builds an internet-only chain `SA0KAM → SM0R1 → SM0R2 → SO5KM-1`. QUIC
carries the same authenticated bundles and control traffic as a radio link,
so this is the quickest way to try multi-hop routing and end-to-end receipts.
It does not simulate radio loss; the [simulators](../simulation.md) do.

Build the binary, make four station directories, and exchange all four
identities:

```sh
cargo build -p hm-cli
HM=target/debug/hm
mkdir -p lab/{a,r1,r2,b}
$HM --config lab/a/station.toml  keygen --call SA0KAM
$HM --config lab/r1/station.toml keygen --call SM0R1
$HM --config lab/r2/station.toml keygen --call SM0R2
$HM --config lab/b/station.toml  keygen --call SO5KM-1

A="$($HM --config lab/a/station.toml whoami)"
R1="$($HM --config lab/r1/station.toml whoami)"
R2="$($HM --config lab/r2/station.toml whoami)"
B="$($HM --config lab/b/station.toml whoami)"
for pair in \
  "lab/a/station.toml|SA0KAM" \
  "lab/r1/station.toml|SM0R1" \
  "lab/r2/station.toml|SM0R2" \
  "lab/b/station.toml|SO5KM-1"
do
  cfg=${pair%%|*}; self=${pair#*|}
  for identity in "$A" "$R1" "$R2" "$B"; do
    [ "${identity%% *}" = "$self" ] ||
      $HM --config "$cfg" trust add "$identity"
  done
done
```

In both `lab/r1/station.toml` and `lab/r2/station.toml`, turn relaying on:

```toml
[relay]
enabled = true
mailbox = true
```

Start these in four terminals, in this order:

```sh
# B
target/debug/hm --config lab/b/station.toml node --no-radio \
  --http 127.0.0.1:8104 --listen 127.0.0.1:4204 --beacon-minutes 0

# R2
target/debug/hm --config lab/r2/station.toml node --no-radio \
  --http 127.0.0.1:8103 --listen 127.0.0.1:4203 \
  --peer SO5KM-1=127.0.0.1:4204 --beacon-minutes 0

# R1
target/debug/hm --config lab/r1/station.toml node --no-radio \
  --http 127.0.0.1:8102 --listen 127.0.0.1:4202 \
  --peer SM0R2=127.0.0.1:4203 --beacon-minutes 0

# A
target/debug/hm --config lab/a/station.toml node --no-radio \
  --http 127.0.0.1:8101 --listen 127.0.0.1:4201 \
  --peer SM0R1=127.0.0.1:4202 --beacon-minutes 0
```

Each process prints its token-bearing web URL. Open A's (port 8101) and B's
(8104), wait until the links appear under Network, then send from A to
`SO5KM-1`. A's line passes through **Queued** and **In transit** and becomes
**Delivered** only after B's signed receipt has come back along the chain.
Stop R2 before sending to see the message wait; start it again and the
message completes. "Drop queued" in Chat or Mail cancels a message that is
still queued here.

For a three-station test, use only `a`, `r1` and `b`: start B as above, R1
with `--peer SO5KM-1=127.0.0.1:4204`, then A with `--peer SM0R1=127.0.0.1:4202`,
with relaying on R1 only. **Delivered** shows that A got B's end-to-end
receipt, not only R1's custody receipt.
