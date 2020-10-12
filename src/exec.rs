extern crate nix;
extern crate tempdir;
///
/// The executor processes are submitted to the farm and then pull jobs from the
/// server, execute them, and return the results. The process goes away when told
/// to or when they lose connection to the server.
///
extern crate tokio;

use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::select;

use super::ipc;
use super::task::Task;
use std::error::Error;

/// Runs the executor, which means:
///  1 - Establish a connection to the server (ObedientServer message)
///  2 - Process each sent task and return the results (TaskOutput, TaskDone)
///      2a - Deal with any cancellation messages
///  3 - Continue processing tasks until told to terminate
pub async fn run(callme: ipc::CallMe) -> Result<(), Box<dyn Error + Send + Sync>> {
    let stream = TcpStream::connect(callme.addr).await?;
    let mut conn = ipc::Connection::new(stream);
    conn.write_message(&ipc::Message::YourObedientServant { access_code: 42 }).await?;

    loop {
        match conn.read_message().await? {
            Some(msg) => match msg {
                ipc::Message::Task { access_code: _, details } => {
                    // TODO: Blocks, should be spawned so we can listen for cancel messages
                    handle_new_task(&mut conn, details).await?;
                }
                ipc::Message::CancelTask => {
                    unimplemented!("Task cancellation is unimplemented");
                }
                ipc::Message::PissOff => {
                    break;
                }
                _ => {
                    panic!("Unexpected message: {:?}", msg);
                }
            },
            None => {
                // TODO: This is only an issue if a task hasn't completed. But since we block on tasks...
                println!("Exec: connection dropped");
                break;
            }
        }
    }
    Ok(())
}

async fn handle_new_task(conn: &mut ipc::Connection, details: ipc::TaskDetails) -> Result<(), Box<dyn Error + Send + Sync>> {
    println!("Got task {:?}", details);
    match Task::start(&details.working_dir, &details.cmd, details.args, &details.output_args) {
        Ok(task) => run_task(conn, task).await,
        Err(err) => Err(err.into()),
    }
}

async fn run_task(conn: &mut ipc::Connection, mut task: Task) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut stdout_reader = BufReader::new(task.child.stdout.take().unwrap()).lines();
    let mut stderr_reader = BufReader::new(task.child.stderr.take().unwrap()).lines();

    let mut stdout_done = false;
    let mut stderr_done = false;

    while !stdout_done || !stderr_done {
        select! {
            line_res = stdout_reader.next_line(), if !stdout_done => {
                if let Ok(None) = line_res {
                    stdout_done = true;
                } else if let Ok(Some(mut line)) = line_res {
                    println!("Got stdout: {}", &line);
                    line.push('\n');
                    conn.write_message(&ipc::Message::TaskOutput { output_type : ipc::OutputType::Stdout, content : Vec::from(line) }).await?;

                } else {
                    println!("Error reading stdout");
                    stdout_done = true;
                }
            },
            line_res = stderr_reader.next_line(), if !stderr_done => {
                if let Ok(None) = line_res{
                    stderr_done = true;
                } else if let Ok(Some(mut line)) = line_res {
                    line.push('\n');
                    conn.write_message(&ipc::Message::TaskOutput { output_type : ipc::OutputType::Stderr, content : Vec::from(line) }).await?;
                } else {
                    println!("Error reading stderr");
                    stderr_done = true;
                }
            }
        }
    }

    println!("Sending {} output files", task.output_files.len());
    for (idx, generated_file) in task.output_files.iter().enumerate() {
        send_output_file(conn, ipc::OutputType::File(idx), &generated_file).await?;
    }

    let status = task.child.await?;
    conn.write_message(&ipc::Message::TaskDone { exit_code: status.code() }).await?;
    println!("Status = {:?}", status);
    Ok(())
}

/// Read the specified file and send the contents as one or more TaskOutput messages
async fn send_output_file(conn: &mut ipc::Connection, output_type: ipc::OutputType, path: &PathBuf) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Ignore file open errors, these are typically due to compiler errors. No output is sent, which indicates the file should not
    // be created on the other end.
    if let Ok(mut fs) = tokio::fs::File::open(path.as_os_str()).await {
        println!("Sending output file {}", path.to_string_lossy());
        loop {
            let mut buf: Vec<u8> = vec![0; 65536];
            let n = fs.read(buf.as_mut_slice()).await?;
            if n > 0 {
                buf.truncate(n);
                conn.write_message(&ipc::Message::TaskOutput { output_type, content: buf }).await?
            } else {
                break;
            };
        }
    }
    // TODO: Handle other kinds of errors?
    Ok(())
}