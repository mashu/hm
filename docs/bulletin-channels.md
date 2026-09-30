# Bulletin channels

Bulletins are **RF group posts**, not IRC chat. Shipping 1:1 chat remains custody
transfer with end-to-end receipts; a free-for-all room would burn 1200 bd
half-duplex airtime.

## Implemented (RF broadcast)

| Piece | Behaviour |
| --- | --- |
| Address | `Address::Group` (≤ 32 bytes), e.g. `SK-EMCOMM` |
| Kind | `Kind::Bulletin` |
| Transport | RF: `Dest::Broadcast`. Internet: push to linked peers; late peers can **pull** via holdings SYNC |
| Completions | Publisher marks **Published** after RF send or a successful internet delivery |
| Receipts | **None** (no kind-5, no transfer ACK from RF listeners) |
| Bearer | Radio **and/or** internet (queue without either; goes out when a path appears) |
| Precedence | Routine only |
| Size | Sealed object ≤ 4096 bytes |
| Rate | ≤ 4 publishes / station / hour; ≤ 12 inbound / origin / hour stored |
| Relay | Group destinations are still rejected on the unicast relay path |

The station web page has a **Bulletin** tab: compose (group, optional subject,
body) and a timeline with group filters. API: `POST /api/send` with
`"kind":"bulletin"` and `group` / `to`; `GET /api/messages?kind=bulletin&group=…`.

## Why not IRC

On air, every transfer competes for CSMA slots, a 50% duty-cycle budget, and a
small number of concurrent RX sessions per sender. If *N* stations each send
chatty lines and expect receipts, airtime grows roughly with *N* × overs ×
acks. Bulletins are rare posts: publish once, many listeners read, no receipt
storm.

## Still out of scope

- Internet / modem **flood** of every bulletin to the world (only linked, trusted peers)  
- Relaying group holdings as a separate custody graph  
- Per-group publish allowlists / subscriptions  
- Optional “heard by N” acknowledgements  
- Modem (VARA/ARDOP) bulletin path 

## References

- Address and kind tags: [the specification](spec.md) § bundles  
- `Address::Group`, `Kind::Bulletin`: `crates/hm-bundle/src/types.rs`  
- Broadcast xfer: `Command::Broadcast` in `crates/hm-xfer`  
