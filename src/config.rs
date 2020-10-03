extern crate toml;

use std::fs::{File, OpenOptions};
use std::io::{Read, Result, Write};
use std::path::{Path, PathBuf};

use super::ipc;

const CONNECT_FILE: &str = ".spraycc-server";

/// Write a configuration file describing the post and port the server is listening on and
/// the access code a client process should use.
pub fn write_server_contact_info(callme: &ipc::CallMe) -> Result<()> {
    let mut f = OpenOptions::new().create(true).write(true).open(CONNECT_FILE)?;
    let config = toml::to_string(callme).unwrap();
    f.write_all(config.as_bytes())
}

/// Read a server configuration from the file system
/// The current direct is checked first, then it moves to the parent directory until either a
/// configuration is found or None is returned
pub fn read_server_contact_info() -> Option<ipc::CallMe> {
    read_server_contact_info_from(Path::new(".").canonicalize().expect("Unable to read current directory"))
}

fn read_server_contact_info_from(mut p: PathBuf) -> Option<ipc::CallMe> {
    p.push(CONNECT_FILE);
    println!("Checking directory {}", p.to_string_lossy());
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

    write_server_contact_info(&config).unwrap();
    let c2 = read_server_contact_info().unwrap();
    assert_eq!(c2.addr, config.addr);
    assert_eq!(c2.access_code, config.access_code);

    std::fs::remove_file(CONNECT_FILE).unwrap();

    assert!(read_server_contact_info().is_none());
}
