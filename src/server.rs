extern crate get_if_addrs;
extern crate priority_queue;
extern crate rand;
extern crate simple_process_stats;
extern crate tokio;
extern crate ubyte;

// Setup some tokens to allow us to identify which event is
// for which socket.
use get_if_addrs::{get_if_addrs, Interface};
use ipc::Message;
use priority_queue::PriorityQueue;
use std::collections::VecDeque;
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::SystemTime;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use ubyte::{ByteUnit, ToByteUnit};

use super::config::ExecConfig;
use super::history::{load_current_history, write_history_file, History};
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
    send_chan: mpsc::Sender<Box<ipc::Message>>,
    // Paired remote ID. For client type, this is the executor ID which is running the task, or None if
    // not assigned yet. For executors, it is the ID of the client which submitted the task, or None.
    paired_id: Option<u32>,
}

struct ServerState {
    /// Connections to remote processes, both clients (providing a task) and execs (processing N tasks).
    /// Array position corresponds to the ID of the client/exec.
    remotes: Vec<Remote>,
    /// Queue of tasks submitted and the ID of the client which submitted it, not yet assigned to an exec
    task_queue: PriorityQueue<(usize, ipc::TaskDetails), Duration>,
    /// Queue of exec IDs waiting for tasks
    exec_queue: VecDeque<usize>,
    /// Total tasks submitted
    submit_count: usize,
    /// Total tasks completed
    finish_count: usize,
    /// Number of exec processes started but which have not yet phoned home
    pending_exec_count: usize,
    /// Number of exec processes started in alt queue but which have not yet phoned home
    second_pending_exec_count: usize,
    /// Number of currently running exec processes
    running_exec_count: usize,
    /// Timestamp when the most recent task was received, or none if no task yet received
    last_submit_time: Option<SystemTime>,
    /// Timestamp when last activity occured
    last_activity_time: SystemTime,
    /// User configuration parameters
    user_config: ExecConfig,
    /// User-specific key to avoid party crashers
    user_private_key: u64,
    /// Exec process start command
    start_cmd: String,
    /// Second start command, if in use
    second_start_cmd: Option<String>,
    /// Verbose reporting
    verbose: bool,

    /// Record of prior runs
    prior_history: History,
    /// Log of only the latest results
    in_the_making: History,

    // Metrics
    /// Total bytes of files (TaskOutput) messages returned
    total_bytes: ByteUnit,
    /// Bytes returned during this sampling period
    bytes_this_period: ByteUnit,
    /// Total task runtime
    total_run_time: std::time::Duration,
    /// Total time to send results back
    total_send_time: std::time::Duration,
    /// Maximum send time
    peak_send_time: std::time::Duration,
}

impl ServerState {
    fn new(port: ipc::CallMe, user_private_key: u64, max_cpus: Option<usize>, alt_start_cmd: bool, both_queues: bool, verbose: bool) -> ServerState {
        let mut config = config::load_config_file();

        if let Some(c) = max_cpus {
            config.max_count = c;
        }

        let cmd: &str = if alt_start_cmd && config.alt_start_cmd.is_some() {
            config.alt_start_cmd.as_ref().unwrap()
        } else {
            &config.start_cmd
        };
        assert!(port.access_code & 0x01 == 0, "Expected bit-0 of access code to be 0");
        let start_cmd = format!("{} exec --callme={} --code={}", &cmd, port.addr, port.access_code);
        println!("SprayCC: Start command: {}", &start_cmd);

        let second_start_cmd = if both_queues {
            if let Some(cmd) = config.alt_start_cmd.as_ref() {
                // access_code + 1 because last bit indicates secondary queue
                let sc = format!("{} exec --callme={} --code={}", &cmd, port.addr, port.access_code + 1);
                println!("SprayCC: Second command: {}", &sc);
                Some(sc)
            } else {
                println!("--both ignored: alt_start_cmd provided in .spraycc file");
                None
            }
        } else {
            None
        };

        ServerState {
            remotes: vec![],
            task_queue: PriorityQueue::new(),
            exec_queue: VecDeque::new(),
            submit_count: 0,
            finish_count: 0,
            pending_exec_count: 0,
            second_pending_exec_count: 0,
            running_exec_count: 0,
            last_submit_time: None,
            last_activity_time: SystemTime::now(),
            user_config: config,
            user_private_key,
            start_cmd,
            second_start_cmd,
            verbose,
            prior_history: load_current_history(),
            in_the_making: History::default(),
            total_bytes: 0.bytes(),
            bytes_this_period: 0.bytes(),
            total_run_time: Duration::from_secs(0),
            total_send_time: Duration::from_secs(0),
            peak_send_time: Duration::from_secs(0),
        }
    }

