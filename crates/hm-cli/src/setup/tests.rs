use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hm_wire::Callsign;

use super::*;
use crate::files::KeyFile;

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
    assert_eq!(config.internet.peers, vec![config::public_hub::peer()]);
    assert_eq!(config.trust, vec![config::public_hub::trust()]);
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
    assert!(config.internet.open_hub);
    assert_eq!(config.internet.peers, vec![config::public_hub::peer()]);
    assert_eq!(config.trust, vec![config::public_hub::trust()]);
    assert!(
        output.contains("cloud server") || output.contains("core node"),
        "{output}"
    );
    assert!(output.contains("hm trust add"), "{output}");
    assert!(output.contains("[[internet.peers]]"), "{output}");
}

#[test]
fn setup_as_public_hub_does_not_dial_itself() {
    let dir = TestDir::new("self-hub");
    let script = format!("{}\n4\n\n\n\n", config::public_hub::CALL);
    let (outcome, output) = run_script(&dir, &script, AudioDevices::default());
    assert_eq!(outcome.unwrap(), SetupOutcome::Complete, "{output}");
    let config = Config::load(&dir.config()).unwrap();
    assert!(config.internet.peers.is_empty(), "{:?}", config.internet.peers);
    assert!(config.trust.is_empty(), "{:?}", config.trust);
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

    assert_eq!(
        fs::read_to_string(dir.0.join("station.key")).unwrap(),
        "home station key\n"
    );
    assert!(dir.0.join("core.key").exists(), "{text}");
    let written = Config::load(&config).unwrap();
    assert_eq!(written.station.key, PathBuf::from("core.key"));
    assert_eq!(written.station.store, PathBuf::from("core.db"));
    assert!(text.contains("hm --config"), "{text}");
    assert!(text.contains("core.toml"), "{text}");
}
