use minicbor::Encode;

use super::*;

mod custody;
mod persistence;
mod queue;
mod receipts;
mod relay;

fn call(s: &str) -> Callsign {
    Callsign::parse(s).unwrap()
}

fn id(n: u8) -> ObjectId {
    ObjectId([n; 32])
}

struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p = std::env::temp_dir().join(format!("hm-store-{}-{}.db", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        TempDb(p)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
