# Settings: `station.toml`

Everything a station needs to know lives in one TOML file in its folder:
`[station]`, `[radio]`, `[internet]`, `[modem]`, `[delivery]`, `[relay]`,
planned `[[contact]]`s and the trusted stations as `[[trust]]` entries. Every
setting has a default. `hm setup` writes the chosen settings; `hm keygen`
writes a commented starter file. The key stays in `station.key` (readable by
you only) and the web page's access token in `station.token`. Paths in the
file are relative to the file.

```toml
[station]
key = "station.key"
locator = "JO89xi"          # sent in beacons

[radio]
kiss = "serial:/dev/ttyUSB0:57600"
beacon_minutes = 10         # 0 turns the beacon off
max_keyup_secs = 20         # longest the transmitter stays keyed at a time
duty_cycle_percent = 50     # long-run share of the time on the air

[internet]
listen = "0.0.0.0:4433"

[[internet.peers]]
station = "SO5KM"
address = "hm.example.org:4433"

[delivery]
radio_cost = 5.0            # a minute of radio airtime
internet_cost = 2.0         # an internet attempt
modem_cost = 5.0            # a minute of ARQ modem airtime

[relay]
enabled = false
mailbox = false

[[trust]]
station = "SO5KM-1"
key = "8a1e…"
note = "Jan"
```

Every `hm` command reads `station.toml` from the current folder, or the file
given with `--config`. Command-line options override the file for one run
(`hm node --help` names the setting each one overrides) and the node logs
which ones did. Unknown settings are errors, so a typo is reported rather
than ignored.

## `[delivery]`: what sending costs

Costs are in hundredths of a delivered message's value, and they are what
route choice weighs against a route's chance and speed
([routing](../routing.md)):

| Setting | Default | Meaning |
| --- | --- | --- |
| `radio_cost` | 5.0 | a minute of radio airtime on a quiet channel; dearer as others keep the channel busy |
| `internet_cost` | 2.0 | an attempt over the internet |
| `modem_cost` | 5.0 | a minute of ARQ modem airtime |
| `retry_first_secs`, `retry_max_secs`, `retry_attempts` | 60, 3600, 12 | the soonest a failed message is planned again, doubling; how many attempts before a relay gives a holding back |
| `custody_grace_secs` | 21600 | the origin keeps a copy on offer this long after handing a message on |
| `custody_suspect_secs` | 86400 | the longest the origin waits for an end-to-end receipt before taking custody back (it decides how long within this; see [custody](../custody.md)) |
| `receipt_retry_attempts` | 24 | attempts for end-to-end receipts |

A message waits when a better chance is forecast later, and goes at once when
its next hop is heard; the retry settings bound how soon a failed attempt is
planned again, not when it goes. Our own messages are kept until they expire.

## `[relay]`

Relaying is opt-in.

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | false | accept custody for traffic to other stations and forward it |
| `mailbox` | false | hold traffic until its intermittently connected recipient comes in reach |
| `max_holdings`, `max_bytes` | 256, 16 MiB | how much is held for others |
| `max_hops` | 8 | hop limit |
| `airtime_budget_secs` | 300 | radio airtime one bundle may use along its route |
| `control_airtime_fraction` | 0.02 | share of the channel the stations in reach give control traffic, together |

Relay custody requires a verified origin in the local trust list, which is
also the list of stations allowed to use this station's airtime. The final
destination need not be listed at every hop. Removing the origin from trust
stops any queued relay holding from being forwarded.

## Retired settings

`[delivery] evidence_half_life_secs` and `[relay] urgent_min_gain` are
accepted and ignored, so that older files still load: what is learned about
links keeps its own memory, and urgent traffic goes two ways when that is
worth its cost.

## Changing settings while the node runs

The node applies these at once, without a restart:

- **trusted stations**: on the web page, with `hm trust add/remove`, or by
  editing `[[trust]]`; a station taken off the list loses its internet link
  at once;
- **delivery** costs, retries and custody timers;
- **relay and mailbox**;
- **internet peers**: which stations the node dials;
- **radio**: everything under `[radio]` (TNC or sound card, PTT, key-up
  delay, channel access, `max_rounds`, `max_keyup_secs`, `duty_cycle_percent`,
  radio on or off);
  the node closes the
  old link and opens the new one, and mail on its way over the old link is
  retried on the new one;
- **beacon interval** and **grid locator**.

Changes made on the web page are written to `station.toml`, keeping its
comments and layout. The node also notices when the file is edited by hand,
within a second or two. A file that does not parse is logged and ignored, and
the node keeps the settings it was using. The internet listen address,
open-hub flag, ARQ `[modem]`, web address and store take a restart; the
settings page edits them too and saves them for the next start. Settings given
on the command line stay in force for that run, even when the file changes;
the web page lists them.
