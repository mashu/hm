# Over the internet

A node can reach other stations by radio, over the internet, or both:

```toml
# A home station with radio that also keeps an internet link to a server
[[internet.peers]]
station = "SO5KM"
address = "hm.example.org:4433"
```

```toml
# A server without a radio that trusted stations connect to
[radio]
enabled = false

[internet]
listen = "0.0.0.0:4433"
```

From the command line: `hm node --peer SO5KM=hm.example.org:4433`, or
`hm node --no-radio --listen 0.0.0.0:4433`.

Internet links are QUIC connections authenticated with the station keys
themselves: by default only mutually trusted stations connect, and each link
is bound to a callsign. There is no certificate authority and no central
server; any node can listen, dial, or both. A listener may set
`internet.open_hub = true` to accept any dialer that presents a valid station
certificate (and its SYNC); home stations must still trust the hub.

Station messages stay cleartext inside their signed bundles: QUIC protects the
link, but trusted relays and anyone with access to the store can read them.

## A core node

A core node is a hub home stations dial. Use a separate configuration so it
does not touch your home station's files:

```sh
hm setup --config core.toml   # option 4; writes core.toml, core.key, core.db
hm --config core.toml node
```

Option 4 writes this shape (radio off, listening, relay and mailbox on); you
can also write it by hand:

```toml
[station]
key = "core.key"
store = "core.db"
http = "127.0.0.1:8080"

[radio]
enabled = false
beacon_minutes = 0

[internet]
listen = "0.0.0.0:4433"
open_hub = true                 # any station may dial in; homes still trust this hub

# Optional: a node this server dials. Accepted connections carry both ways,
# so a public listener needs no peer entry for every client.
[[internet.peers]]
station = "OTHER-1"
address = "other.example.net:4433"

[relay]
enabled = true
mailbox = true
```

Point DNS at the server and allow **UDP 4433** through its firewall. A home
station connects with:

```toml
[[internet.peers]]
station = "HUB-1"
address = "node.example.net:4433"
```

QUIC carries both ways once either side connects. If a home node cannot
accept inbound UDP, it can dial the default public core (`SA0KAM-0`,
`34.51.161.47:4433`); there is no automatic NAT traversal or public peer
directory yet. Do not put UDP 4433 through an HTTP reverse proxy.

## Remote access to the web page

The web page and API are separate from the station links: plain HTTP with a
random 192-bit bearer token, meant to stay on loopback. For remote access,
terminate HTTPS in front of it. Caddy can add a second authentication layer:

```caddyfile
node.example.net {
    basic_auth {
        operator {$HM_WEB_PASSWORD_HASH}
    }
    reverse_proxy 127.0.0.1:8080
}
```

Generate the password hash with `caddy hash-password`, set
`HM_WEB_PASSWORD_HASH` for Caddy, and expose only Caddy's TCP 443 (and TCP 80
if used for certificate issuance). The node's bearer token is still required
after Basic authentication. The token file is owner-only on Unix; the node
refuses a weak or group- or world-readable token file. To revoke browser
access, stop the node, remove `station.token`, and restart it to generate a new
token.

An SSH tunnel avoids exposing the web service at all:

```sh
ssh -L 8080:127.0.0.1:8080 user@node.example.net
```

then open the token-bearing URL at `http://127.0.0.1:8080`.
