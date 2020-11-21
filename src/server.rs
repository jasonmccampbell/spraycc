extern crate get_if_addrs;
extern crate tokio;

// Setup some tokens to allow us to identify which event is
// for which socket.
use get_if_addrs::{get_if_addrs, Interface};
use std::collections::VecDeque;
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, SystemTime};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::config::{load_config_file, write_server_contact_info, ExecConfig};
use super::ipc;

const ZERO_DURATION: Duration = Duration::from_secs(0);

#[derive(Debug, PartialEq)]
enum ConnectionType {
    Pending,
    Client,
    Exec,
    Dead,
}

/// Defines one remote connection. Each connection is a task which manages the socket connection and
/// communicates with the server over the defined channels. 'send' and 'recv' are relative to the server.
struct Remote {
    conn_type: ConnectionType,
    // Channel for sending message to the remote task
    send_chan: mpsc::Sender<ipc::Message>,
    // Paired remote ID. For client type, this is the executor ID which is running the task, or None if
    // not assigned yet. For executors, it is the ID of the client which submitted the task, or None.
    paired_id: Option<u32>,
}

struct ServerState {
    /// Connections to remote processes, both clients (providing a task) and execs (processing N tasks).
    /// Array position corresponds to the ID of the client/exec.
    remotes: Vec<Remote>,
    /// Queue of tasks submitted and the ID of the client which submitted it, not yet assigned to an exec
    task_queue: VecDeque<(usize, ipc::TaskDetails)>,
    /// Queue of exec IDs waiting for tasks
    exec_queue: VecDeque<usize>,
    /// Total tasks submitted
    submit_count: usize,
    /// Total tasks completed
    finish_count: usize,
    /// Number of exec processes start but which have not yet phoned home
    pending_exec_count: usize,
    /// Number of currently running exec processes
    running_exec_count: usize,
    /// Timestamp when the most recent task was received, or none if no task yet received
    last_submit_time: Option<SystemTime>,
    /// Timestamp when last activity occured
    last_activity_time: SystemTime,
    // User configuration parameters
    user_config: ExecConfig,
    // Exec process start command
    start_cmd: String,
}

impl ServerState {
    fn new(port: ipc::CallMe) -> ServerState {
        let config = load_config_file();
        let start_cmd = format!("{} exec --callme={} --code={}", config.start_cmd, port.addr, port.access_code);
        println!("Generated start command: {}", &start_cmd);

        ServerState {
            remotes: vec![],
            task_queue: VecDeque::new(),
            exec_queue: VecDeque::new(),
            submit_count: 0,
            finish_count: 0,
            pending_exec_count: 0,
            running_exec_count: 0,
            last_submit_time: None,
            last_activity_time: SystemTime::now(),
            user_config: config,
            start_cmd: start_cmd,
        }
    }

    /// Resets the inactivity timer
    fn activity_occurred(self: &mut ServerState) {
        self.last_activity_time = SystemTime::now();
    }

    /// True if no tasks are queued and no activity has occurred within the shutdown period
    fn ok_to_shutdown(self: &ServerState) -> bool {
        return self.task_queue.is_empty()
            && SystemTime::now()
                .duration_since(self.last_activity_time)
                .unwrap_or(ZERO_DURATION)
                .as_secs()
                > self.user_config.idle_shutdown_after;
    }

    /// Sends a message to the host paired with the given host. That is, given an exec host, send a message to the client
    /// which submitted the task or given a client and a message to the host executing the task.
    async fn send_to_paired_remote(self: &mut ServerState, conn_id: usize, msg: ipc::Message) -> Result<(), Box<dyn Error + Send + Sync>> {
        let other_id = self.remotes[conn_id].paired_id.expect("Expected connection to already be paired") as usize;

        // Forward to client, as long as it hasn't already been marked 'dead'
        if self.remotes[other_id].conn_type == ConnectionType::Client {
            if let Err(err) = self.remotes[other_id].send_chan.send(msg).await {
                println!("Error sending to remote (was ctrl-c hit?): {}", err);
                self.remotes[other_id].conn_type = ConnectionType::Dead;
            }
        }
        Ok(())
    }

    /// Adds a new remote which has connected to the server
    fn add_remote(self: &mut ServerState, tx: mpsc::Sender<ipc::Message>) -> usize {
        self.remotes.push(Remote {
            conn_type: ConnectionType::Pending,
            send_chan: tx,
            paired_id: None,
        });
        self.remotes.len() - 1
    }

    /// A connection was dropped, perhaps expectedly, perhaps not.
    async fn handle_dropped(self: &mut ServerState, conn_id: usize) -> Result<(), Box<dyn Error + Send + Sync>> {
        let remote = &self.remotes[conn_id];
        if let Some(other_id) = remote.paired_id {
            if remote.conn_type == ConnectionType::Exec {
                // Notify the client that something bad has happened...
                self.remotes[other_id as usize]
                    .send_chan
                    .send(ipc::Message::TaskFailed {
                        error_message: String::from("exec dropped connection"),
                    })
                    .await?;
            }
        }

        if remote.conn_type == ConnectionType::Exec {
            self.running_exec_count -= 1;
        }
        self.remotes[conn_id].conn_type = ConnectionType::Dead;
        Ok(())
    }

