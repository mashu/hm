# hm-net

A federated store-and-forward network for amateur radio mail, chat, forms and
bulletins over VHF/UHF and HF. Written in Rust; no central server; content in
clear, identities signed.

`hm` is a working prefix until the project has a name.

## Status: Phase 0 (spec, core types, simulator)

| Crate | What it does | State |
| --- | --- | --- |
| `hm-core` | `Millis`, the sans-IO `Machine` trait, station `Input`/`Output`, deterministic RNG | done |
| `hm-wire` | Base-40 callsigns, 18-byte frame header, ACK payload, object ids | done |
| `hm-ident` | Ed25519 identities, signed envelopes, callsign binding records, attestations | done |
| `hm-bundle` | Messages: build, seal, open, verify; receipts; attachment references | done |
| `hm-sim` | Discrete-event simulator: airtime, half-duplex, hidden-terminal collisions, bursty loss, faults | done |
| `hm-xfer`, `hm-modem-afsk`, `hm-rig`, `hm-store`, `hm-node`, `hm-ui` | Phase 1 | not started |

The wire format is in [SPEC.md](SPEC.md), with test vectors verified by an
independent Python implementation.

## Build and test

Requires Rust 1.88 or newer.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

# Test vectors, and their independent check
pip install blake3 pynacl cbor2
cargo run -q -p hm-bundle --example vectors | python3 tools/check_vectors.py
```

## Design rules

- Protocol crates are `no_std` + `alloc` and never read clocks, sleep or do I/O.
  Time and randomness are passed in, so the simulator and the daemon run identical code.
- Signed objects are forwarded byte-for-byte. Ids and signatures cover the raw
  signed bytes, so old nodes can route and verify messages written by newer software.
- The simulator is deterministic: same seed, same run, on every platform. A test
  pins the trace hash of a reference scenario.
- Performance claims come from the simulator or on-air tests, never from estimates.

## Licence

MIT or Apache-2.0, at your option.
