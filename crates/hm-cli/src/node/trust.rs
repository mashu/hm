//! The trust list while the node runs: read by the radio, the internet link
//! and the API, changed through the API or by editing the trust file.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::SystemTime;

use hm_ident::PublicKey;
use hm_wire::Callsign;

use crate::files::{edit_trust_file, Trust};

pub struct SharedTrust {
    /// The list and a version bumped on every change.
    current: RwLock<(Trust, u64)>,
    file: Option<PathBuf>,
    /// The file's modification time and size when last read. The size too,
    /// because some file systems keep times to the second only.
    seen: Mutex<Option<(SystemTime, u64)>>,
}

fn modified(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

impl SharedTrust {
    /// Start from `trust`, as read from `file` (if the node has one).
    pub fn new(trust: Trust, file: Option<PathBuf>) -> SharedTrust {
        let seen = file.as_deref().and_then(modified);
        SharedTrust {
            current: RwLock::new((trust, 0)),
            file,
            seen: Mutex::new(seen),
        }
    }

    pub fn get(&self) -> Trust {
        self.current.read().expect("lock").0.clone()
    }

    pub fn version(&self) -> u64 {
        self.current.read().expect("lock").1
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Use `trust` from now on; true if it differs from what was in use.
    fn replace(&self, trust: Trust) -> bool {
        let mut cur = self.current.write().expect("lock");
        if cur.0 == trust {
            return false;
        }
        cur.0 = trust;
        cur.1 += 1;
        true
    }

    /// Read the trust file again if it changed on disk since last read.
    /// `None` when nothing changed; `Some(Ok(n))` with the number of stations
    /// after a reload that changed the list; `Some(Err)` when the file does not
    /// parse, in which case the list in use stays as it was.
    pub fn reload_if_changed(&self) -> Option<Result<usize, String>> {
        let path = self.file.as_deref()?;
        let now = modified(path);
        {
            let mut seen = self.seen.lock().expect("lock");
            if now == *seen {
                return None;
            }
            *seen = now;
        }
        match Trust::load(path) {
            Ok(t) => {
                let n = t.len();
                self.replace(t).then_some(Ok(n))
            }
            Err(e) => Some(Err(format!("{}: {e}", path.display()))),
        }
    }

    /// Trust `key` for exactly `call`, saving it to the trust file if there is one.
    pub fn add(&self, call: Callsign, key: PublicKey) -> io::Result<()> {
        self.edit(call, Some(key))
    }

    /// Stop trusting exactly `call`; false if it had no line of its own.
    pub fn remove(&self, call: Callsign) -> io::Result<bool> {
        if !self.get().iter().any(|(c, _)| c == call) {
            return Ok(false);
        }
        self.edit(call, None)?;
        Ok(true)
    }

    fn edit(&self, call: Callsign, key: Option<PublicKey>) -> io::Result<()> {
        // One edit at a time, so two API calls cannot lose each other's change.
        let mut seen = self.seen.lock().expect("lock");
        let mut t = self.get();
        match key {
            Some(k) => t.insert(call, k),
            None => {
                t.remove(call);
            }
        }
        if let Some(path) = &self.file {
            edit_trust_file(path, call, key)?;
            // Our own write is not a change to reload.
            *seen = modified(path);
        }
        self.replace(t);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &str) -> Callsign {
        Callsign::parse(s).unwrap()
    }

    #[test]
    fn edits_are_saved_and_outside_edits_are_picked_up() {
        let path = std::env::temp_dir().join(format!("hm-shared-trust-{}.txt", std::process::id()));
        std::fs::write(&path, "# friends\n").unwrap();
        let t = SharedTrust::new(Trust::load(&path).unwrap(), Some(path.clone()));
        assert_eq!((t.get().len(), t.version()), (0, 0));

        t.add(call("SO5KM-1"), PublicKey([1; 32])).unwrap();
        assert_eq!(t.get().key_for(call("SO5KM-1")), Some(PublicKey([1; 32])));
        assert_eq!(t.version(), 1);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("# friends\nSO5KM-1 0101"));
        assert_eq!(t.reload_if_changed(), None, "our own write is not a change");

        // Someone edits the file by hand: picked up on the next check.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!("SP5AAA {}\n", "02".repeat(32)));
        std::fs::write(&path, text).unwrap();
        assert_eq!(t.reload_if_changed(), Some(Ok(2)));
        assert_eq!(t.version(), 2);

        // A broken file is reported and the list in use stays.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "SP5AAA not-a-key\n").unwrap();
        assert!(matches!(t.reload_if_changed(), Some(Err(_))));
        assert_eq!(t.get().len(), 2);

        assert!(t.remove(call("SO5KM-1")).unwrap());
        assert!(!t.remove(call("SO5KM-1")).unwrap());
        std::fs::remove_file(&path).unwrap();
    }
}