    /// A remote is identified after it sends its first message. In this case, the remote is a client and submitted
    /// a task. Queue processing is done as a part of this function, so the submitted task may be sent out immediately
    /// or queued for later processing.
    async fn remote_is_client(self: &mut ServerState, conn_id: usize, details: ipc::TaskDetails) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.remotes[conn_id].conn_type = ConnectionType::Client;
        self.task_queue.push_back((conn_id, details));
        self.last_submit_time = Some(SystemTime::now());
        self.update().await
    }

    /// A remote is identified after it sends its first message. In this case, the remote is an executor and is now
    /// available to start processing tasks. If one is available, the task is sent to it.
    async fn remote_is_exec(self: &mut ServerState, conn_id: usize) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.remotes[conn_id].conn_type = ConnectionType::Exec;
        self.running_exec_count += 1;
        if self.pending_exec_count > 0 {
            self.pending_exec_count -= 1;
        }
        self.exec_is_ready(conn_id).await
    }

    /// A remote exec process is ready for a new task, either because it is new or has completed the assigned task
    async fn exec_is_ready(self: &mut ServerState, conn_id: usize) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.exec_queue.push_back(conn_id);
        self.remotes[conn_id].paired_id = None;
        self.update().await
    }

    /// Processes the queues and starts/shuts down exec processes
    async fn update(self: &mut ServerState) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.start_execs().await;
        self.check_queues().await
    }

    /// Check the task and ready exec queues to see if anything can be assigned
    async fn check_queues(self: &mut ServerState) -> Result<(), Box<dyn Error + Send + Sync>> {
        while !self.task_queue.is_empty() && !self.exec_queue.is_empty() {
            let (client_id, task) = self.task_queue.pop_front().unwrap();
            let exec_id = self.exec_queue.pop_front().unwrap();
            assert!(client_id != exec_id, "Somehow exec and client ID are the same");

            // Pair the client w/ the executor running the task
            let client = &mut self.remotes[client_id];
            assert!(
                client.paired_id.is_none(),
                "Client {} paired ID should be None, got {:?}",
                client_id,
                client.paired_id
            );
            client.paired_id = Some(exec_id as u32);

            // ... and the executor with the client submitting the task
            let exec = &mut self.remotes[exec_id];
            assert!(
                exec.paired_id.is_none(),
                "Exec {} paired ID should be None, got {:?}",
                exec_id,
                exec.paired_id
            );
            exec.paired_id = Some(client_id as u32);

            // println!(
            //     "Assigning task to exec {}: {}",
            //     exec_id,
            //     task.args.iter().fold(String::new(), |mut acc, s| {
            //         if !acc.is_empty() {
            //             acc.push_str(" ");
            //         }
            //         acc.push_str(s);
            //         acc
            //     })
            // );

            // Send the task
            exec.send_chan
                .send(ipc::Message::Task {
                    access_code: 0,
                    details: task,
                })
                .await?;
        }

        // If no more task are coming, shut down idle executors
        let ok_to_release = if let Some(last_task_time) = self.last_submit_time {
            SystemTime::now().duration_since(last_task_time).unwrap_or(ZERO_DURATION).as_secs() > self.user_config.release_delay
        } else {
            false
        };

        while ok_to_release && !self.exec_queue.is_empty() {
            let exec_id = self.exec_queue.pop_front().unwrap();
            let exec = &mut self.remotes[exec_id];
            exec.conn_type = ConnectionType::Dead;
            exec.send_chan.send(ipc::Message::PissOff {}).await?;
        }

        println!(
            "Status: {} / {} finished, {} running{}",
            self.finish_count,
            self.submit_count,
            self.submit_count - self.finish_count - self.task_queue.len(),
            if ok_to_release { " (releasing execs)" } else { "" }
        );
        Ok(())
    }

    /// Check to see if we need to start more exec engines
    async fn start_execs(self: &mut ServerState) {
        if self.task_queue.is_empty() {
            return;
        }
        while self.running_exec_count + self.pending_exec_count < self.user_config.max_count
            && self.pending_exec_count < self.user_config.keep_pending
            || self.running_exec_count + self.pending_exec_count < self.user_config.initial_count
        {
            match Command::new("sh").arg("-c").arg(&self.start_cmd).spawn() {
                Ok(_) => {
                    self.pending_exec_count += 1;
                    println!("Exec started");
                    // Tokio does not do kill-on-drop by default, so we let the child run and hopefully finish quickly
                }
                Err(err) => {
                    println!("Error starting exec process '{}': {}", &self.start_cmd, err);
                }
            }
        }
    }
}

