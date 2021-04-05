extern crate home;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Result, Write};
use std::path::PathBuf;
use std::time;
use tempfile::NamedTempFile;

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
    if let Some(home_dir) = home::home_dir() {
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
fn write_history_file(history: History) -> Result<()> {
    if let Some(home_dir) = home::home_dir() {
        write_history_file_internal(history, home_dir)?;
    } else {
        println!("SprayCC: Unable to get user home directory, history not saved");
    }
    Ok(())
}

fn write_history_file_internal(history: History, home_dir: PathBuf) -> Result<()> {
    let tmp = NamedTempFile::new_in(home_dir.as_path())?;

    // If there is an existing history, load it and this history will be merged into it
    let mut total_history = load_history_internal(&path_to_history(&home_dir)).unwrap_or(History::new());
    total_history.merge(history);

    if let Ok(data) = toml::to_vec(&total_history) {
        // Write the serialized dat and then rename the temporary file to main history file. This rename operation
        // is atomic, or as atomic as most file systems support.
        tmp.as_file().write_all(&data[..])?;
        tmp.persist(path_to_history(&home_dir))?;
    } else {
        println!("SprayCC: Internal error while serializing history data");
    }
    Ok(())
}

fn path_to_history(path: &PathBuf) -> PathBuf {
    path.join(".spraycc.history")
}

#[cfg(test)]
use tempfile::TempDir;

#[test]
fn test_read_write_history() {
    let foo_time = time::Duration::from_secs(5);
    let bar_time = time::Duration::from_secs(55);
    let mut h = History::new();
    h.update("foo.o", foo_time);
    h.update("bar.o", bar_time);

    let test_dir = TempDir::new_in(".").expect("Failed to create temporary directory in current directory");
    let home_dir = test_dir.path().to_path_buf();
    write_history_file_internal(h, home_dir.clone()).expect("Failed writing history");

    let htest = load_history_internal(&path_to_history(&home_dir)).expect("Error loading history file");
    assert_eq!(htest.get("foo.o"), Some(foo_time));
    assert_eq!(htest.get("bar.o"), Some(bar_time));
}

#[test]
fn test_merge_history() {
    let foo_time = time::Duration::from_secs(5);
    let bar_time = time::Duration::from_secs(55);
    let fuz_time = time::Duration::from_secs(321);

    let test_dir = TempDir::new_in(".").expect("Failed to create temporary directory in current directory");
    let home_dir = test_dir.path().to_path_buf();

    // Write a base history first
    let mut h2 = History::new();
    h2.update("fuz_test", fuz_time);
    write_history_file_internal(h2, home_dir.clone()).expect("Failed writing base history");

    let mut h = History::new();
    h.update("foo.o", foo_time);
    h.update("bar.o", bar_time);

    write_history_file_internal(h, home_dir.clone()).expect("Failed writing history");

    // The history loaded back should be a merge of the two
    let htest = load_history_internal(&path_to_history(&home_dir)).expect("Error loading history file");
    assert_eq!(htest.get("foo.o"), Some(foo_time));
    assert_eq!(htest.get("bar.o"), Some(bar_time));
    assert_eq!(htest.get("fuz_test"), Some(fuz_time));
}
