//! Writing changes back to `station.toml` with `toml_edit`, which keeps
//! the operator's comments and layout.

use std::fs;
use std::io;
use std::path::Path;

use hm_ident::PublicKey;
use hm_wire::Callsign;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use crate::hex;

use super::{invalid, Config, ModemSettings, RadioSettings, RelaySettings};

fn read_doc(path: &Path) -> io::Result<DocumentMut> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    // A file of only comments parses as trailing text, which would end up
    // after anything added; keep it at the top instead.
    if doc.as_table().is_empty() {
        let head = doc.trailing().as_str().unwrap_or_default().to_string();
        doc.set_trailing("");
        doc.as_table_mut().decor_mut().set_prefix(head);
    }
    Ok(doc)
}

/// Check the edited document still is a valid config, then replace the file atomically.
fn write_doc(path: &Path, doc: &DocumentMut) -> io::Result<()> {
    let text = doc.to_string();
    Config::parse(&text)
        .map_err(|e| invalid(format!("the change would make {} invalid: {e}", path.display())))?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

fn table<'a>(doc: &'a mut DocumentMut, name: &str) -> &'a mut Table {
    let item = doc.entry(name).or_insert(Item::Table(Table::new()));
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("a table")
}

/// Set `[section] key = v` in the file, keeping everything else as it is.
pub fn set_value(path: &Path, section: &str, key: &str, v: impl Into<toml_edit::Value>) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let t = table(&mut doc, section);
    let mut new = v.into();
    // Keep the comment written after the old value.
    if let Some(old) = t.get(key).and_then(|i| i.as_value()) {
        *new.decor_mut() = old.decor().clone();
    }
    t[key] = Item::Value(new);
    write_doc(path, &doc)
}

/// Remove `[section] key` from the file, so the default applies.
pub fn unset_value(path: &Path, section: &str, key: &str) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    table(&mut doc, section).remove(key);
    write_doc(path, &doc)
}

/// Trust `key` for exactly `call`: its entry is updated in place (keeping its
/// note unless a new one is given), or a new entry goes at the end.
pub fn set_trust(path: &Path, call: Callsign, key: &PublicKey, note: Option<&str>) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    // The comment at the end of the file introduces the entries: the first
    // one goes below it, not above.
    let mut head = None;
    if doc
        .get("trust")
        .and_then(|t| t.as_array_of_tables())
        .is_none_or(|t| t.is_empty())
    {
        head = Some(doc.trailing().as_str().unwrap_or_default().to_string());
        doc.set_trailing("");
    }
    let entries = trust_entries(&mut doc);
    let station = call.to_string();
    let found = entries
        .iter_mut()
        .find(|t| t.get("station").and_then(|s| s.as_str()) == Some(station.as_str()));
    match found {
        Some(t) => {
            t["key"] = value(hex::encode(&key.0));
            if let Some(n) = note {
                t["note"] = value(n);
            }
        }
        None => {
            let mut t = Table::new();
            t["station"] = value(station);
            t["key"] = value(hex::encode(&key.0));
            if let Some(n) = note {
                t["note"] = value(n);
            }
            if let Some(h) = head {
                t.decor_mut().set_prefix(h);
            }
            entries.push(t);
        }
    }
    write_doc(path, &doc)
}

/// Stop trusting exactly `call`; false if it had no entry.
pub fn remove_trust(path: &Path, call: Callsign) -> io::Result<bool> {
    let mut doc = read_doc(path)?;
    let station = call.to_string();
    let entries = trust_entries(&mut doc);
    let Some(i) = entries
        .iter()
        .position(|t| t.get("station").and_then(|s| s.as_str()) == Some(station.as_str()))
    else {
        return Ok(false);
    };
    // Comments above the entry stay: above the next entry, or at the end.
    let prefix = entries
        .get(i)
        .and_then(|t| t.decor().prefix())
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string();
    entries.remove(i);
    if !prefix.trim().is_empty() {
        let last = entries.is_empty();
        if let Some(next) = entries.get_mut(i) {
            let old = next
                .decor()
                .prefix()
                .and_then(|p| p.as_str())
                .unwrap_or_default()
                .to_string();
            next.decor_mut().set_prefix(format!("{prefix}{old}"));
        } else if last {
            doc.set_trailing(prefix);
        }
    }
    write_doc(path, &doc)?;
    Ok(true)
}

fn trust_entries(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    let item = doc
        .entry("trust")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    if !item.is_array_of_tables() {
        *item = Item::ArrayOfTables(ArrayOfTables::new());
    }
    item.as_array_of_tables_mut().expect("an array of tables")
}

