# On-air test plan

Everything in hm-net so far has run against simulated channels, a fake KISS
TNC, pseudo-terminals and a virtual sound card. This plan takes it on the air:
first to show that it works between real radios, then to measure how well, and
to compare those numbers with the simulator's (README, "Measured").

The plan is written for two operators, **A** and **B**, each with a licence and
a station. A third station **C** helps in stages 6 and 8. Every stage says what
to set up, what to do, what to write down and what counts as a pass. Record
results in the log at the end, one row per run, and keep the node logs
(`hm node` prints to standard error; redirect it to a file).

## Rules that apply throughout

- **Frequency.** Use a channel your band plan sets aside for packet or digital
  data, at a time it is quiet. Not the APRS frequency. On 2 m in IARU Region 1
  that is one of the packet channels in 144.800–144.990 MHz other than
  144.800; check your national plan. Listen first.
- **Identification.** Every hm frame on a KISS TNC or the built-in modem is an
  AX.25 UI frame whose source address is your callsign, so each transmission
  identifies the station. Use your own callsign with an SSID that is yours to
  use, never a made-up name, on anything with a radio.
- **Content in clear.** hm signs messages but never encrypts them. Send only
  what may be sent in clear under your licence; test traffic is fine
  ("hm-net test 12 from SA0KAM").
- **Power.** Use the least power that makes the path work. Stage 1 is on the
  bench with dummy loads or low power.
- **Transmit time.** The built-in modem keys up for at most 30 s at a time
  (`max_tx`); a hardware TNC or Direwolf has its own limits. Watch the first
  transmissions of every stage and be ready to switch the radio off.
- **Stop** a stage at once if a PTT stays keyed, a transmission lasts longer
  than expected, or anyone else on the channel is disturbed.

## Equipment

| Setup | Radio link | `station.toml` |
| --- | --- | --- |
| **D** Direwolf | Direwolf 1.7 with the radio's audio and PTT; `MODEM 1200`, `KISSPORT 8001` | `[radio]` `kiss = "127.0.0.1:8001"` |
| **T** hardware TNC | NinoTNC, Mobilinkd, TNC-Pi or a TNC-2 in KISS mode on a serial port | `kiss = "serial:/dev/ttyUSB0:57600"` (or `COM3`) |
| **M** built-in modem | a sound-card interface: AIOC, Digirig, SignaLink | `audio = "USB Audio"`, `ptt = "cm108:/dev/hidraw0"` (or `rts:…`, `rigctld`, `vox`) |

Each station also needs the `hm` binary for its platform, a clock set by NTP
(the beacon reports clock offsets), and for stage 7 an internet connection.

## Stage 0: preparation (each station, no transmitting)

1. `hm keygen --call YOURCALL-N` in a new folder; exchange the lines
   `hm whoami` prints, and `hm trust add "…"` the other's line.
2. Set `locator` under `[station]` to your grid square.
3. With setup M: `hm audio-devices` lists the sound card; note its name.
4. Start `hm node`, open the web page, check the station line and the settings.

**Pass:** each station trusts the other; the page shows the right callsign,
locator and radio link.

## Stage 1: on the bench (both stations within a few metres, low power or dummy loads)

1. Set audio levels. Transmit audio should be about a third of full deviation
   (±1.5–2 kHz on FM); received audio should not clip. With setup D or M, send
   a few beacons (`beacon_minutes = 1`) and watch Direwolf's or the other
   node's audio level report.
2. Check the key-up delay: start at `txdelay_ms = 300` and lower it in steps of
   50 ms while frames still decode; then add 100 ms. Write the value down.
3. Send ten chat lines each way from the web page.

**Record:** audio levels, `txdelay_ms`, lines sent and delivered, each line's
delivery time, the key check for the other's beacon (should be "trusted").

**Pass:** 10/10 delivered each way with receipts verified; no PTT left keyed;
each beacon heard and "trusted".

## Stage 2: KISS path over the air (setup D or T at both ends)

The same as stage 1, with the stations at their normal places, a real path
between them, and normal power.

1. `hm listen` on B, then from A:
   `hm send --to B-CALL --text "stage 2 test 1"`, then a longer one of about
   2.7 kB (`--text "$(head -c 2000 /dev/urandom | base64)"`).
2. Twenty chat lines each way from the web page, a few seconds apart.
3. Five mail messages with a subject each way, one of each precedence.

