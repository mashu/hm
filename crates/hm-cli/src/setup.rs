//! Interactive first-run setup for a station.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use hm_wire::{Callsign, Locator};

use crate::config::{self, Config};
use crate::files::KeyFile;
use crate::kiss_link::KissTarget;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Connection {
    Kiss,
    BuiltIn,
    InternetOnly,
    /// Public hub: listen on the internet, relay and mailbox on, no radio.
    CoreNode,
}

impl Connection {
    fn description(self) -> &'static str {
        match self {
            Connection::Kiss => "KISS TNC / Direwolf",
            Connection::BuiltIn => "built-in sound-card modem",
            Connection::InternetOnly => "internet only",
            Connection::CoreNode => "core node (cloud hub)",
        }
    }
}

struct Wizard<'a, R, W> {
    input: &'a mut R,
    output: &'a mut W,
}

impl<R: BufRead, W: Write> Wizard<'_, R, W> {
    fn say(&mut self, message: impl std::fmt::Display) -> io::Result<()> {
        writeln!(self.output, "{message}")
    }

    fn answer(&mut self, prompt: &str) -> io::Result<Option<String>> {
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

    fn callsign(&mut self) -> io::Result<Option<Callsign>> {
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

    fn connection(&mut self) -> io::Result<Option<Connection>> {
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

    fn kiss_endpoint(&mut self) -> io::Result<Option<String>> {
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

    fn audio_device(&mut self, devices: &AudioDevices) -> io::Result<Option<String>> {
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

    fn ptt(&mut self) -> io::Result<Option<String>> {
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

    fn framing(&mut self) -> io::Result<Option<String>> {
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

    fn locator(&mut self) -> io::Result<Option<Option<String>>> {
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

    fn yes_no(&mut self, prompt: &str, default: bool) -> io::Result<Option<bool>> {
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

    fn listen_address(&mut self) -> io::Result<Option<String>> {
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

fn audio_choices(devices: &AudioDevices) -> Vec<(String, &'static str)> {
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

fn path_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// Key/store basename from the config path: `core.toml` → `core.key` / `core.db`.
fn companion_name(config_path: &Path, extension: &str) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("station");
    PathBuf::from(format!("{stem}.{extension}"))
}

fn ensure_paths_available(config_path: &Path, key_path: &Path) -> io::Result<()> {
    if config_path == key_path {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the configuration and key paths must be different",
        ));
    }
    for path in [config_path, key_path] {
        if path.try_exists().map_err(|error| path_error(path, error))? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists; setup will not overwrite it", path.display()),
            ));
        }
    }
    Ok(())
}

fn save_setup(config_path: &Path, key_path: &Path, config: &Config, call: Callsign) -> io::Result<KeyFile> {
    ensure_paths_available(config_path, key_path)?;
    let key = KeyFile::generate(call)?;
    key.save(key_path).map_err(|error| path_error(key_path, error))?;
    if let Err(error) = config.save_new(config_path) {
        let _ = fs::remove_file(key_path);
        return Err(path_error(config_path, error));
    }
    Ok(key)
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
            "hub: radio off; relay and mailbox on".to_string()
        }
    };

    let Some(locator) = wizard.locator()? else {
        return Ok(SetupOutcome::Cancelled);
    };
    config.station.locator = locator;

    if connection == Connection::CoreNode {
        wizard.say("\nA core node is a meeting point on the internet for stations you trust.")?;
        wizard.say(
            "Run it on a computer with a public address (a small rented cloud server works well).",
        )?;
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

        let Some(mailbox) =
            wizard.yes_no("Hold mailbox traffic for offline trusted stations?", false)?
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
        wizard.say(format!(
            "On this core node: trust each home station (`hm{config_flag} trust add` their whoami line)."
        ))?;
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let unique = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("hm-setup-{name}-{}-{unique}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn config(&self) -> PathBuf {
            self.0.join("station.toml")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_script(dir: &TestDir, script: &str, devices: AudioDevices) -> (io::Result<SetupOutcome>, String) {
        let mut input = io::Cursor::new(script.as_bytes());
        let mut output = Vec::new();
        let result = run_with_io(&dir.config(), &devices, &mut input, &mut output);
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn built_in_modem_setup_writes_complete_checked_files() {
        let dir = TestDir::new("builtin");
        let devices = AudioDevices {
            inputs: vec!["USB Radio".into()],
            outputs: vec!["USB Radio".into()],
            error: None,
        };
        let script = "M0ABC-1\n2\n2\n\n\nIO91wm\ny\n\ny\ny\n\n";
        let (outcome, output) = run_script(&dir, script, devices);
        assert_eq!(outcome.unwrap(), SetupOutcome::Complete, "{output}");

        let config = Config::load(&dir.config()).unwrap();
        assert_eq!(config.radio.audio.as_deref(), Some("USB Radio"));
        assert_eq!(config.radio.ptt, "vox");
        assert_eq!(config.radio.framing, "il2p");
        assert_eq!(config.station.locator.as_deref(), Some("IO91wm"));
        assert_eq!(config.internet.listen.as_deref(), Some("0.0.0.0:4433"));
        assert!(config.relay.enabled);
        assert!(config.relay.mailbox);
        let key = KeyFile::load(&dir.0.join("station.key")).unwrap();
        assert_eq!(key.call, Callsign::parse("M0ABC-1").unwrap());
        assert!(output.contains("Other stations can trust this station"));
    }

    #[test]
    fn internet_only_defaults_to_a_listener_and_disables_radio() {
        let dir = TestDir::new("internet");
        let script = "SA0NET-2\n3\n\n\n\nn\nn\n\n";
        let (outcome, output) = run_script(&dir, script, AudioDevices::default());
        assert_eq!(outcome.unwrap(), SetupOutcome::Complete, "{output}");

        let config = Config::load(&dir.config()).unwrap();
        assert!(!config.radio.enabled);
        assert_eq!(config.radio.beacon_minutes, 0);
        assert_eq!(config.internet.listen.as_deref(), Some("0.0.0.0:4433"));
        assert!(!config.relay.enabled);
        assert!(!config.relay.mailbox);
    }

    #[test]
    fn core_node_enables_listener_relay_and_mailbox() {
        let dir = TestDir::new("core");
        // callsign, connection 4, locator empty, listen default, write yes
        let script = "SM0HUB-1\n4\n\n\n\n";
        let (outcome, output) = run_script(&dir, script, AudioDevices::default());
        assert_eq!(outcome.unwrap(), SetupOutcome::Complete, "{output}");

        let config = Config::load(&dir.config()).unwrap();
        assert!(!config.radio.enabled);
        assert_eq!(config.radio.beacon_minutes, 0);
        assert_eq!(config.internet.listen.as_deref(), Some("0.0.0.0:4433"));
        assert!(config.relay.enabled);
        assert!(config.relay.mailbox);
        assert!(output.contains("cloud server") || output.contains("core node"), "{output}");
        assert!(output.contains("hm trust add"), "{output}");
        assert!(output.contains("[[internet.peers]]"), "{output}");
    }

    #[test]
    fn invalid_callsign_retries_and_cancellation_writes_nothing() {
        let dir = TestDir::new("cancel");
        let script = "not a call\nM0ABC\n1\n\n\nn\nn\nn\nn\n";
        let (outcome, output) = run_script(&dir, script, AudioDevices::default());
        assert_eq!(outcome.unwrap(), SetupOutcome::Cancelled);
        assert!(output.contains("Invalid callsign"), "{output}");
        assert!(!dir.config().exists());
        assert!(!dir.0.join("station.key").exists());
    }

    #[test]
    fn existing_files_are_preserved() {
        let dir = TestDir::new("existing");
        fs::write(dir.config(), "do not replace\n").unwrap();
        let (outcome, _) = run_script(&dir, "", AudioDevices::default());
        let error = outcome.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(dir.config()).unwrap(), "do not replace\n");

        fs::remove_file(dir.config()).unwrap();
        fs::write(dir.0.join("station.key"), "existing private key\n").unwrap();
        let (outcome, _) = run_script(&dir, "", AudioDevices::default());
        let error = outcome.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(dir.0.join("station.key")).unwrap(),
            "existing private key\n"
        );
    }

    #[test]
    fn named_config_uses_matching_key_beside_existing_station() {
        let dir = TestDir::new("named");
        fs::write(dir.0.join("station.key"), "home station key\n").unwrap();
        fs::write(dir.0.join("station.toml"), "home station config\n").unwrap();

        let config = dir.0.join("core.toml");
        let script = "SM0HUB-1\n4\n\n\n\n";
        let mut input = io::Cursor::new(script.as_bytes());
        let mut output = Vec::new();
        let outcome = run_with_io(&config, &AudioDevices::default(), &mut input, &mut output);
        let text = String::from_utf8(output).unwrap();
        assert_eq!(outcome.unwrap(), SetupOutcome::Complete, "{text}");

        assert_eq!(fs::read_to_string(dir.0.join("station.key")).unwrap(), "home station key\n");
        assert!(dir.0.join("core.key").exists(), "{text}");
        let written = Config::load(&config).unwrap();
        assert_eq!(written.station.key, PathBuf::from("core.key"));
        assert_eq!(written.station.store, PathBuf::from("core.db"));
        assert!(text.contains("hm --config"), "{text}");
        assert!(text.contains("core.toml"), "{text}");
    }
}
