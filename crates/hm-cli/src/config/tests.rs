use super::*;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hm-config-{name}-{}.toml", std::process::id()))
}

#[test]
fn defaults_and_partial_files() {
    assert_eq!(Config::parse("").unwrap(), Config::default());
    let c = Config::parse("[radio]\nenabled = false\n[delivery]\ninternet_cost = 0.5\n").unwrap();
    assert!(!c.radio.enabled);
    assert_eq!(c.delivery.internet_cost, 0.5);
    assert_eq!(c.delivery.radio_cost, hm_node::Costs::default().radio);
    // A typo is an error, not a silently ignored setting.
    let e = Config::parse("[radio]\nbecaon_minutes = 5\n").unwrap_err();
    assert!(e.contains("becaon_minutes"), "{e}");
    assert!(Config::parse("[[trust]]\nstation = \"SO5KM\"\nkey = \"xyz\"\n").is_err());
    assert!(Config::parse("[[internet.peers]]\nstation = \"SO5KM\"\naddress = \"nohost\"\n").is_err());
}

#[test]
fn planned_contacts_are_checked_and_converted() {
    let config = Config::parse(
        "[[contact]]\nfrom = \"M0AAA\"\nto = \"M0BBB\"\nbearer = \"radio\"\nstart = 100\nend = 200\nrate_bps = 1200\ncapacity_bytes = 4096\nsuccess = 0.8\n",
    )
    .unwrap();
    let contacts = config.contacts().unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0].success_permyriad, Some(8_000));
    assert!(Config::parse(
        "[[contact]]\nfrom = \"M0AAA\"\nto = \"M0BBB\"\nbearer = \"radio\"\nstart = 200\nend = 100\nrate_bps = 1200\ncapacity_bytes = 4096\n",
    )
    .is_err());
}

#[test]
fn starter_file_is_valid_and_mostly_comments() {
    let text = starter(call("SA0KAM-1"), Path::new("station.key"));
    let c = Config::parse(&text).unwrap();
    assert_eq!(c.station.key, PathBuf::from("station.key"));
    assert_eq!(c.trust, vec![public_hub::trust()]);
    assert_eq!(c.internet.peers, vec![public_hub::peer()]);
}

#[test]
fn edits_keep_comments_and_check_the_result() {
    let path = tmp("edit");
    fs::write(
        &path,
        "# my station\n[radio]\nkiss = \"127.0.0.1:8001\" # direwolf\n\n# friends\n[[trust]]\nstation = \"SO5KM\"\nkey = \"0101010101010101010101010101010101010101010101010101010101010101\"\nnote = \"Jan\"\n",
    )
    .unwrap();
    set_trust(&path, call("SO5KM"), &PublicKey([2; 32]), None).unwrap();
    set_trust(&path, call("SA0KAM-2"), &PublicKey([3; 32]), Some("my server")).unwrap();
    set_value(&path, "radio", "beacon_minutes", 5i64).unwrap();
    set_peers(&path, &[(call("SA0KAM-2"), "server.example.org:4433".into())]).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("# my station\n[radio]\nkiss = \"127.0.0.1:8001\" # direwolf\n"),
        "{text}"
    );
    assert!(text.contains("# friends"), "{text}");
    let c = Config::load(&path).unwrap();
    assert_eq!(c.radio.beacon_minutes, 5);
    assert_eq!(c.trust[0].note.as_deref(), Some("Jan"), "note kept on update");
    let t = c.trust().unwrap();
    assert_eq!(t.key_for(call("SO5KM-4")), Some(PublicKey([2; 32])));
    assert_eq!(t.key_for(call("SA0KAM-2")), Some(PublicKey([3; 32])));
    assert_eq!(
        c.peers().unwrap(),
        vec![(call("SA0KAM-2"), "server.example.org:4433".to_string())]
    );

    assert!(remove_trust(&path, call("SO5KM")).unwrap());
    assert!(!remove_trust(&path, call("SO5KM")).unwrap());
    unset_value(&path, "radio", "beacon_minutes").unwrap();
    let c = Config::load(&path).unwrap();
    assert_eq!((c.trust.len(), c.radio.beacon_minutes), (1, 10));
    // An edit that would leave the file invalid is refused and changes nothing.
    let before = fs::read_to_string(&path).unwrap();
    assert!(set_value(&path, "radio", "persist", "lots").is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    fs::remove_file(&path).unwrap();
    // A file that does not exist yet is created by the first edit.
    set_trust(&path, call("SO5KM"), &PublicKey([1; 32]), None).unwrap();
    assert_eq!(Config::load(&path).unwrap().trust.len(), 1);
    fs::remove_file(&path).unwrap();
}

#[test]
fn edits_to_the_starter_file_land_under_their_comments() {
    let path = tmp("starter");
    fs::write(&path, starter(call("SA0KAM-1"), Path::new("station.key"))).unwrap();
    set_value(&path, "delivery", "internet_cost", 0.5).unwrap();
    set_value(&path, "radio", "beacon_minutes", 5i64).unwrap();
    set_peers(&path, &[(call("SO5KM"), "hub.example.org:4433".into())]).unwrap();
    set_trust(&path, call("SO5KM-1"), &PublicKey([1; 32]), Some("Jan")).unwrap();
    set_trust(&path, call("SP5AAA"), &PublicKey([2; 32]), None).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("beacon_minutes = 5         # 0 turns the beacon off"),
        "{text}"
    );
    let (intro, peer, delivery, hub) = (
        text.find("[internet]").unwrap(),
        text.find("[[internet.peers]]\nstation = \"SO5KM\"").unwrap(),
        text.find("[delivery]").unwrap(),
        text.find(&format!("station = \"{}\"", public_hub::CALL)).unwrap(),
    );
    assert!(intro < peer && peer < delivery && delivery < hub, "{text}");
    assert!(text.contains("station = \"SO5KM-1\""), "{text}");
    assert!(text.contains("station = \"SP5AAA\""), "{text}");

    assert!(remove_trust(&path, call("SO5KM-1")).unwrap());
    assert!(remove_trust(&path, call("SP5AAA")).unwrap());
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(public_hub::CALL), "{text}");
    set_trust(&path, call("SP5AAA"), &PublicKey([2; 32]), None).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("station = \"SP5AAA\""), "{text}");
    fs::remove_file(&path).unwrap();
}

#[test]
fn public_hub_is_in_the_defaults() {
    let c = Config::default();
    assert_eq!(c.internet.peers, vec![public_hub::peer()]);
    assert_eq!(c.trust, vec![public_hub::trust()]);
    assert_eq!(Config::parse("").unwrap(), c);
    let mut self_hub = c;
    public_hub::omit_self(&mut self_hub, call(public_hub::CALL));
    assert!(self_hub.internet.peers.is_empty());
    assert!(self_hub.trust.is_empty());
}
