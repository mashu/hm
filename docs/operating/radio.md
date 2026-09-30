# On the radio

A station reaches the radio through a KISS TNC (Direwolf over TCP, or a
hardware TNC on a serial port) or through its own modem on a sound card. Both
send the same AX.25 UI frames, so stations on either work together.

## Direwolf

1. Run Direwolf with `MODEM 1200` (or `MODEM 300` on HF), your `PTT` setting
   and `KISSPORT 8001` (the default). Use a packet or digital channel from
   your band plan, not the APRS frequency.
2. On each station, create a key and exchange the line `hm whoami` prints
   ([setup](setup.md#trust)).
3. Run `hm node`, or for a quick test without the daemon:

   ```sh
   hm listen --ssid 1                                        # receiving station
   hm send --to SO5KM-1 --text "73 de SA0KAM"                # sending station
   hm send --to SO5KM-1 --subject "Sked" --text "40m 7.047 at 19Z?" --precedence priority
   ```

Every hm frame is an AX.25 UI frame from your callsign to `HMNET`, so each
transmission carries your station identification, and Direwolf's own CSMA
(`PERSIST`, `SLOTTIME`) handles channel access. Callsigns must fit AX.25: at
most 6 letters or digits plus SSID 0–15. If transfers time out on a slow or
busy channel, raise `guard_ms`; if your TNC's key-up delay differs from
300 ms, set `txdelay_ms` to match (both under `[radio]`, or `--guard` and
`--txdelay` for one run).

## A hardware TNC on a serial port

`--kiss` also takes a serial port, for TNCs such as a NinoTNC, Mobilinkd,
TNC-Pi or a TNC-2 in KISS mode:

```sh
hm node --kiss serial:/dev/ttyUSB0:57600
hm send --kiss /dev/ttyACM0 --to SO5KM-1 --text "via a hardware TNC"   # 9600 Bd
hm listen --kiss COM3                                                   # Windows
```

A hardware TNC keys the radio and waits for a clear channel itself, so on
opening the port `hm` sends it the key-up delay, persistence and slot time
(`txdelay_ms`, `persist`, `slottime_ms`) as KISS parameters. The TNC must
already be in KISS mode.

## The built-in modem

Without Direwolf, the node runs its own AFSK 1200 modem on a sound card:

```sh
hm audio-devices                                          # list sound cards
hm node --audio default --ptt vox
hm node --audio "USB Audio" --ptt cm108:/dev/hidraw0      # AIOC or Digirig
hm node --audio "USB Audio" --ptt rigctld                 # CAT through Hamlib's rigctld
hm node --audio "USB Audio" --ptt rts:/dev/ttyUSB0
```

or in `station.toml`: `audio = "USB Audio"` and `ptt = "cm108:/dev/hidraw0"`
under `[radio]`.

It waits for a clear channel (p-persistent CSMA on its carrier detect:
`persist`, `slottime_ms`), sends each burst in one key-up, and releases PTT on
every exit path.

With `framing = "il2p"` under `[radio]`, it sends the same frames in IL2P, the
framing of NinoTNC and Direwolf 1.7: Reed–Solomon parity repairs up to 8 bad
bytes in each block, so frames get through far more noise. On a simulated
channel, 40 frames of about 60 bytes at a full-band SNR of −4 dB: 20 arrived
as AX.25, all 40 as IL2P; at −6 dB, none against 30. `framing = "auto"` sends
IL2P to stations that said they decode it (every hm station on the built-in
modem does, and says so in its OPEN) and AX.25 to everyone else, beacons
included. The modem always decodes both. Against Direwolf both ways: Direwolf
decodes all of our IL2P frames, and we decode more of `gen_packets -I 1` than
Direwolf itself (95 against 94 of 100 in rising noise).

## Beacons

Every 10 minutes (`beacon_minutes`, 0 for none) the node sends a signed
beacon: its callsign and the id of its key, its grid locator, whether it has internet links
or relays, whether it holds mail for others, and the stations it has heard
lately. As more stations share the channel, beacons go further apart, so that
together they keep to a small share of it ([control plane](../control-plane.md)).
The Network view lists every station heard, with the distance and bearing to
those that give a locator, and for those that beacon whether their key
matches the one you trust.

## How long the transmitter is keyed

Data modes keep a transmitter at full output for as long as it is keyed, and
long key-ups heat the final amplifier. The node keys up for at most
`max_keyup_secs` at a time (20 by default; 0 for no limit): no burst of a
transfer is longer, the airtime a burst may use at once is no more (above it
the node keeps to `duty_cycle_percent` of the time on the air, 50 by default:
set what your transceiver's data-mode rating allows), and beacons and control
frames wait for its own frames to go out rather than run on after them. In a
simulated week of five HF stations at 300 bd, half the key-ups are under
5 s (mostly beacons) and each station's transmitter is on about 1 % of the
time ([results](../results.md)). Run data modes at reduced power as your
transceiver's manual advises.

On HF, 300 bd packet is slow and fragile against fading; an ARQ modem
(VARA HF, ARDOP) moves the same message in far less time on the air when
the path is decent.

See also [ARQ modems](modems.md) for VARA, Mercury and ARDOP.
