//! The questions setup asks, one at a time, each asked again until the
//! answer is usable.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;

use hm_wire::{Callsign, Locator};

use super::AudioDevices;
use crate::kiss_link::KissTarget;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Connection {
    Kiss,
    BuiltIn,
    InternetOnly,
    /// Public hub: listen on the internet, relay and mailbox on, no radio.
    CoreNode,
}

impl Connection {
    pub(super) fn description(self) -> &'static str {
        match self {
            Connection::Kiss => "KISS TNC / Direwolf",
            Connection::BuiltIn => "built-in sound-card modem",
            Connection::InternetOnly => "internet only",
            Connection::CoreNode => "core node (cloud hub)",
        }
    }
}

pub(super) struct Wizard<'a, R, W> {
    pub(super) input: &'a mut R,
    pub(super) output: &'a mut W,
}

impl<R: BufRead, W: Write> Wizard<'_, R, W> {
    pub(super) fn say(&mut self, message: impl std::fmt::Display) -> io::Result<()> {
        writeln!(self.output, "{message}")
    }

    pub(super) fn answer(&mut self, prompt: &str) -> io::Result<Option<String>> {
        write!(self.output, "{prompt}")?;
        self.output.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            self.say("\nSetup cancelled; no files were changed.")?;
            return Ok(None);
        }
        let answer = line.trim().to_string();
        if answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit") {
            self.say("Setup cancelled; no files were changed.")?;
            return Ok(None);
        }
        Ok(Some(answer))
    }

    pub(super) fn callsign(&mut self) -> io::Result<Option<Callsign>> {
        loop {
            let Some(answer) = self.answer("Station callsign (for example, M0ABC-1): ")? else {
                return Ok(None);
            };
            match Callsign::parse(&answer) {
                Ok(call) => return Ok(Some(call)),
                Err(error) => self.say(format!("Invalid callsign: {error}. Please try again."))?,
            }
        }
    }

    pub(super) fn connection(&mut self) -> io::Result<Option<Connection>> {
        self.say("\nHow will this station connect?")?;
        self.say("  1. KISS TNC or Direwolf (default)")?;
        self.say("  2. Built-in sound-card modem")?;
        self.say("  3. Internet only (no radio)")?;
        self.say("  4. Core node — hub for other stations (no radio; for a cloud server)")?;
        loop {
            let Some(answer) = self.answer("Connection [1]: ")? else {
                return Ok(None);
            };
            match answer.as_str() {
                "" | "1" => return Ok(Some(Connection::Kiss)),
                "2" => return Ok(Some(Connection::BuiltIn)),
                "3" => return Ok(Some(Connection::InternetOnly)),
                "4" => return Ok(Some(Connection::CoreNode)),
                _ => self.say("Enter 1, 2, 3, or 4.")?,
            }
        }
    }

    pub(super) fn kiss_endpoint(&mut self) -> io::Result<Option<String>> {
        loop {
            let Some(answer) = self.answer("KISS endpoint [127.0.0.1:8001]: ")? else {
                return Ok(None);
            };
            let endpoint = if answer.is_empty() {
                "127.0.0.1:8001".to_string()
            } else {
                answer
            };
            match KissTarget::parse(&endpoint) {
                Ok(_) => return Ok(Some(endpoint)),
                Err(error) => self.say(format!("Invalid KISS endpoint: {error}. Please try again."))?,
            }
        }
    }

    pub(super) fn audio_device(&mut self, devices: &AudioDevices) -> io::Result<Option<String>> {
        if let Some(error) = &devices.error {
            self.say(format!("\nSound-card discovery was unavailable: {error}"))?;
        }

        let choices = audio_choices(devices);
        self.say("\nAudio device:")?;
        self.say("  1. System default (recommended)")?;
        for (index, (name, capabilities)) in choices.iter().enumerate() {
            self.say(format!("  {}. {name} ({capabilities})", index + 2))?;
        }
        self.say("You may also enter part of a device name.")?;

        loop {
            let Some(answer) = self.answer("Audio device [1]: ")? else {
                return Ok(None);
            };
            if answer.is_empty() || answer == "1" {
                return Ok(Some("default".into()));
            }
            if let Ok(index) = answer.parse::<usize>() {
                if let Some((name, _)) = index.checked_sub(2).and_then(|i| choices.get(i)) {
                    return Ok(Some(name.clone()));
                }
                self.say(format!(
                    "Enter 1 through {}, or a device name.",
                    choices.len() + 1
                ))?;
                continue;
            }
            return Ok(Some(answer));
        }
    }

    pub(super) fn ptt(&mut self) -> io::Result<Option<String>> {
        self.say("\nPTT examples: vox, rigctld, rts:/dev/ttyUSB0, dtr:DEVICE, cm108:HIDRAW.")?;
        loop {
            let Some(answer) = self.answer("PTT method [vox]: ")? else {
                return Ok(None);
            };
            let ptt = if answer.is_empty() {
                "vox".to_string()
            } else {
                answer
            };
            if ptt.trim().is_empty() {
                self.say("PTT method cannot be empty.")?;
            } else {
                return Ok(Some(ptt));
            }
        }
    }

    pub(super) fn framing(&mut self) -> io::Result<Option<String>> {
        self.say("\nBuilt-in modem framing:")?;
        self.say("  1. IL2P with error correction (recommended)")?;
        self.say("  2. AX.25 HDLC")?;
        self.say("  3. Auto-select per destination")?;
        loop {
            let Some(answer) = self.answer("Framing [1]: ")? else {
                return Ok(None);
            };
            match answer.as_str() {
                "" | "1" => return Ok(Some("il2p".into())),
                "2" => return Ok(Some("ax25".into())),
                "3" => return Ok(Some("auto".into())),
                _ => self.say("Enter 1, 2, or 3.")?,
            }
        }
    }

    pub(super) fn locator(&mut self) -> io::Result<Option<Option<String>>> {
        loop {
            let Some(answer) = self.answer("\nMaidenhead locator (4 or 6 characters; optional): ")? else {
                return Ok(None);
            };
            if answer.is_empty() {
                return Ok(Some(None));
            }
            match Locator::parse(&answer) {
                Ok(locator) => return Ok(Some(Some(locator.to_string()))),
                Err(error) => self.say(format!("Invalid locator: {error}. Please try again."))?,
            }
        }
    }

    pub(super) fn yes_no(&mut self, prompt: &str, default: bool) -> io::Result<Option<bool>> {
        let suffix = if default { " [Y/n]: " } else { " [y/N]: " };
        loop {
            let Some(answer) = self.answer(&format!("{prompt}{suffix}"))? else {
                return Ok(None);
            };
            match answer.to_ascii_lowercase().as_str() {
                "" => return Ok(Some(default)),
                "y" | "yes" => return Ok(Some(true)),
                "n" | "no" => return Ok(Some(false)),
                _ => self.say("Enter y or n.")?,
            }
        }
    }

    pub(super) fn listen_address(&mut self) -> io::Result<Option<String>> {
        loop {
            let Some(answer) = self.answer("Internet listen address [0.0.0.0:4433]: ")? else {
                return Ok(None);
            };
            let address = if answer.is_empty() {
                "0.0.0.0:4433".to_string()
            } else {
                answer
            };
            match address.parse::<SocketAddr>() {
                Ok(_) => return Ok(Some(address)),
                Err(error) => self.say(format!("Invalid listen address: {error}. Please try again."))?,
            }
        }
    }
}

pub(super) fn audio_choices(devices: &AudioDevices) -> Vec<(String, &'static str)> {
    let mut choices: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for input in &devices.inputs {
        choices.entry(input.clone()).or_default().0 = true;
    }
    for output in &devices.outputs {
        choices.entry(output.clone()).or_default().1 = true;
    }
    choices
        .into_iter()
        .map(|(name, (input, output))| {
            let capabilities = match (input, output) {
                (true, true) => "capture and playback",
                (true, false) => "capture only",
                (false, true) => "playback only",
                (false, false) => unreachable!("a listed device has a capability"),
            };
            (name, capabilities)
        })
        .collect()
}
