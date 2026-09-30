//! The key and `station.toml` setup writes, never over existing files.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hm_wire::Callsign;

use crate::config::Config;
use crate::files::KeyFile;

pub(super) fn path_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// Key/store basename from the config path: `core.toml` → `core.key` / `core.db`.
pub(super) fn companion_name(config_path: &Path, extension: &str) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("station");
    PathBuf::from(format!("{stem}.{extension}"))
}

pub(super) fn ensure_paths_available(config_path: &Path, key_path: &Path) -> io::Result<()> {
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

pub(super) fn save_setup(
    config_path: &Path,
    key_path: &Path,
    config: &Config,
    call: Callsign,
) -> io::Result<KeyFile> {
    ensure_paths_available(config_path, key_path)?;
    let key = KeyFile::generate(call)?;
    key.save(key_path).map_err(|error| path_error(key_path, error))?;
    if let Err(error) = config.save_new(config_path) {
        let _ = fs::remove_file(key_path);
        return Err(path_error(config_path, error));
    }
    Ok(key)
}
