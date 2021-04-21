extern crate home;
extern crate rand;
extern crate rlimit;
extern crate tempfile;
extern crate toml;

use super::ipc;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Result, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const CONNECT_FILE: &str = ".spraycc-server";

/// A user-specific random key stored in ~/.spraycc.private
#[derive(Serialize, Deserialize, Debug)]
struct UserPrivateKey {
    key: u64,
}

#[derive(Deserialize, Debug)]
struct ConfigWrapper {
    exec: ExecConfig,
    command: Option<std::collections::HashMap<String, toml::map::Map<String, toml::Value>>>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CmdConfig {
    command: Vec<String>,
    output_files: Option<Vec<String>>,
}
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct ExecConfig {
    /// Number of execs to start immediately
    pub initial_count: usize,
    /// Maximum number of execs to start if there are enough tasks
    pub max_count: usize,
    /// Number of exec's pending as count climbs towards max
    pub keep_pending: usize,
    /// Idle delay when no more task have arrived to start releasing idle exec's
    pub release_delay: u64,
    /// Idle delay when no tasks are running or arriving to shutdown server
    pub idle_shutdown_after: u64,
    /// Command to use to start exec processes
    pub start_cmd: String,
    /// Alternate start command
    pub alt_start_cmd: Option<String>,
}

impl Default for ExecConfig {
    fn default() -> Self {
        ExecConfig {
            initial_count: 5,
            max_count: 1000,
            keep_pending: 5,
            release_delay: 30,
            idle_shutdown_after: 60,
            start_cmd: "spraycc".to_string(), // TODO: lookup up path to this process
            alt_start_cmd: None,
        }
    }
}

/// Only exists to clean up the server configuration file at exit
pub struct ServerConfigCleanup {
    file: PathBuf,
}

impl ServerConfigCleanup {
    fn new(file_to_delete: PathBuf) -> ServerConfigCleanup {
        ServerConfigCleanup { file: file_to_delete }
    }
}

impl Drop for ServerConfigCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

/// Reads a configuration file and returns the config object
pub fn load_config_file() -> ExecConfig {
    if let Ok(config) = load_config_file_internal(&PathBuf::from(".spraycc")) {
        return config;
    } else if let Some(mut home_dir) = home::home_dir() {
        home_dir.push(".spraycc");
        if let Ok(config) = load_config_file_internal(&home_dir) {
            return config;
        }
    }
    ExecConfig::default()
}

fn load_config_file_internal(path: &PathBuf) -> Result<ExecConfig> {
    match read_config_file(path) {
        Ok(config) => Ok(config),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(err),
        Err(err) => {
            println!("SprayCC: Error reading configuration file {}:\n   {}", path.to_string_lossy(), err);
            Err(err)
        }
    }
}

fn read_config_file(path: &PathBuf) -> Result<ExecConfig> {
    let mut f = OpenOptions::new().read(true).open(&path)?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;

    let config: ConfigWrapper = toml::from_str(&buf)?;
    println!("SprayCC: Read config file from {}", path.to_string_lossy());
    Ok(config.exec)
}

/// Write a configuration file describing the post and port the server is listening on and
/// the access code a client process should use.
pub fn write_server_contact_info(callme: &ipc::CallMe) -> Result<ServerConfigCleanup> {
    let mut f = OpenOptions::new().create(true).write(true).open(CONNECT_FILE)?;
    let config = toml::to_string(callme).unwrap();
    f.write_all(config.as_bytes())?;
    Ok(ServerConfigCleanup::new(PathBuf::from(CONNECT_FILE)))
}

/// Read a server configuration from the file system
/// The current direct is checked first, then it moves to the parent directory until either a
/// configuration is found or None is returned
pub fn read_server_contact_info() -> Option<ipc::CallMe> {
    read_server_contact_info_from(Path::new(".").canonicalize().expect("Unable to read current directory"))
}

fn read_server_contact_info_from(mut p: PathBuf) -> Option<ipc::CallMe> {
    p.push(CONNECT_FILE);
    if p.exists() {
        match OpenOptions::new().read(true).open(&p) {
            Ok(mut f) => deserialize_contact_info(&p, &mut f),
            Err(e) => {
                println!("SprayCC: Unable to open server config file {}: {}", p.to_string_lossy(), e);
                None
            }
        }
    } else if p.pop() && p.pop() {
        read_server_contact_info_from(p)
    } else {
        println!("SprayCC: No server config file found in this directory or any parent directory");
        None
    }
}

fn deserialize_contact_info(p: &PathBuf, f: &mut File) -> Option<ipc::CallMe> {
    let mut buf = String::new();
    match f.read_to_string(&mut buf) {
        Ok(_) => match toml::from_str::<ipc::CallMe>(&buf) {
            Ok(config) => Some(config),
            Err(e) => {
                println!("SprayCC: Error deserializing server config file {}; {}", p.to_string_lossy(), e);
                None
            }
        },
        Err(e) => {
            println!("SprayCC: Error reading server config file {}: {}", p.to_string_lossy(), e);
            None
        }
    }
}

/// Reads the user's private key from ~/.spraycc.private
pub fn load_user_private_key(create_if_not_present: bool) -> u64 {
    if let Some(mut home_dir) = home::home_dir() {
        home_dir.push(".spraycc.private");
        match load_user_private_key_internal(&home_dir) {
            Ok(key) => return key,
            Err(err) if create_if_not_present && err.kind() == std::io::ErrorKind::NotFound => {
                if let Ok(key) = write_user_private_key(&home_dir) {
                    return key;
                }
            }
            Err(err) => {
                println!("SprayCC: Error reading user-private key file {}:\n   {}", home_dir.to_string_lossy(), err);
            }
        }
    }
    13
}

/// Reads a private key from the given path and returns the key itself or an error
fn load_user_private_key_internal(path: &PathBuf) -> Result<u64> {
    let mut f = OpenOptions::new().read(true).open(&path)?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;

    let key: UserPrivateKey = toml::from_str(&buf)?;
    Ok(key.key)
}

/// Generates a random private key and writes it to the specified path. The generated
/// key is returned. The key file is user-private.
fn write_user_private_key(path: &PathBuf) -> Result<u64> {
    let key = UserPrivateKey {
        key: rand::random::<u64>() >> 1,
    };
    let mut f = OpenOptions::new().create(true).write(true).mode(0o600).open(path)?;
    let keystr = toml::to_string(&key).unwrap();
    f.write_all(keystr.as_bytes())?;
    println!("SprayCC: Generated user-private key in {}", path.to_string_lossy());
    Ok(key.key)
}

/// The server requires quite a few open file handles: 1 per exec + 1 per client + 2 per pending exec
/// On some systems the soft limit is too low. This attempts to raise it to 2048 if needed and lets
/// the user know if not.
pub fn setup_process_file_limit(verbose: bool) {
    // Validate the user's rlimit for open file descriptors is high enough
    if let Ok((soft_limit, hard_limit)) = rlimit::getrlimit(rlimit::Resource::NOFILE) {
        // println!("Resource limit: {}, {}", soft_limit.as_usize(), hard_limit);
        if hard_limit.as_usize() < 2048 {
            println!(
                "SprayCC: Warning: hard-limit on open file scriptors (ulimit -n) is only {}, may limit parallelism",
                hard_limit
            );
        } else if soft_limit.as_usize() < 2048 {
            let new_limit = rlimit::Rlim::from_usize(std::cmp::min(4096, hard_limit.as_usize()));
            if rlimit::setrlimit(rlimit::Resource::NOFILE, new_limit, hard_limit).is_err() {
                println!(
                    "SprayCC: Warning: soft-limit on open file descriptors is only {}, consider using ulimit -n to increase above 2048",
                    soft_limit
                );
            } else if verbose {
                println!("SprayCC: Successfully increased file descriptor limit to {}", new_limit);
            }
        }
    }
}

#[cfg(test)]
use std::os::unix::fs::MetadataExt;

#[test]
fn config_read_write_callme() {
    let config = ipc::CallMe {
        addr: "192.168.5.6:1234".parse().unwrap(),
        access_code: 6789,
    };

    {
        let _remover = write_server_contact_info(&config).unwrap();
        let c2 = read_server_contact_info().unwrap();
        assert_eq!(c2.addr, config.addr);
        assert_eq!(c2.access_code, config.access_code);
    }

    assert!(read_server_contact_info().is_none());
}

#[test]
fn config_read_write_private_key() {
    let test_dir = tempfile::TempDir::new_in(".").expect("Failed to create temporary directory in current directory");
    let private_file = test_dir.path().to_path_buf().join("private");

    // Default if it doesn't exist
    assert!(
        load_user_private_key_internal(&private_file).is_err(),
        "Expected error reading private file {:?}",
        private_file
    );

    // Create the file
    let value_new = write_user_private_key(&private_file).expect(&format!("Error writing private file {:?}", private_file));

    // File is user-private?
    let mode = File::open(&private_file).unwrap().metadata().unwrap().mode();
    assert_eq!(mode & 0o777, 0o600);

    // Reread the file, same key?
    let value_reread = load_user_private_key_internal(&private_file).unwrap();
    assert_eq!(value_new, value_reread);
}