    /// Resets the inactivity timer
    fn activity_occurred(self: &mut ServerState) {
        self.last_activity_time = SystemTime::now();
    }

    /// True if no tasks are queued and no activity has occurred within the shutdown period
    fn ok_to_shutdown(self: &ServerState) -> bool {
        return self.task_queue.is_empty()
            && self.running_exec_count == 0
            && SystemTime::now()
                .duration_since(self.last_activity_time)
                .unwrap_or(ZERO_DURATION)
                .as_secs()
                > self.user_config.idle_shutdown_after;
    }

    /// Writes any recorded history prior to shutdown
    fn shutdown(self: &mut ServerState) {
        if !self.in_the_making.is_empty() {
            if let Err(err) = write_history_file(std::mem::take(&mut self.in_the_making)) {
                println!("SprayCC: Warning: error writing history: {}", &err);
            }
        }
    }

    /// Record the elapsed of successful tasks for prioritization next time
    fn record_elapsed(self: &mut ServerState, target_id: &str, elapsed: std::time::Duration) {
        self.in_the_making.update(target_id, elapsed);
    }

    /// Sends a message to the host paired with the given host. That is, given an exec host, send a message to the client
    /// which submitted the task or given a client and a message to the host executing the task.
    async fn send_to_paired_remote(self: &mut ServerState, conn_id: usize, msg: Box<ipc::Message>) -> Result<(), Box<dyn Error + Send + Sync>> {
        let other_id = self.remotes[conn_id].paired_id.expect("Expected connection to already be paired") as usize;

        // Forward to client, as long as it hasn't already been marked 'dead'
        if self.remotes[other_id].conn_type == ConnectionType::Client {
            if let Err(err) = self.remotes[other_id].send_chan.send(msg).await {
                println!("SprayCC: Error sending to remote (was ctrl-c hit?): {}", err);
                self.remotes[other_id].conn_type = ConnectionType::Dead;
            }
        }
        Ok(())
    }

    /// Adds a new remote which has connected to the server
    fn add_remote(self: &mut ServerState, tx: mpsc::Sender<Box<ipc::Message>>) -> usize {
        self.remotes.push(Remote {
            conn_type: ConnectionType::Pending,
            send_chan: tx,
            paired_id: None,
        });
        if self.verbose {
            println!("SprayCC: added pending remote in slot {}", self.remotes.len() - 1);
        }

        self.remotes.len() - 1
    }

    /// A connection was dropped, perhaps expectedly, perhaps not.
    async fn handle_dropped(self: &mut ServerState, conn_id: usize) -> Result<(), Box<dyn Error + Send + Sync>> {
        let remote = &self.remotes[conn_id];
        if remote.conn_type == ConnectionType::Exec {
            if let Some(other_id) = remote.paired_id {
                // Notify the client that something bad has happened...
                self.remotes[other_id as usize]
                    .send_chan
                    .send(Box::new(ipc::Message::TaskFailed {
                        error_message: String::from("exec dropped connection"),
                    }))
                    .await?;
            }
        }

        if remote.conn_type == ConnectionType::Exec {
            self.running_exec_count -= 1;
        }
        self.remotes[conn_id].conn_type = ConnectionType::Dead;
        if self.verbose {
            println!("SprayCC: connection {} dropped", conn_id);
        }
        Ok(())
    }

