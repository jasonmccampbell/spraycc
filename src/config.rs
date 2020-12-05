extern crate home;
extern crate toml;

use super::ipc;
use serde::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::{Read, Result, Write};
use std::path::{Path, PathBuf};

const CONNECT_FILE: &str = ".spraycc-server";

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
            println!("Error reading configuration file {}:\n   {}", path.to_string_lossy(), err);
            Err(err)
        }
    }
}

fn read_config_file(path: &PathBuf) -> Result<ExecConfig> {
    let mut f = OpenOptions::new().read(true).open(&path)?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;

    let config: ConfigWrapper = toml::from_str(&buf)?;
    println!("Read config file from {}", path.to_string_lossy());
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
    // println!("Checking directory {}", p.to_string_lossy());
    if p.exists() {
        match OpenOptions::new().read(true).open(&p) {
            Ok(mut f) => deserialize_contact_info(&p, &mut f),
            Err(e) => {
                println!("Unable to open server config file {}: {}", p.to_string_lossy(), e);
                None
            }
        }
    } else if p.pop() && p.pop() {
        read_server_contact_info_from(p)
    } else {
        println!("No server config file found in this directory or any parent directory");
        None
    }
}

fn deserialize_contact_info(p: &PathBuf, f: &mut File) -> Option<ipc::CallMe> {
    let mut buf = String::new();
    match f.read_to_string(&mut buf) {
        Ok(_) => match toml::from_str::<ipc::CallMe>(&buf) {
            Ok(config) => Some(config),
            Err(e) => {
                println!("Error deserializing server config file {}; {}", p.to_string_lossy(), e);
                None
            }
        },
        Err(e) => {
            println!("Error reading server config file {}: {}", p.to_string_lossy(), e);
            None
        }
    }
}

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
