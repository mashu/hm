//! Commands that ask a running node, through its HTTP API.

use std::path::Path;
use std::time::Duration;

use hm_cli::config::Config;
use hm_cli::station;

/// GET a JSON path on the running node's local HTTP API.
pub(crate) fn api_get(c: &Config, path: &str) -> Result<serde_json::Value, String> {
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;

    let addr = c
        .station
        .http
        .parse::<std::net::SocketAddr>()
        .map_err(|e| format!("station.http {}: {e}", c.station.http))?;
    let token = api_token(&c.station.store)?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| format!("node API at {addr}: {e} (is `hm node` running?)"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let body = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .map(|(_, b)| b.trim_start_matches('\u{feff}'))
        .ok_or_else(|| "node API: malformed HTTP response".to_string())?;
    // Drop a possible chunked framing first line length if present — prefer finding JSON.
    let json_start = body.find(['{', '[']).ok_or_else(|| {
        format!(
            "node API: not JSON ({})",
            body.chars().take(80).collect::<String>()
        )
    })?;
    serde_json::from_str(&body[json_start..]).map_err(|e| format!("node API JSON: {e}"))
}

/// Live status from the running node (needs local HTTP; SSH in if 8080 is not public).
pub(crate) fn status(c: &Config) -> Result<(), String> {
    let v = api_get(c, "/api/status")?;
    println!("call           {}", v["call"].as_str().unwrap_or("?"));
    println!("locator        {}", v["locator"].as_str().unwrap_or("-"));
    println!(
        "packet radio   {}",
        match v["radio"].as_bool() {
            Some(true) => format!(
                "up{}",
                v["radio_via"]
                    .as_str()
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default()
            ),
            Some(false) => "down".into(),
            None => "off".into(),
        }
    );
    let listen = v["internet_listen"].as_str().unwrap_or("-");
    let peers = v["internet_peers"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".into());
    println!("internet       listen {listen}; peers: {peers}");
    println!(
        "arq            {}",
        match v["modem"].as_bool() {
            Some(true) => {
                let peer = v["modem_peer"].as_str().unwrap_or("-");
                format!("ready (peer {peer})")
            }
            Some(false) => "down".into(),
            None => "off".into(),
        }
    );
    if let Some(rows) = v["estimates"].as_array() {
        if !rows.is_empty() {
            println!("reachability:");
            for row in rows {
                let station = row["station"].as_str().unwrap_or("?");
                let bearer = match row["bearer"].as_str() {
                    Some("radio") => "packet-radio",
                    Some("modem") => "arq",
                    Some(other) => other,
                    None => "?",
                };
                let success = row["success"].as_f64().unwrap_or(0.0);
                println!(
                    "  {station:9} {bearer:12} {:>3}%",
                    (success * 100.0).round() as i64
                );
            }
        }
    }
    if let Some(heard) = v["heard"].as_array() {
        if !heard.is_empty() {
            println!("heard on packet radio:");
            for h in heard.iter().take(20) {
                let call = h["call"].as_str().unwrap_or("?");
                let when = h["at"].as_u64().unwrap_or(0);
                println!("  {call}  (last at {when})");
            }
        }
    }
    Ok(())
}

/// List stored messages. Opens the store read-only so it works beside `hm node`.
pub(crate) fn messages(c: &Config, direction: &str, kind: &str, limit: usize) -> Result<(), String> {
    use hm_bundle::{Kind, Opened};
    use hm_store::{Direction, Store};

    let dirs: &[Direction] = match direction {
        "in" => &[Direction::In],
        "out" => &[Direction::Out],
        "relay" => &[Direction::Relay],
        "all" => &[Direction::In, Direction::Out, Direction::Relay],
        other => return Err(format!("direction must be in, out, relay or all, not {other:?}")),
    };
    let want_kind: Option<Vec<Kind>> = match kind {
        "human" => Some(vec![Kind::Chat, Kind::Mail, Kind::Bulletin]),
        "all" => None,
        "chat" => Some(vec![Kind::Chat]),
        "mail" => Some(vec![Kind::Mail]),
        "bulletin" => Some(vec![Kind::Bulletin]),
        "receipt" => Some(vec![Kind::Receipt]),
        other => {
            return Err(format!(
                "kind must be human, chat, mail, bulletin, receipt or all, not {other:?}"
            ))
        }
    };

    let store = Store::open_read_only(&c.station.store).or_else(|e| {
        Store::open(&c.station.store).map_err(|open_err| {
            format!("could not open store read-only ({e}); also failed write open: {open_err}")
        })
    })?;
    let mut rows = Vec::new();
    for d in dirs {
        for r in store.list(*d, 500).map_err(|e| e.to_string())? {
            rows.push(r);
        }
    }
    rows.sort_by(|a, b| b.at.cmp(&a.at).then(b.seq.cmp(&a.seq)));

    let mut shown = 0usize;
    for r in rows {
        let object = store.object(r.id).map_err(|e| e.to_string())?;
        let Some(bytes) = object else { continue };
        let opened = match Opened::decode(&bytes) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if want_kind
            .as_ref()
            .is_some_and(|kinds| !kinds.contains(&opened.bundle.kind))
        {
            continue;
        }
        let dir = match r.direction {
            Direction::In => "in",
            Direction::Out => "out",
            Direction::Relay => "relay",
        };
        let subject = opened
            .bundle
            .subject
            .as_deref()
            .map(|s| format!(" [{s}]"))
            .unwrap_or_default();
        let detail = if opened.bundle.kind == Kind::Receipt {
            match opened.bundle.reply_to {
                Some(id) => {
                    let s = id.to_string();
                    format!("ACK {}", &s[..s.len().min(12)])
                }
                None => "ACK (no id)".into(),
            }
        } else {
            let text = opened
                .bundle
                .body
                .as_ref()
                .and_then(|b| b.as_text().ok())
                .map(|t| t.into_owned())
                .unwrap_or_else(|| "<no text>".into());
            if text.len() > 120 {
                format!("{}…", &text[..117])
            } else {
                text
            }
        };
        let by = r.by.as_deref().map(|b| format!("  by={b}")).unwrap_or_default();
        println!(
            "{} {dir:5} {:?} {} ↔ {}  {:?}{subject}{by}  {}",
            station::utc_clock(r.at),
            opened.bundle.kind,
            opened.bundle.from,
            r.peer,
            r.state,
            detail
        );
        shown += 1;
        if shown >= limit {
            break;
        }
    }
    if shown == 0 {
        println!("(no messages)");
    }
    Ok(())
}

/// The API token beside the store, created on first start (owner-only on Unix).
pub(crate) fn api_token(store: &Path) -> Result<String, String> {
    let path = store.with_extension("token");
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let token = contents.trim();
            if token.len() != 48 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!(
                    "{}: invalid access token; remove the file to generate a new one",
                    path.display()
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(&path)
                    .map_err(|error| format!("{}: {error}", path.display()))?
                    .permissions()
                    .mode();
                if mode & 0o077 != 0 {
                    return Err(format!(
                        "{}: access token is readable by other users; run `chmod 600 {}`",
                        path.display(),
                        path.display()
                    ));
                }
            }
            return Ok(token.to_string());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", path.display())),
    }
    let mut raw = [0u8; 24];
    getrandom::fill(&mut raw).map_err(|e| format!("no system randomness: {e}"))?;
    let token = hm_cli::hex::encode(&raw);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write as _;
    opts.open(&path)
        .and_then(|mut f| f.write_all(format!("{token}\n").as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_token_is_strong_stable_and_private() {
        let store = std::env::temp_dir().join(format!(
            "hm-api-token-{}-{}.db",
            std::process::id(),
            getrandom::u64().unwrap()
        ));
        let path = store.with_extension("token");
        let token = api_token(&store).unwrap();
        assert_eq!(token.len(), 48);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(api_token(&store).unwrap(), token);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(api_token(&store).unwrap_err().contains("chmod 600"));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        std::fs::write(&path, "weak\n").unwrap();
        assert!(api_token(&store).unwrap_err().contains("invalid access token"));
        std::fs::remove_file(path).unwrap();
    }
}
