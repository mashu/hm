//! Interactive first-run setup for a station.

mod files;
#[cfg(test)]
mod tests;
mod wizard;

use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::config::{self, Config};
use files::{companion_name, ensure_paths_available, save_setup};
use wizard::{Connection, Wizard};

#[derive(Clone, Debug, Default)]
struct AudioDevices {
    inputs: Vec<String>,
    outputs: Vec<String>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupOutcome {
    Complete,
    Cancelled,
}

fn run_with_io<R: BufRead, W: Write>(
    config_path: &Path,
    devices: &AudioDevices,
    input: &mut R,
    output: &mut W,
) -> io::Result<SetupOutcome> {
    let key_setting = companion_name(config_path, "key");
    let store_setting = companion_name(config_path, "db");
    let key_path = config::dir_of(config_path).join(&key_setting);
    ensure_paths_available(config_path, &key_path)?;

    let mut wizard = Wizard { input, output };
    wizard.say("hm first-run setup")?;
    wizard.say("Type q at any prompt to cancel. Existing files are never overwritten.\n")?;

    let Some(call) = wizard.callsign()? else {
        return Ok(SetupOutcome::Cancelled);
    };
    let Some(connection) = wizard.connection()? else {
        return Ok(SetupOutcome::Cancelled);
    };

    let mut config = Config::default();
    config.station.key = key_setting;
    config.station.store = store_setting;
    // Defaults include the public core hub; do not dial/trust ourselves.
    config::public_hub::omit_self(&mut config, call);
    let connection_detail = match connection {
        Connection::Kiss => {
            let Some(endpoint) = wizard.kiss_endpoint()? else {
                return Ok(SetupOutcome::Cancelled);
            };
            config.radio.kiss = endpoint.clone();
            format!("KISS endpoint {endpoint}")
        }
        Connection::BuiltIn => {
            let Some(device) = wizard.audio_device(devices)? else {
                return Ok(SetupOutcome::Cancelled);
            };
            let Some(ptt) = wizard.ptt()? else {
                return Ok(SetupOutcome::Cancelled);
            };
            let Some(framing) = wizard.framing()? else {
                return Ok(SetupOutcome::Cancelled);
            };
            config.radio.audio = Some(device.clone());
            config.radio.ptt = ptt;
            config.radio.framing = framing.clone();
            format!("audio device {device}, {framing} framing")
        }
        Connection::InternetOnly => {
            config.radio.enabled = false;
            config.radio.beacon_minutes = 0;
            "radio disabled".to_string()
        }
        Connection::CoreNode => {
            config.radio.enabled = false;
            config.radio.beacon_minutes = 0;
            config.relay.enabled = true;
            config.relay.mailbox = true;
            config.internet.open_hub = true;
            "hub: radio off; relay and mailbox on; accept any dialer".to_string()
        }
    };

    let Some(locator) = wizard.locator()? else {
        return Ok(SetupOutcome::Cancelled);
    };
    config.station.locator = locator;

    if connection == Connection::CoreNode {
        wizard.say("\nA core node is a meeting point on the internet for stations you trust.")?;
        wizard.say("Run it on a computer with a public address (a small rented cloud server works well).")?;
        wizard.say("Allow UDP port 4433 through the firewall so home stations can dial in.")?;
        let Some(address) = wizard.listen_address()? else {
            return Ok(SetupOutcome::Cancelled);
        };
        config.internet.listen = Some(address);
    } else {
        let listen_default = connection == Connection::InternetOnly;
        let Some(enable_listener) = wizard.yes_no(
            "\nAccept authenticated connections from trusted internet stations?",
            listen_default,
        )?
        else {
            return Ok(SetupOutcome::Cancelled);
        };
        if enable_listener {
            let Some(address) = wizard.listen_address()? else {
                return Ok(SetupOutcome::Cancelled);
            };
            config.internet.listen = Some(address);
        } else if connection == Connection::InternetOnly {
            wizard.say("Note: add an internet peer before starting this internet-only node.")?;
        }

        let Some(relay) = wizard.yes_no(
            "\nRelay messages when both the sender and final receiver are trusted?",
            false,
        )?
        else {
            return Ok(SetupOutcome::Cancelled);
        };
        config.relay.enabled = relay;

        let Some(mailbox) = wizard.yes_no("Hold mailbox traffic for offline trusted stations?", false)?
        else {
            return Ok(SetupOutcome::Cancelled);
        };
        config.relay.mailbox = mailbox;
    }

    wizard.say("\nConfiguration summary:")?;
    wizard.say(format!("  Callsign: {call}"))?;
    wizard.say(format!(
        "  Connection: {} ({connection_detail})",
        connection.description()
    ))?;
    wizard.say(format!(
        "  Locator: {}",
        config.station.locator.as_deref().unwrap_or("not broadcast")
    ))?;
    wizard.say(format!(
        "  Internet listener: {}",
        config.internet.listen.as_deref().unwrap_or("disabled")
    ))?;
    wizard.say(format!(
        "  Relay: {}; mailbox: {}",
        if config.relay.enabled {
            "enabled"
        } else {
            "disabled"
        },
        if config.relay.mailbox {
            "enabled"
        } else {
            "disabled"
        }
    ))?;
    wizard.say(format!("  Private key: {}", key_path.display()))?;
    wizard.say(format!("  Configuration: {}", config_path.display()))?;
    if connection != Connection::CoreNode
        && config
            .internet
            .peers
            .iter()
            .any(|p| p.station == config::public_hub::CALL)
    {
        wizard.say(format!(
            "  Default hub: {} at {}",
            config::public_hub::CALL,
            config::public_hub::ADDRESS
        ))?;
    }

    let Some(confirm) = wizard.yes_no("\nWrite these files?", true)? else {
        return Ok(SetupOutcome::Cancelled);
    };
    if !confirm {
        wizard.say("Setup cancelled; no files were changed.")?;
        return Ok(SetupOutcome::Cancelled);
    }

    let key = save_setup(config_path, &key_path, &config, call)?;
    wizard.say(format!("\nWrote {}.", key_path.display()))?;
    wizard.say(format!("Wrote {}.", config_path.display()))?;
    let config_flag = config_path
        .to_str()
        .filter(|p| *p != config::DEFAULT_PATH)
        .map(|p| format!(" --config {p}"))
        .unwrap_or_default();
    wizard.say("Other stations can trust this station with:")?;
    wizard.say(format!("  hm trust add {:?}", key.trust_line()))?;
    if connection == Connection::CoreNode {
        wizard.say("On each home station: trust this core node, then add under [internet]:")?;
        wizard.say(format!(
            "  [[internet.peers]]\n  station = {:?}\n  address = \"YOUR.SERVER.HOST:4433\"",
            call.to_string()
        ))?;
        wizard.say(
            "This hub accepts any dialer (`open_hub`); home stations only need to trust and peer this node.",
        )?;
        wizard.say(format!("Then run `hm{config_flag} node` here."))?;
    } else {
        wizard.say(format!(
            "Next: review {}, add trusted peers, then run `hm{config_flag} node`.",
            config_path.display()
        ))?;
    }
    Ok(SetupOutcome::Complete)
}

/// Run the interactive setup wizard against the terminal.
pub fn run(config_path: &Path) -> Result<(), String> {
    let devices = match hm_rig::soundcard::devices() {
        Ok((inputs, outputs)) => AudioDevices {
            inputs,
            outputs,
            error: None,
        },
        Err(error) => AudioDevices {
            error: Some(error.to_string()),
            ..AudioDevices::default()
        },
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_with_io(config_path, &devices, &mut stdin.lock(), &mut stdout.lock())
        .map(|_| ())
        .map_err(|error| error.to_string())
}