/// Main server loop. This starts a socket listener to wait for remote connections. Each connection
/// is spun off as a separate task, which relays backs back over an MPSC.
pub async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let ip_addr = get_network_addr();
    let callme = ipc::CallMe {
        addr: SocketAddr::new(ip_addr, 45678),
        access_code: 42,
    };
    write_server_contact_info(&callme)?;

    let listener = TcpListener::bind(&callme.addr).await?;
    println!("Server started on {}", listener.local_addr().unwrap());

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<(usize, ipc::Message)>(256);

    // Make sure we check on things even if no clients are communicating
    let mut watchdog: tokio::time::Sleep;

    let mut server_state = ServerState::new(callme);
    loop {
        watchdog = tokio::time::sleep(Duration::from_secs(5));
        tokio::select! {
            socket_res = listener.accept() => {
                let (socket, _) = socket_res.expect("Error accepting incoming connection");
                // A new task is spawned for each inbound socket. The socket is
                // moved to the new task and processed there.
                let (outbound_tx, outbound_rx) = mpsc::channel::<ipc::Message>(256);
                let remote_id = server_state.add_remote(outbound_tx);
                let in_tx = inbound_tx.clone();
                tokio::spawn(async move {
                    match handle_connection(socket, remote_id, in_tx, outbound_rx).await {
                        Ok(_) => {
                            // pass
                        }
                        Err(err) => {
                            println!("Connection {}: error on connection: {}", remote_id, err);
                        }
                    }
                });
            }
            res = inbound_rx.recv() => {
                match res {
                    Some((id, msg)) => handle_msg(&mut server_state, id, msg).await?,
                    None => break,
                }
                server_state.activity_occurred();
            }
            _ = &mut watchdog => {
                server_state.update().await?
            }
        }

        if server_state.ok_to_shutdown() {
            break;
        }
    }
    Ok(())
}

/// Task for handling connections by relating messages between the internal channels and the socket.
async fn handle_connection(
    stream: TcpStream,
    conn_id: usize,
    tx_chan: mpsc::Sender<(usize, ipc::Message)>,
    mut rx_chan: mpsc::Receiver<ipc::Message>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut conn = ipc::Connection::new(stream);
    loop {
        tokio::select! {
            res = conn.read_message() => {
                match res {
                    Ok(Some(msg)) => tx_chan.send((conn_id, msg)).await?,
                    Ok(None) => {
                        break;
                    }
                    Err(err) => {
                        println!("Error reading from socket {}: {}", conn_id, err);
                        break;
                    }
                }
            }
            res = rx_chan.recv() => {
                if let Some(msg) = res {
                    conn.write_message(&msg).await?;
                } else {
                    break;
                }
            }
        }
    }
    tx_chan.send((conn_id, ipc::Message::Dropped)).await?;
    Ok(())
}

/// On the main server thread, handle a message from either a client or exec remote. Often this involves
/// forwarding a message to another remote.
async fn handle_msg(server_state: &mut ServerState, conn_id: usize, msg: ipc::Message) -> Result<(), Box<dyn Error + Send + Sync>> {
    match msg {
        ipc::Message::YourObedientServant { access_code } => {
            server_state.remote_is_exec(conn_id).await?;
        }
        ipc::Message::Task { access_code, details } => {
            server_state.submit_count += 1;
            server_state.remote_is_client(conn_id, details).await?;
        }
        ipc::Message::TaskOutput { .. } => {
            server_state.send_to_paired_remote(conn_id, msg).await?;
        }
        ipc::Message::TaskDone { .. } => {
            server_state.send_to_paired_remote(conn_id, msg).await?;
            server_state.finish_count += 1;
            server_state.exec_is_ready(conn_id).await?;
        }
        ipc::Message::TaskFailed { .. } => {
            server_state.send_to_paired_remote(conn_id, msg).await?;
            server_state.finish_count += 1;
            server_state.exec_is_ready(conn_id).await?;
        }
        ipc::Message::CancelTask { .. } => {
            // TODO: implement cancellation
            unimplemented!("Task cancellation");
        }
        ipc::Message::Dropped => {
            server_state.handle_dropped(conn_id).await?;
        }
        ipc::Message::PissOff { .. } => {
            panic!("A lowly remote should never tell the server to piss off");
        }
    }
    Ok(())
}

fn get_network_addr() -> std::net::IpAddr {
    match get_if_addrs() {
        Ok(ifaces) => {
            let non_loops: Vec<&Interface> = ifaces.iter().filter(|iface| !iface.is_loopback()).collect();
            if non_loops.is_empty() {
                println!("No network interfaces found, falling back to loopback");
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
            } else {
                if non_loops.len() > 1 {
                    println!(
                        "Warning: multiple network interace found, using {} ({})",
                        non_loops[0].name,
                        non_loops[0].ip()
                    );
                }
                non_loops[0].ip()
            }
        }
        Err(err) => {
            println!("Warning: unable to get network interfaces, defaulting to loopback: {}", err);
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        }
    }
}
