# Building and testing

Requires Rust 1.90 or newer.

```sh
cargo test --workspace                                  # everything that runs in CI
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps   # API docs, every link resolved
cargo install --path crates/hm-cli                      # the `hm` command
```

Longer experiments are `#[ignore]`d tests or examples; each is listed with
the command that reproduces it in [results](results.md).

## What is checked, and where

Windows and macOS are slow on CI, so the tests run there only for a release
tag (`git tag v0.1.0 && git push origin v0.1.0`) or when the ci workflow is
run by hand (Actions > ci > Run workflow); everything else runs on Linux.

| What | How | Every pull request and push to main (Linux) | Nightly |
| --- | --- | --- | --- |
| Test vectors | `cargo run -q -p hm-bundle --example vectors \| python3 tools/check_vectors.py` (needs `pip install blake3 pynacl cbor2`): an independent Python implementation checks the vectors in the [specification](spec.md) | yes | |
| Simulator vs independent oracle | `HM_SEEDS=10000 cargo test -p hm-sim --release --test oracle` | 100 seeds | 10,000 seeds |
| Simulated channel vs the modem | airtime of AX.25 frames against the modulator's output; carrier-detect delay against the demodulator; the frame-loss table re-measured (`cargo test -p hm-sim --release --test afsk -- --include-ignored`) | airtime, carrier detect | loss table |
| Whole stations on HF | `cargo test -p hm-node --release`: a station reaching one it never hears through a neighbour; the same seed giving the same run | yes | |
| Decoder mutations (no panic, no forgery) | `HM_MUTATIONS=1000000 cargo test -p hm-bundle --release --test mutations` | 5,000 | 1,000,000 |
| Coverage-guided fuzzing | `cd fuzz && cargo +nightly fuzz run <target>`; targets: frame, ack, ctrl, beacon, sync, routed, envelope, binding, bundle, callsign, kiss, ax25, il2p, xfer, stream | compile only | 10 min per target |
| Cross-platform determinism | a pinned trace hash of a reference simulation | Linux; Windows and macOS on release tags | |
| End to end over TCP | a fake KISS TNC relaying frames (with drops and APRS noise) between `hm listen` and `hm send` processes | yes | |
| KISS over serial | pseudo-terminals as serial TNCs: channel-access parameters on opening, frames both ways, the port released on close | Linux; macOS on release tags | |
| Built-in modem link | stations on a virtual radio channel in real time: carrier sense defers to a busy channel, PTT only around transmissions, one key-up per burst, two nodes exchanging mail | yes | |
| Internet links | QUIC stations on localhost: delivery with verified receipts, rejection, impostors and wrong server keys refused, redial after restart | yes | |
| Station nodes | `hm node` instances driven only through the HTTP API: radio delivery, restart, store-and-forward, internet-only nodes, radio failing over to the internet, relays over four stations, a radio-only station reaching an internet one through a gateway | yes | |

The oracle test builds random two-channel networks with every fault type and
replays the simulator's log through channel rules written independently of
the simulator, bit stuffing and carrier sense included. The mutation test
checks that any mutated message which still verifies carries exactly the
original signed bytes.

## Where tests live

Each crate's unit tests sit next to the code (`src/tests.rs` or a `tests`
module); cross-crate and process-level tests are in each crate's `tests/`.
The whole-station harness is `crates/hm-node/tests/support/world.rs`, shared
by the tests and the `hf_days` and `hf_scale` examples.