    /// A remote is identified after it sends its first message. In this case, the remote is a client and submitted
    /// a task. Queue processing is done as a part of this function, so the submitted task may be sent out immediately
    /// or queued for later processing.
    async fn remote_is_client(self: &mut ServerState, conn_id: usize, details: ipc::TaskDetails) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Priority tasks get a bump. 300 == over a 5-minute long task
        let priority = Duration::from_secs(if details.priority_task { 300 } else { 0 });

        let target_id = details.get_target_id();
        let prior = self.prior_history.get(&target_id).unwrap_or(Duration::from_secs(1)) + priority;
        if self.verbose {
            println!("SprayCC: received task {} priority={:?}", &target_id, prior);
        }

        self.remotes[conn_id].conn_type = ConnectionType::Client;
        self.task_queue.push((conn_id, details), prior);
        self.last_submit_time = Some(SystemTime::now());
        if self.verbose {
            println!("SprayCC: connection {} is a client", conn_id);
        }
        self.update().await
    }

    /// A remote is identified after it sends its first message. In this case, the remote is an executor and is now
    /// available to start processing tasks. If one is available, the task is sent to it.
    async fn remote_is_exec(self: &mut ServerState, conn_id: usize, second_queue: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.remotes[conn_id].conn_type = ConnectionType::Exec;
        self.running_exec_count += 1;
        if !second_queue && self.pending_exec_count > 0 {
            self.pending_exec_count -= 1;
        } else if second_queue && self.second_pending_exec_count > 0 {
            self.second_pending_exec_count -= 1;
        }

        if self.verbose {
            println!(
                "SprayCC: connection {} is an exec from {} queue",
                conn_id,
                if !second_queue { "primary" } else { "secondary" }
            );
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
        if self.verbose {
            println!("SprayCC: updating state");
        }
        self.start_execs().await;
        self.check_queues().await
    }

    /// Check the task and ready exec queues to see if anything can be assigned
    async fn check_queues(self: &mut ServerState) -> Result<(), Box<dyn Error + Send + Sync>> {
        while !self.task_queue.is_empty() && !self.exec_queue.is_empty() {
            let ((client_id, task), prior) = self.task_queue.pop().unwrap();
            let exec_id = self.exec_queue.pop_front().unwrap();
            assert!(client_id != exec_id, "Somehow exec and client ID are the same");

            if self.verbose {
                println!("SprayCC: assigned task {} to {}, priority={:?}", task.get_target_id(), exec_id, prior);
            }

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

            // Send the task
            exec.send_chan
                .send(Box::new(ipc::Message::Task {
                    access_code: 0,
                    user_code: 0,
                    details: task,
                }))
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
            exec.send_chan.send(Box::new(ipc::Message::PissOff {})).await?;
            self.running_exec_count -= 1;
        }
        Ok(())
    }

    fn report_status(self: &ServerState, reporting_period: u64) {
        let running = self.submit_count - self.finish_count - self.task_queue.len();

        // Avoid reporting while idle
        if running > 0 || self.running_exec_count > 0 || self.bytes_this_period > 0 {
            println!(
                "SprayCC: {} / {} finished, {} running, {} executors, {} / sec",
                self.finish_count,
                self.submit_count,
                running,
                self.running_exec_count,
                self.bytes_this_period / reporting_period
            );
        }
    }

    /// Check to see if we need to start more exec engines
    async fn start_execs(self: &mut ServerState) {
        if self.task_queue.is_empty() {
            return;
        }

        let init_needed = std::cmp::max(self.user_config.initial_count, self.running_exec_count) - self.running_exec_count;
        let keep_pending = std::cmp::max(self.user_config.keep_pending, init_needed);
        if self.verbose {
            println!(
                "SprayCC: start execs: queue={}, running={}, pending={},{} init_needed={}, keep_pending={}",
                self.task_queue.len(),
                self.running_exec_count,
                self.pending_exec_count,
                self.second_pending_exec_count,
                init_needed,
                keep_pending
            );
        }
        if self.second_start_cmd.is_some() {
            let half_pending = (keep_pending + 1) / 2;
            self.start_execs_in_queue(false, half_pending).await;
            self.start_execs_in_queue(true, half_pending).await;
        } else {
            self.start_execs_in_queue(false, keep_pending).await;
        }
    }

    async fn start_execs_in_queue(self: &mut ServerState, alt_queue: bool, keep_pending: usize) {
        let (cmd, pending, other_pending) = if !alt_queue {
            (&self.start_cmd, &mut self.pending_exec_count, self.second_pending_exec_count)
        } else {
            (
                self.second_start_cmd.as_ref().unwrap(),
                &mut self.second_pending_exec_count,
                self.pending_exec_count,
            )
        };

        while self.running_exec_count + *pending + other_pending < self.task_queue.len()
            && self.running_exec_count + *pending + other_pending < self.user_config.max_count
            && *pending < keep_pending
        {
            match Command::new("sh").arg("-c").arg(&cmd).spawn() {
                Ok(_) => {
                    *pending += 1;
                    // Tokio does not do kill-on-drop by default, so we let the child run and hopefully finish quickly
                    if self.verbose {
                        println!("Submitted job to {} queue", if !alt_queue { "primary" } else { "secondary" });
                    }
                }
                Err(err) => {
                    println!("SprayCC: Error starting exec process '{}': {}", &self.start_cmd, err);
                }
            }
        }
    }
}

