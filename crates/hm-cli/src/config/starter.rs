//! The commented starter file `hm keygen` and `hm setup` write.

use std::path::Path;

use hm_wire::Callsign;

use super::public_hub;

/// A commented starting file for a new station.
pub fn starter(call: Callsign, key: &Path) -> String {
    format!(
        r#"# hm-net station {call}. Every setting has a default; see README.md.
# The web page edits the delivery settings, the internet peers and the
# trusted stations, and keeps these comments.

# ssid = 1                  when the key file names a callsign without an SSID
# store = "station.db"
# http = "127.0.0.1:8080"   the web page; keep it on localhost unless behind HTTPS
# locator = "JO89xi"        your grid square, sent in beacons
[station]
key = {key:?}

# enabled = false           an internet-only node
# kiss = "127.0.0.1:8001"   Direwolf, or "serial:/dev/ttyUSB0:9600" for a hardware TNC
# audio = "default"         the built-in modem on a sound card instead
# ptt = "vox"               or "rts:/dev/ttyUSB0", "cm108:/dev/hidraw0"
# framing = "ax25"          built-in modem: or "il2p" (far more robust in noise), "auto"
# max_rounds = 12           radio handoff attempts before giving up
# max_keyup_secs = 20       longest the transmitter stays keyed at a time (0: no limit)
# duty_cycle_percent = 50   long-run share of the time on the air (100: no limit)
[radio]
beacon_minutes = 10         # 0 turns the beacon off

# listen = "0.0.0.0:4433"   accept links from other stations over the internet
# Stations this node dials, one entry each. The public core hub is included by default.
[internet]

[[internet.peers]]
station = "{hub_call}"
address = "{hub_addr}"

# An ARQ modem program as another way to reach stations (Mercury speaks
# VARA's interface; ARDOP uses port 8515):
#   [modem]
#   enabled = true
#   kind = "vara"
#   port = 8300
#   ptt = "none"            or how to key the radio when the modem asks

# Costs, in hundredths of a delivered message's value: a minute of radio
# airtime (dearer as the channel gets busier), an internet attempt, a minute
# of ARQ modem airtime. Each message goes the way, now or later, that is
# worth most for its chance and speed. A failed delivery is retried after
# first_secs, doubling up to max_secs, attempts times. After a hop handoff,
# shadow retain and suspect reclaim apply.
[delivery]
radio_cost = 5.0
internet_cost = 2.0
modem_cost = 5.0
retry_first_secs = 60
retry_max_secs = 3600
retry_attempts = 12
custody_grace_secs = 21600
custody_suspect_secs = 86400
receipt_retry_attempts = 24

# Relaying is opt-in. `mailbox` holds traffic for intermittently connected
# stations; `enabled` may forward it through another relay.
[relay]
enabled = false
mailbox = false
# max_holdings = 256
# max_bytes = 16777216
# max_hops = 8
# airtime_budget_secs = 300
# control_airtime_fraction = 0.02

# Stations whose messages you can verify. The public core hub is trusted by
# default. Add others with `hm trust add "CALL KEY"` or on the web page.
[[trust]]
station = "{hub_call}"
key = "{hub_key}"
note = "public core hub"
"#,
        key = key.display().to_string(),
        hub_call = public_hub::CALL,
        hub_addr = public_hub::ADDRESS,
        hub_key = public_hub::KEY_HEX,
    )
}