/// Write the `[radio]` settings of `new` that differ from `old` (the
/// beacon interval aside), in one checked edit.
pub fn set_radio(path: &Path, old: &RadioSettings, new: &RadioSettings) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let t = table(&mut doc, "radio");
    let mut put = |key: &str, v: toml_edit::Value| {
        let mut v = v;
        if let Some(o) = t.get(key).and_then(|i| i.as_value()) {
            *v.decor_mut() = o.decor().clone();
        }
        t[key] = Item::Value(v);
    };
    if old.enabled != new.enabled {
        put("enabled", new.enabled.into());
    }
    if old.kiss != new.kiss {
        put("kiss", new.kiss.as_str().into());
    }
    if old.tnc_port != new.tnc_port {
        put("tnc_port", i64::from(new.tnc_port).into());
    }
    if old.ptt != new.ptt {
        put("ptt", new.ptt.as_str().into());
    }
    if old.framing != new.framing {
        put("framing", new.framing.as_str().into());
    }
    if old.max_rounds != new.max_rounds {
        put("max_rounds", i64::from(new.max_rounds).into());
    }
    if old.persist != new.persist {
        put("persist", i64::from(new.persist).into());
    }
    for (key, o, n) in [
        ("slottime_ms", old.slottime_ms, new.slottime_ms),
        ("bitrate", u64::from(old.bitrate), u64::from(new.bitrate)),
        ("txdelay_ms", old.txdelay_ms, new.txdelay_ms),
        ("guard_ms", old.guard_ms, new.guard_ms),
        ("max_keyup_secs", old.max_keyup_secs, new.max_keyup_secs),
    ] {
        if o != n {
            put(key, (n as i64).into());
        }
    }
    if old.audio != new.audio {
        match &new.audio {
            Some(a) => put("audio", a.as_str().into()),
            None => {
                t.remove("audio");
            }
        }
    }
    write_doc(path, &doc)
}

/// Replace changed `[relay]` keys, keeping comments on untouched ones.
pub fn set_relay(path: &Path, old: &RelaySettings, new: &RelaySettings) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let t = table(&mut doc, "relay");
    let mut put = |key: &str, v: toml_edit::Value| {
        let mut v = v;
        if let Some(o) = t.get(key).and_then(|i| i.as_value()) {
            *v.decor_mut() = o.decor().clone();
        }
        t[key] = Item::Value(v);
    };
    if old.enabled != new.enabled {
        put("enabled", new.enabled.into());
    }
    if old.mailbox != new.mailbox {
        put("mailbox", new.mailbox.into());
    }
    if old.max_holdings != new.max_holdings {
        put("max_holdings", (new.max_holdings as i64).into());
    }
    if old.max_bytes != new.max_bytes {
        put("max_bytes", (new.max_bytes as i64).into());
    }
    if old.max_hops != new.max_hops {
        put("max_hops", i64::from(new.max_hops).into());
    }
    if old.airtime_budget_secs != new.airtime_budget_secs {
        put("airtime_budget_secs", (new.airtime_budget_secs as i64).into());
    }
    if old.control_airtime_fraction != new.control_airtime_fraction {
        put("control_airtime_fraction", new.control_airtime_fraction.into());
    }
    write_doc(path, &doc)
}

/// Replace changed `[modem]` keys, keeping comments on untouched ones.
pub fn set_modem(path: &Path, old: &ModemSettings, new: &ModemSettings) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let t = table(&mut doc, "modem");
    let mut put = |key: &str, v: toml_edit::Value| {
        let mut v = v;
        if let Some(o) = t.get(key).and_then(|i| i.as_value()) {
            *v.decor_mut() = o.decor().clone();
        }
        t[key] = Item::Value(v);
    };
    if old.enabled != new.enabled {
        put("enabled", new.enabled.into());
    }
    if old.kind != new.kind {
        put("kind", new.kind.as_str().into());
    }
    if old.host != new.host {
        put("host", new.host.as_str().into());
    }
    if old.port != new.port {
        put("port", i64::from(new.port).into());
    }
    if old.bandwidth != new.bandwidth {
        put("bandwidth", i64::from(new.bandwidth).into());
    }
    if old.ptt != new.ptt {
        put("ptt", new.ptt.as_str().into());
    }
    write_doc(path, &doc)
}

/// Replace the internet peers.
pub fn set_peers(path: &Path, peers: &[(Callsign, String)]) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let mut list = ArrayOfTables::new();
    for (call, address) in peers {
        let mut t = Table::new();
        t["station"] = value(call.to_string());
        t["address"] = value(address.as_str());
        list.push(t);
    }
    table(&mut doc, "internet")["peers"] = Item::ArrayOfTables(list);
    write_doc(path, &doc)
}
