extern crate home;

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Result, Write};
use std::path::{Path, PathBuf};
use std::time;
use std::{collections::HashMap, net::ToSocketAddrs};
use tempfile::tempfile;

/// Maps a unique target (output file, command line, etc) to a pair of the prior elapsed time for that task.
#[derive(Serialize, Deserialize, Clone)]
pub struct History {
    version: u32,
    /// Map target internally is a duration (elapsed of the task) and the time when it was recorded to
    /// allow stale entries to be flushed.
    history: HashMap<String, (time::Duration, time::SystemTime)>,
}

impl History {
    /// Constructs a new, empty history for the server to record into
    pub fn new() -> History {
        History {
            version: 1,
            history: HashMap::new(),
        }
    }

    /// Returns the runtime for task
    pub fn get(self: &History, target: &str) -> Option<time::Duration> {
        self.history.get(target).map(|v| (*v).0)
    }

    /// Update the record with a new entry. Any existing entry is replaced.
    pub fn update(self: &mut History, target: &str, elapsed: time::Duration) {
        self.history.insert(target.to_string(), (elapsed, time::SystemTime::now()));
    }

    /// Update 'self' with the more recent entries from the incoming history
    fn merge(self: &mut History, incoming: History) {
        self.history.extend(incoming.history);
    }
}

/// Reads the current history, returning an empty History object if no data are present.
pub fn load_current_history() -> History {
    if let Some(mut home_dir) = home::home_dir() {
        if let Ok(history) = load_history_internal(&path_to_history(&home_dir)) {
            return history;
        }
    }
    History::new()
}

/// Loads the history at the given location and returns the History instance or reports a warning to the
/// user if the file exists and isn't readable. History is optional so the expectation is things move ahead
/// with an empty one.
fn load_history_internal(path: &PathBuf) -> Result<History> {
    match read_history_file(path) {
        Ok(history) => Ok(history),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(err),
        Err(err) => {
            println!(
                "SprayCC: Warning reading existing history (ignored) {}:\n   {}",
                path.to_string_lossy(),
                err
            );
            Err(err)
        }
    }
}

/// Low-level history file reader.
fn read_history_file(path: &PathBuf) -> Result<History> {
    let mut f = OpenOptions::new().read(true).open(&path)?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;

    let history: History = toml::from_str(&buf)?;
    println!("SprayCC: Read history file from {}", path.to_string_lossy());
    Ok(history)
}

/// Writes the history to a unique, temporary file next to the real history and, once complete, renames
/// the temporary file to the final name. This avoids potential cases where the history is updated by
/// two servers at the same time.
fn write_history_file(history: &History) -> Result<()> {
    if let Some(home_dir) = home::home_dir() {
        let tmppath = unique_history_path(&home_dir);
        let mut f = File::open(&tmppath)?;
        if let Ok(data) = toml::to_vec(history) {
            f.write_all(&data[..])?;
            drop(f);

            std::fs::rename(tmppath, home_dir.join(".spraycc.history"))?;
        } else {
            println!("SprayCC: Internal error while serializing history data");
        }
    } else {
        println!("SprayCC: Unable to get user home directory, history not saved");
    }
    Ok(())
}

fn unique_history_path(path: &PathBuf) -> PathBuf {
    // TODO: Not likely to be unique
    path.join(".spraycc.history.5")
}

fn path_to_history(path: &PathBuf) -> PathBuf {
    path.join(".spraycc.history")
}
