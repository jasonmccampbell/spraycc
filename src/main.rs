extern crate clap;
extern crate tokio;

use clap::{App, AppSettings, Arg, SubCommand};
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;

/// # client
/// Module implementing the client wrapper which submits a task to the cluster
mod client;
/// # config
/// Utilities for reading and writing application configurations
mod config;
/// # exec
/// Module implementing the 'exec' subcommand
mod exec;
/// # ipc
/// Inter-process communication utilities, mostly focused on message passing
mod ipc;
/// # server
/// Implements the 'server' subcommand
mod server;
/// # tasks
/// The tasks module provides services for starting a defined task and mapping generated output to
/// pipes which can be read by the caller.
pub mod task;

/// Main entry point which just delegates to the appropriate module
#[tokio::main]
async fn main() {
    let matches = App::new("SprayCC")
        .version("0.1.0")
        .about("SprayCC - distributed compiler wrapper")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .subcommand(
            SubCommand::with_name("exec")
                .about("For running the executor process in the cluster")
                .arg(
                    Arg::with_name("callme")
                        .long("callme")
                        .takes_value(true)
                        .help("Provides the IP address and port of the server"),
                )
                .arg(
                    Arg::with_name("access_code")
                        .long("code")
                        .takes_value(true)
                        .help("Proivdes the access code specified by the server"),
                ),
        )
        .subcommand(SubCommand::with_name("server").about("Starts the SprayCC server, if not already running"))
        .subcommand(
            SubCommand::with_name("run")
                .about("Sends the command on the rest of the command line to the server for execution")
                .setting(AppSettings::TrailingVarArg)
                .arg(Arg::with_name("compiler_options").help("Command line to be executed").multiple(true)),
        )
        // Used for testing to fake running a command that writes a file
        .subcommand(
            SubCommand::with_name("fakecc")
                .setting(AppSettings::Hidden)
                .setting(AppSettings::TrailingVarArg)
                .arg(
                    Arg::with_name("output_file")
                        .help("Specifies one or more files to be written")
                        .short("o")
                        .multiple(true)
                        .takes_value(true),
                )
                .arg(Arg::with_name("fail").long("fail").help("If present, the exit status is non-zero")),
        )
        .get_matches();

    let res: Result<(), Box<dyn Error + Send + Sync>> = if matches.subcommand_matches("server").is_some() {
        server::run().await
    } else if let Some(exec) = matches.subcommand_matches("exec") {
        assert!(exec.is_present("callme") && exec.is_present("access_code"));
        let addr = exec.value_of("callme").unwrap();
        let code = exec.value_of("access_code").unwrap();

        if let Ok(code) = code.parse() {
            if let Ok(addr) = addr.parse() {
                let callme = ipc::CallMe { addr, access_code: code };
                exec::run(callme).await
            } else {
                panic!("Error: 'callme' must be an IPv4 address and port number, got: {}", addr);
            }
        } else {
            panic!("Error: access code must be a numeric value, got: {}", code);
        }
    } else if let Some(run) = matches.subcommand_matches("run") {
        let args: Vec<String> = run.values_of("compiler_options").unwrap().map(String::from).collect();
        match client::run(args).await {
            Ok(ec) if ec > 0 => {
                // Remote task failed so exit with the same exit code
                std::process::exit(ec);
            }
            Ok(ec) if ec < 0 => {
                // Remote task failed with signal
                std::process::exit(-1);
            }
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    } else if let Some(fakecc) = matches.subcommand_matches("fakecc") {
        for output in fakecc.values_of("output_file").unwrap() {
            // C++: OpenOptions uses the builder pattern to do essentially the same as or'ing bit flags
            //      together to specify the file mode. But it is done in a manner which allows the combinations
            //      to be validated and can be more cross-platform in some cases.
            // C++: In a single line we open the file, write the result, and release the File object, closing
            //      the file similar to using C++ streams. The 'unwrap' calls assert if anything fails since
            //      we don't care about real error handling here.
            OpenOptions::new()
                .create(true)
                .write(true)
                .open(&output)
                .unwrap()
                .write_all(output.as_bytes())
                .unwrap();
            println!("Wrote file {}", &output);
        }
        if fakecc.is_present("fail") {
            panic!("Failing as requested");
        }
        Ok(())
    } else {
        unreachable!();
    };

    // TODO: Better way to control exit code from async func? Non-async main?
    std::process::exit(match res {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("Error: {}", e);
            1
        }
    });
}