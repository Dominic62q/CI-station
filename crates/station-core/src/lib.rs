//! Station core: event model, SQLite store, spool ingest, git state and the
//! test runner. No UI code lives here.

pub mod board;
pub mod event;
pub mod git;
pub mod github;
pub mod pipeline;
pub mod spool;
pub mod store;
pub mod testrun;
pub mod timeline;

use std::path::PathBuf;

/// Station's data directory: `$STATION_HOME`, else `~/.station`.
pub fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("STATION_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".station")
}

/// Where the hook writes. Matches the plugin: `$STATION_SPOOL_DIR`, else
/// `~/.station/spool`.
pub fn spool_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("STATION_SPOOL_DIR") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".station").join("spool")
}

pub fn db_path() -> PathBuf {
    home_dir().join("station.db")
}