/// Main server loop. This starts a socket listener to wait for remote connections. Each connection
/// is spun off as a separate task, which relays backs back over an MPSC.
pub async fn run(max_cpus: Option<usize>, alt_start_cmd: bool, both_queues: bool, verbose: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Try to make sure we have enough file descriptors to handle all of the incoming clients + executors
    config::setup_process_file_limit(false);

    // Load the user's private key and create if needed. Creation isn't ideal because there is a race-condition where
    // the executors may not see it, but mostly it is ok and keeps the users jobs from failing.
    let user_key = config::load_user_private_key(true /* create if not yet created */);

    let ip_addr = get_network_addr();
    // Toml only allows i64, LSb used to indicate which pool remote started from
    let access_code = (rand::random::<u64>() >> 1) & !1u64;
    let mut callme = ipc::CallMe {
        addr: SocketAddr::new(ip_addr, 0),
        access_code,
    };

    let listener = TcpListener::bind(&callme.addr).await?;
    callme.addr = listener.local_addr()?;
    println!("Spraycc: Server: {}", &callme.addr);

    let _cleaner = config::write_server_contact_info(&callme)?;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<(usize, Box<ipc::Message>)>(256);

    // Make sure we check on things even if no clients are communicating
    let watchdog = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(watchdog);

    // Server state includes the user configuration from .spraycc, plus overrides
    let mut server_state = ServerState::new(callme, user_key, max_cpus, alt_start_cmd, both_queues, verbose);
    let user_private_key = server_state.user_private_key;
    loop {
        tokio::select! {
            socket_res = listener.accept() => {
                let (socket, _) = socket_res.expect("Error accepting incoming connection");
                // A new task is spawned for each inbound socket. The socket is
                // moved to the new task and processed there.
                let (outbound_tx, outbound_rx) = mpsc::channel::<Box<ipc::Message>>(256);
                let remote_id = server_state.add_remote(outbound_tx);
                let in_tx = inbound_tx.clone();
                tokio::spawn(async move {
                    match handle_connection(socket, access_code, user_private_key, remote_id, in_tx, outbound_rx).await {
                        Ok(_) => {
                            // pass
                        }
                        Err(err) => {
                            println!("SprayCC: Connection {}: error on connection: {}", remote_id, err);
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
                server_state.update().await?;
                server_state.report_status(5);
                server_state.bytes_this_period = ByteUnit::Byte(0);
                watchdog.as_mut().reset(Instant::now() + Duration::from_secs(5));
            }
        }

        if server_state.ok_to_shutdown() {
            break;
        }
    }

    server_state.shutdown();

    let stats = simple_process_stats::ProcessStats::get().await?;
    println!(
        "SprayCC: shutdown, {} CPU seconds, {} generated",
        stats.cpu_time_user.as_secs(),
        server_state.total_bytes
    );
    println!(
        "SprayCC: task stats: total run time: {:.2}s, total send time: {:.2}s, peak send time: {:.2}s",
        server_state.total_run_time.as_secs_f64(),
        server_state.total_send_time.as_secs_f64(),
        server_state.peak_send_time.as_secs_f64()
    );
    Ok(())
}

/// Task for handling connections by relating messages between the internal channels and the socket.
async fn handle_connection(
    stream: TcpStream,
    server_access_code: u64,
    user_private_key: u64,
    conn_id: usize,
    tx_chan: mpsc::Sender<(usize, Box<ipc::Message>)>,
    mut rx_chan: mpsc::Receiver<Box<ipc::Message>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut conn = ipc::Connection::new(stream);
    loop {
        tokio::select! {
            res = conn.read_message() => {
                match res {
                    Ok(Some(msg)) => {
                        let drop = match &msg {
                            Message::YourObedientServant { access_code, user_code }  => *access_code & !1u64 != server_access_code || *user_code != user_private_key,
                            Message::Task { access_code, user_code, details : _details } => *access_code != server_access_code || *user_code != user_private_key,
                            _ => false,
                        };
                        tx_chan.send((conn_id, if drop { Box::new(Message::Dropped)} else { Box::new(msg)} )).await?
                    }
                    Ok(None) => {
                        break;
                    }
                    Err(err) => {
                        println!("SprayCC: Error reading from socket {}: {}", conn_id, err);
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
    tx_chan.send((conn_id, Box::new(ipc::Message::Dropped))).await?;
    Ok(())
}

/// On the main server thread, handle a message from either a client or exec remote. Often this involves
/// forwarding a message to another remote.
async fn handle_msg(server_state: &mut ServerState, conn_id: usize, msg: Box<ipc::Message>) -> Result<(), Box<dyn Error + Send + Sync>> {
    match *msg {
        ipc::Message::YourObedientServant {
            access_code,
            user_code: _user_code,
        } => {
            // If the last bit of the access code is '1' it means the exec is from the second queue
            server_state.remote_is_exec(conn_id, access_code & 0x01 != 0).await?;
        }
        ipc::Message::Task {
            access_code: _access_code,
            user_code: _user_code,
            details,
        } => {
            server_state.submit_count += 1;
            server_state.remote_is_client(conn_id, details).await?;
        }
        ipc::Message::TaskOutput { output_type, content } => {
            let size = content.len().bytes();
            // TODO: Simplify once ubyte crate PR merged
            // server_state.total_bytes += size;
            // server_state.bytes_this_period += size;
            server_state.total_bytes = server_state.total_bytes + size;
            server_state.bytes_this_period = server_state.bytes_this_period + size;
            server_state
                .send_to_paired_remote(conn_id, Box::new(ipc::Message::TaskOutput { output_type, content }))
                .await?;
        }
        ipc::Message::TaskDone {
            exit_code,
            target_id,
            run_time,
            send_time,
        } => {
            if exit_code == Some(0) {
                server_state.record_elapsed(&target_id, run_time);
            }
            server_state
                .send_to_paired_remote(
                    conn_id,
                    Box::new(ipc::Message::TaskDone {
                        exit_code,
                        target_id,
                        run_time,
                        send_time,
                    }),
                )
                .await?;
            server_state.finish_count += 1;
            server_state.exec_is_ready(conn_id).await?;

            // Stats for tracking runtime vs. time to send back results. Does it make sense to pipeline the execs
            // such that a new task starts while results are being sent back? Or doesn't matter?
            // TODO: record runtime + send time w/ task name so queue can be prioritized
            server_state.total_run_time += run_time;
            server_state.total_send_time += send_time;
            server_state.peak_send_time = std::cmp::max(server_state.peak_send_time, send_time);
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
                println!("SprayCC: No network interfaces found, falling back to loopback");
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
            } else {
                if non_loops.len() > 1 {
                    println!(
                        "SprayCC: Warning: multiple network interace found, using {} ({})",
                        non_loops[0].name,
                        non_loops[0].ip()
                    );
                }
                non_loops[0].ip()
            }
        }
        Err(err) => {
            println!("SprayCC: Warning: unable to get network interfaces, defaulting to loopback: {}", err);
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        }
    }
}