**Record:** per message: size, delivery time (the Sent log shows when it was
queued and delivered), overs (count the sender's transmissions in Direwolf's or
the TNC's log, or by ear), the bearer ("radio"), receipt verified. The average audio SNR if the TNC reports it.

**Pass:** everything delivered, receipts verified; no duplicate in either inbox.

## Stage 3: the built-in modem (setup M) against Direwolf (setup D)

A runs the built-in modem, B runs Direwolf; then swap.

1. Repeat stage 2, steps 2 and 3.
2. Let beacons run for 30 minutes (`beacon_minutes = 5`).

**Record:** as stage 2, and the stations heard with their beacon details on
each page (locator, distance, bearing, clock offset).

**Pass:** Direwolf decodes our frames and we decode Direwolf's: everything
delivered both ways; distance and bearing agree with a map to within a grid
subsquare.

## Stage 4: the built-in modem at both ends (setup M)

1. Repeat stage 2.
2. Channel access: B sends a 5 kB message to A while A sends one to B, both
   queued at the same moment.

**Record:** as stage 2; for step 2, both delivery times and the number of
overs each took.

**Pass:** everything delivered; the two transfers finish without either
station keying up over the other's transmission for more than a moment (listen
on a third receiver, or compare the node logs' timestamps).

## Stage 5: a weak path

Make the path marginal: lower power step by step, or use an attenuator, until
some frames are lost (transfers take more overs; Direwolf's or the TNC's log
shows each transmission).

1. At each step, send five 2 kB messages A to B.
2. Note the point where delivery takes more than three overs, and where it
   fails.

**Record:** power (or attenuation), received SNR if known, delivery time and
overs per message, failures.

**Compare:** the simulator's measured-loss row (2 kB at 7 / 8 / 9 dB SNR:
latency p50 26.1 / 17.2 / 17.1 s). Loss on air is burstier than white noise;
note how much worse it is at the same SNR.

**Pass:** no corrupted message is ever stored (each message delivered is
byte-identical; the signature check guarantees this, so a failure here is a
bug); messages that cannot get through end as "not delivered" with a reason,
not as a stuck transmitter.

## Stage 6: a busy channel (A, B and C)

C sends traffic on the same frequency: another hm node sending to A, or
ordinary packet or APRS traffic from Direwolf.

1. While C transmits every 20–60 s, A and B exchange ten 1 kB messages.
2. C sends to A while B also sends to A (two senders to one station).

**Record:** delivery times, overs, and how often a transmission collided
(heard by a fourth receiver, or inferred from lost frames).

**Pass:** everything delivered; our stations wait for a clear channel (none of
our transmissions starts while C is transmitting, apart from within the
carrier-detect delay).

## Stage 7: radio and internet together

Both stations have radio and internet; A dials B over the internet
(`[[internet.peers]]`) or both dial a hub.

1. With the radio path good, send five messages: they go by radio (default
   costs).
2. Switch B's radio off (on the Settings page, untick "Use a radio"). Send
   five more: after the radio attempts fail, they go by internet.
3. Switch B's radio back on. Send five more: they go by radio again within a
   few messages.
4. Change A's TNC or sound card on the Settings page while a message is queued;
   it goes out on the new link.

**Record:** the bearer and delivery time of each message; how many failed
radio attempts it took before the internet took over, and after the radio came
back, how long until radio carried the traffic again.

**Pass:** all 15 delivered; the switch-over happens without a restart.

## Stage 8: a day on the air

A, B (and C if possible) leave their nodes running for 24 hours with
`beacon_minutes = 10`, and a script sending a chat line every 10 minutes in
each direction through the API:

```sh
TOKEN=$(cat station.token)
while sleep 600; do
  curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d "{\"to\":\"B-CALL\",\"text\":\"soak $(date -u +%H%MZ)\"}" http://127.0.0.1:8080/api/send
done
```

**Record:** lines sent, delivered, failed; delivery time p50 and p95; restarts
(none expected); memory use of `hm node` at start and end; any PTT fault.

**Pass:** at least 99% delivered while both stations were up; no PTT left
keyed; memory roughly flat.

## After the tests

- File each failure as an issue with the stage, the node logs, and the log row.
- Update the README "Measured" table with an "On air" section: the stage 2, 4,
  5 and 8 numbers.
- Feed what the channel looked like (loss pattern at the margin, collisions)
  back into the simulator's channel models where they differ.

## Results log

| Date (UTC) | Stage | Stations and setups | Band, frequency, power | Messages (sent / delivered) | Delivery time p50 / p95 | Overs per message | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| | | | | | | | |
