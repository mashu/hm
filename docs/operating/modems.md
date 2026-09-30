# HF and FM through VARA, Mercury or ARDOP

An ARQ modem program is a third way to reach stations, next to the radio and
the internet: VARA HF or FM, Mercury (which speaks VARA's host interface) or
ARDOP. The modem does its own error correction and retries, and hm uses the
connection it makes the way it uses an internet link: each bundle goes over it
whole, and the next hop answers with a signed custody receipt. The origin
marks the message **Delivered** only when the destination's separate signed
end-to-end receipt returns, as on the other bearers.

```toml
[modem]
enabled = true
kind = "vara"        # or "ardop" (port 8515); Mercury: "vara" with its ports
host = "127.0.0.1"
port = 8300          # command port; data is on the next one
bandwidth = 2300     # VARA HF 500, 2300 or 2750; ARDOP 200 to 2000; 0 leaves it
ptt = "none"         # the modem keys the radio; or "rts:/dev/ttyUSB0", "cm108:…", "rigctld"
```

The node registers its callsign with the modem and listens. To deliver, it
calls the station, sends every bundle waiting for it and hangs up; calls from
other stations are answered. The modem carries one connection at a time, so
deliveries to other stations wait their turn. Whether a message goes by the
modem, the radio or the internet, and when, is route choice: the modem's
airtime is priced by `modem_cost` per minute, and what the station has learned
of each link says how likely each way is ([routing](../routing.md)). The status
line shows whether the modem program is reachable and whom it is connected
to. Changing `[modem]` takes a restart.
