extern crate nix;
///
/// The client is the command line wrapper for a single invokation of a compiler. It
/// submits the compilation command line to the server and waits around until the
/// results are available.
///
extern crate tokio;
extern crate which;

use std::error::Error;
use std::ffi::OsString;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::config;
use super::ipc;

/// A lazy file opener
enum LazyFile {
    FileName { file_name: String },
    File { file: tokio::fs::File },
}

impl LazyFile {
    pub fn new(file_name: String) -> LazyFile {
        LazyFile::FileName { file_name }
    }

    /// Opens the file if not already open
    pub async fn as_file(self: &mut LazyFile) -> Result<&mut tokio::fs::File, Box<dyn Error + Send + Sync>> {
        if let LazyFile::FileName { file_name } = self {
            *self = LazyFile::File {
                file: tokio::fs::File::create(file_name).await?,
            };
        }
        if let LazyFile::File { file } = self {
            Ok(file)
        } else {
            panic!("Should not be possible");
        }
    }
}

/// Runs the client which submits the task to the server and waits for the output of all generated files,
/// plus stdout/stderr to be returned.
pub async fn run(args: Vec<String>) -> Result<i32, Box<dyn Error + Send + Sync>> {
    println!(
        "Running compiler: {}",
        args.iter().fold(String::new(), |mut acc, s| {
            if !acc.is_empty() {
                acc.push_str(", ");
            }
            acc.push_str(s);
            acc
        })
    );

    // Figures out whether the command should run locally (exec from this process) or be sent to the server for
    // remote exec and which, if any, of the arguments are files generated by this command which need to be
    // repatriated to this host.
    let res = interpret_command_line(args);
    if let Err(e) = res {
        // TODO: error handling
        panic!("Error interpreting command line: {}", e);
        // return Err(e);
    }
    let (run_local, task) = res.unwrap();

    let mut status: Option<i32> = None;
    if run_local {
        unimplemented!("Run local");
    } else if let Some(callme) = config::read_server_contact_info() {
        match TcpStream::connect(callme.addr).await {
            Ok(stream) => {
                let mut conn = ipc::Connection::new(stream);

                let mut output_files = make_output_files(&task);

                // Start by sending the task to the server. This tells the server this process is a client as well as providing the task data.
                conn.write_message(&ipc::Message::Task {
                    access_code: callme.access_code,
                    details: task,
                })
                .await?;

                while status.is_none() {
                    match conn.read_message().await? {
                        Some(msg) => status = handle_msg(&mut output_files, msg).await?,
                        None => {
                            // TODO: This is only an issue if a task hasn't completed. But since we block on tasks...
                            println!("Client: connection dropped");
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                println!(
                    "Unable to connect to server at {}, please make sure it was started with 'spraycc server': {}",
                    callme.addr, err
                );
            }
        }
    } else {
        println!("Has the server been started in this same directory tree?");
        panic!("No server configuration found");
        // TODO: error handling
        // Err(String::from("No server configuration found"))
    }
    Ok(status.expect("No task status as set"))
}

/// Handles messages coming from the client via the server. These will be stderr, stdout and generated files from
/// the execution of the task, and then finally the task status.
///
/// Returns: None if all is fine and the task has not yet finished.
///          Some(status) if the task is finished where status == 0 means all ok, status > 0 is exit code,
///          and status < 0 means the task failed (couldn't find executable, aborted on signal, etc)
async fn handle_msg(output_files: &mut Vec<LazyFile>, msg: ipc::Message) -> Result<Option<i32>, Box<dyn Error + Sync + Send>> {
    match msg {
        ipc::Message::TaskOutput { output_type, content } => {
            write_output(output_files, output_type, content).await?;
            Ok(None)
        }
        ipc::Message::TaskFailed { error_message } => {
            println!("Task failed: {}", error_message);
            Ok(Some(-1))
        }
        ipc::Message::TaskDone { exit_code } => {
            // println!("Task done, exited with {:?}", exit_code);
            Ok(exit_code)
        }
        _ => {
            panic!("Unhandled message: {:?}", msg);
        }
    }
}

/// Writes returned content to stdout, stderr, or a file produced by the compiler
async fn write_output(output_files: &mut Vec<LazyFile>, output_type: ipc::OutputType, content: Vec<u8>) -> Result<(), Box<dyn Error + Send + Sync>> {
    match output_type {
        ipc::OutputType::Stdout => tokio::io::stdout().write_all(&content).await?,
        ipc::OutputType::Stderr => tokio::io::stderr().write_all(&content).await?,
        ipc::OutputType::File(idx) => output_files[idx].as_file().await?.write_all(&content).await?,
    }
    Ok(())
}

/// Generate a LazyFile for each of the output files
fn make_output_files(task: &ipc::TaskDetails) -> Vec<LazyFile> {
    task.output_args
        .iter()
        .map(|idx| LazyFile::new(task.args[*idx as usize].clone()))
        .collect()
}

/// Interprets the given command line
/// Based on the command being read, returns the indexes of the arguments which are the names of files written by this
/// command. That is, the files which need to be captured and returned to the origin.
///
/// # Returns
/// A tuple of (run locally, task description) on success, or an error message string
fn interpret_command_line(args: Vec<String>) -> Result<(bool, ipc::TaskDetails), String> {
    match which::which(OsString::from(&args[0])) {
        Ok(cmd) => {
            let cmd_name = cmd.file_name().map(|s| s.to_string_lossy()).unwrap(); // Can this fail if path exists?
                                                                                  // println!("Got file name: {}", cmd_name);

            let (keep_local, output_args) = {
                if cmd_name == "gcc" || cmd_name == "g++" || cmd_name == "clang" {
                    handle_gnu(&args)
                } else if cmd_name == "echo" || cmd_name == "ls" {
                    (false, Vec::new())
                } else {
                    return Err(String::from("Not implemented"));
                }
            };

            if let Ok(cwd) = std::env::current_dir() {
                Ok((
                    keep_local,
                    ipc::TaskDetails {
                        working_dir: cwd,
                        cmd,
                        args,
                        output_args,
                    },
                ))
            } else {
                Err(String::from("Unable to get current working directory"))
            }
        }
        Err(_) => Err(format!("Command {} not found", &args[0])),
    }
}

/// TODO: Replace with a user-configurable data structure which describes the various compilers and other executes
/// which may be used.
fn handle_gnu(args: &[String]) -> (bool, Vec<u16>) {
    // -o and -MF specify output file names.
    // -c, -s, -E, and -MD (dependency generation) are compilation of various sorts and should be remoted; lacking
    // those are linking which is kept local
    let mut keep_local = true;
    let mut output_idxs: Vec<u16> = Vec::new();
    for idx in 1..args.len() {
        let arg = &args[idx];
        if arg == "-c" || arg == "-s" || arg == "-e" || arg == "-MD" {
            keep_local = false;
        } else if idx < args.len() - 1 && (arg == "-o" || arg == "-MF") {
            output_idxs.push((idx + 1) as u16);
        }
    }
    (keep_local, output_idxs)
}

#[test]
fn test_cmdline_handling() {
    let cmdline: Vec<String> = vec!["g++", "test.cpp", "-c", "-o", "test.o"].iter().map(|s| s.to_string()).collect();
    let (local, task) = interpret_command_line(cmdline).unwrap();
    println!("Got task: {:?}", task);
    assert_eq!(local, false);
    assert_eq!(task.args[0], "g++");
}
