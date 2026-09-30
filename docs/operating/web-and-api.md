# The web page and the API

`hm node` prints a link such as `http://127.0.0.1:8080/#token=…`. The token is
also kept beside the store (`station.token`); the page asks for it if you open
the plain address.

## The page

- **Chat**: 1:1 conversations by station, like a messenger. The left rail
  lists chats, stations **on frequency** (heard on the radio) and **trusted**
  peers as one-click starts; type a callsign to open anyone else. Enter
  sends. The thread header shows trust, hearability, locator and distance, and
  delivery estimates when known. Badges distinguish pending, in transit,
  delivered, failed, cancelled and received lines. A queued line can be
  dropped before delivery. Chats can be archived in the browser, or their
  inactive local history cleared; pending delivery is never erased by either.
- **Mail**: messages with a subject and precedence, with an inbox and a sent
  log. Every chat line and mail item has a details view with decoded metadata
  and the exact raw signed object, and a control to delete an inactive local
  copy.
- **Bulletin**: group posts. On the radio they are broadcast once to listeners
  on frequency; over the internet they go to linked stations, and others can
  pull missed ones through holdings sync. No per-listener receipts; at most
  four publishes an hour and a small size cap. See
  [bulletin channels](../bulletin-channels.md).
- **Network**: link health (radio, internet, modem), what the station believes
  of each link, the outbound queue, stations heard on the radio (beacons,
  clock offset, locators, distance and bearing), and trusted stations to add
  or remove.
- **Settings**: live settings (delivery, relay, radio, peers, locator) and
  those that take a restart (listen address, open hub, modem, web address,
  store).

The page stays up to date by itself: the node tells it what changed over a
server-sent event stream (`/api/events`). A chat line between two stations
linked over the internet arrives within a second; by radio it takes as long
as the channel does. Chat and mail are both stored and forwarded: a line to a
station out of reach waits, and goes when a way to it opens. History stays in
`station.db` until you delete the local item or clear a chat; queued and
in-transit messages are protected from history cleanup.

## The API

| Method | Path | |
| --- | --- | --- |
| GET | `/api/status` | callsign, key, radio and internet state, what the station believes of each link, stations heard on the radio with their beacons |
| GET | `/api/messages?direction=in\|out\|all&peer=CALL&kind=chat\|mail&limit=n` | newest first, with delivery state and link; `peer` gives one conversation |
| GET | `/api/messages/{id}` | decoded metadata and the exact raw signed object as hex |
| GET | `/api/events` | server-sent events, one `data:` line naming what changed: `message`, `status` or `settings` |
| POST | `/api/send` | `{"to", "text", "subject"?, "precedence"?}` → `201 {"id"}`; without a subject it is a chat line |
| DELETE | `/api/messages/{id}` | cancel a queued outbound message; a second delete, or deleting inactive or received history, removes the local copy |
| DELETE | `/api/conversations/{peer}` | delete inactive local chat history, keeping queued and in-transit messages |
| POST | `/api/read/{id}` | mark an inbound message read |
| GET | `/api/trust` | trusted stations with their notes, and the file they are saved to |
| POST | `/api/trust` | `{"line": "SO5KM-1 8a1e…", "note"?}` (as `hm whoami` prints it) → `201` |
| DELETE | `/api/trust/{station}` | stop trusting a station → `204` |
| GET | `/api/settings` | the settings in use: `live` ones, and those that take a restart |
| PATCH | `/api/settings` | any live fields: `beacon_minutes`, delivery costs, retries, `custody_*`, `receipt_retry_attempts`, `relay` (`enabled`, `mailbox`, limits…), `peers`, `locator` (`""` for none), `radio` (any `[radio]` field); optional `restart_to_change` (`internet_listen`, `open_hub`, `modem`, `http`, `store`) saved for the next start → the new settings, written to `station.toml` |

Every `/api` request needs `Authorization: Bearer <token>`. The API is plain
HTTP and listens on localhost by default; to use it from another machine, put
it behind HTTPS or an SSH tunnel ([internet](internet.md#remote-access-to-the-web-page))
so the token never crosses a network in the clear.
