# Setting up a station

Build and install the `hm` command (Rust 1.90 or newer):

```sh
cargo install --path crates/hm-cli
```

Then:

```sh
hm setup                                  # KISS, sound-card modem, internet only, or core node
hm trust add "SO5KM-1 8a1e…"              # the line `hm whoami` prints on their side
hm node                                   # radio through Direwolf on 127.0.0.1:8001
```

`hm setup` validates each answer, lists detected sound devices, defaults the
built-in modem to IL2P, and can configure a locator, an authenticated internet
listener, relay and mailbox. It shows a summary before writing and never
overwrites an existing key or configuration. Type `q` at any prompt to leave
without changing files. For scripted provisioning, `hm keygen --call SA0KAM-1`
writes a key and a commented starter file.

The choices:

1. **KISS TNC**: Direwolf over TCP, or a hardware TNC on a serial port
   ([radio](radio.md)).
2. **Built-in modem** on a sound card, with its own PTT ([radio](radio.md)).
3. **Internet only**: no radio ([internet](internet.md)).
4. **Core node**: a hub with no radio that listens on the internet, with relay
   and mailbox on, meant for a computer with a public address (a small rented
   server is typical).

A second profile in the same folder uses matching names:
`hm setup --config core.toml` writes `core.key` and `core.db`, leaving
`station.key` alone.

## Trust

A station verifies the messages of the stations it trusts, and only their
traffic may use its airtime. Exchange the line `hm whoami` prints over a
channel you trust, then:

```sh
hm trust add "SO5KM-1 8a1e…"
hm trust remove SO5KM-1
```

or use the Network view of the [web page](web-and-api.md). A beacon never adds
a trusted station; a beacon whose key differs from the trusted one is logged
as a warning.

New stations trust and dial the public core hub **SA0KAM-0** at
`34.51.161.47:4433` by default. Remove or replace that `[[trust]]` and
`[[internet.peers]]` entry if you do not want it.

## Several stations under one callsign

Every SSID is a station of its own, so you can run more than one node, for
example a home station and a server:

```sh
hm --config home/station.toml keygen --call SA0KAM-1     # one folder and key per station
hm --config server/station.toml keygen --call SA0KAM-2
hm --config home/station.toml whoami                     # SA0KAM-1 <key>
```

Mail to SA0KAM-2 goes to that node only. A `[[trust]]` entry with an SSID
names exactly that station; one without (as from a key made with
`--call SA0KAM`) covers every SSID that has no entry of its own, for a key
used with `ssid = …` under `[station]`. A node that only uses the internet
does not transmit, so it needs no licence; any name of up to 9 letters,
digits, `-`, `/` or `.` works (`KAMHOME`), but use your callsign on anything
with a radio.

Next: [settings](settings.md).
