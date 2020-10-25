extern crate byteorder;
extern crate bytes;

use byteorder::{BigEndian, ByteOrder};
use bytes::{Buf, BytesMut};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Error returned by most functions.
///
/// When writing a real application, one might want to consider a specialized
/// error handling crate or defining an error type as an `enum` of causes.
/// However, for our example, using a boxed `std::error::Error` is sufficient.
///
/// For performance reasons, boxing is avoided in any hot path. For example, in
/// `parse`, a custom error `enum` is defined. This is because the error is hit
/// and handled during normal execution when a partial frame is received on a
/// socket. `std::error::Error` is implemented for `parse::Error` which allows
/// it to be converted to `Box<dyn std::error::Error>`.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// A specialized `Result` type for mini-redis operations.
///
/// This is defined as a convenience.
pub type Result<T> = std::result::Result<T, Error>;

/// The host and port to connect to and the access code to use
#[derive(Serialize, Deserialize, Debug)]
pub struct CallMe {
    pub addr: std::net::SocketAddr,
    pub access_code: u64,
}

//
// Catalog of messages used in the system
//

/// Type of output generated by a task, either standard IO or zero or more files written by the process
#[derive(PartialEq, Eq, Hash, Copy, Clone, Serialize, Deserialize, Debug)]
pub enum OutputType {
    Stdout,
    Stderr,
    File(usize),
}

/// Describes the task to be started on a remote host
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TaskDetails {
    /// Directory in which to run the command
    pub working_dir: PathBuf,
    /// Absolute path to the command to be executes
    pub cmd: PathBuf,
    /// Set of arguments to be passed to the command
    pub args: Vec<String>,
    /// Indices into 'args' which are output file names to be captured and relayed back to the originating host
    pub output_args: Vec<u16>,
}

/// Messages sent between Spraycc processes.
/// *C++*: This could be done as a trait (interface / base class) or as an enum (enum + struct). A trait
/// would be more common in open systems the set of messages needs to be extened without requiring a
/// recompile of the whole system (any module can add a new implementation of the interface). I went with
/// an enum here becaause it's a closed system with a small set of messages so it's nice to have the compiler
/// validate that every message type is handled. And it's way to experiment with Rust enum's.
#[derive(Serialize, Deserialize, Clone)]
pub enum Message {
    /// Executor is ready, sends access code
    YourObedientServant {
        access_code: u64,
    },

    /// Task defintion, from wrapper to server or server to executable. When sent to an executor the
    /// access code is ignored.
    Task {
        access_code: u64,
        details: TaskDetails,
    },

    /// Output from a task from exec to server, possibly to wrapper
    TaskOutput {
        output_type: OutputType,
        content: Vec<u8>,
    },

    // A task failed to even start
    TaskFailed {
        error_message: String,
    },

    // Task has finished, all output has been sent. When reeived by the wrapper, the wrapper
    // process should disconnect and exit. A 'None' for the exit code means it was terminated by a signal.
    TaskDone {
        exit_code: Option<i32>,
    },

    /// Indicates the server can assume no more tasks are coming (within some bounds of a race condition w/ timing)
    LastTaskSent,

    // A specific task should be canceled
    CancelTask,

    /// Executor can (must) terminate, killing any job running
    PissOff,

    /// A connection was dropped, sent from the connection to the local event loop
    Dropped,
}

/// Normally would be automatically derived, but I don't want to dump the buffer contents. Is there a way
/// to derive everything else without having to implement all of this?
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Message::YourObedientServant { access_code } => write!(f, "YourObedientServant: {}", access_code),
            Message::Task { access_code, details } => write!(f, "Task: code {}, details: {:?}", access_code, details),
            Message::TaskOutput { output_type, content } => match output_type {
                OutputType::Stderr => {
                    let x = String::from_utf8(content.clone()).unwrap();
                    write!(f, "TaskOutput {:?}, length {}: {}", output_type, x.len(), x)
                }
                _ => write!(f, "TaskOutput {:?}, length {}", output_type, content.len()),
            },
            Message::TaskFailed { error_message } => write!(f, "TaskDone, error: {}", error_message),
            Message::TaskDone { exit_code } => write!(f, "TaskDone, exit code {}", exit_code.unwrap_or(-1)),
            Message::LastTaskSent => write!(f, "LastTaskSend"),
            Message::CancelTask => write!(f, "CancelTask"),
            Message::PissOff => write!(f, "PissOff"),
            Message::Dropped => write!(f, "Dropped"),
        }
    }
}

pub struct Connection {
    stream: TcpStream,
    buf: BytesMut, // Buffer sizes based on 'size', may be incomplete
}

impl Connection {
    pub fn new(stream: TcpStream) -> Connection {
        Connection {
            stream,
            buf: BytesMut::with_capacity(65536 * 4096), // Max msg size + 1 page
        }
    }

    /// Returns the next message from the stream or None if the stream has closed. An error is returned
    /// if the stream returns EOF while reading a partial message.
    pub async fn read_message(&mut self) -> Result<Option<Message>> {
        // Each message starts with the message length. A disconnect with 0 bytes read is a normal
        // stream-close event. An EOF with bytes in the buffer is an unexpected reset.
        while self.buf.len() < 4 {
            if self.stream.read_buf(&mut self.buf).await? == 0 {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(format!("Connection reset by peer with {} bytes buffered", self.buf.len()).into())
                };
            }
        }
        let msg_size = self.buf.get_i32() as usize;

        // Keep reading until we have at least a full message
        while self.buf.len() < msg_size && self.stream.read_buf(&mut self.buf).await? > 0 {}

        // May get more than a full message, that is ok. A partial message indicates that the connection
        // was broken prematurely.
        if self.buf.len() >= msg_size {
            let msg = bincode::deserialize(self.buf.as_ref()).expect("Unable to decode message");
            self.buf.advance(msg_size);
            Ok(Some(msg))
        } else {
            Err(format!("connection reset by peer {} {}", self.buf.len(), msg_size).into())
        }
    }

    pub async fn write_message(&mut self, msg: &Message) -> Result<()> {
        let data: Vec<u8> = bincode::serialize(&msg).unwrap();
        let mut sz = [0; 4];

        BigEndian::write_u32(&mut sz, data.len() as u32);
        self.stream.write_all(&sz).await?;
        self.stream.write_all(&data).await?;
        Ok(())
    }
}

// #[cfg(test)]
// use mio::net::{TcpListener, TcpStream};
// #[cfg(test)]
// use mio::*;
// #[cfg(test)]
// use std::net::Shutdown;

// /// Test sending / receiving messages
// #[test]
// fn test_msgs() {
//     fn vec_of_size(sz: usize) -> Vec<u8> {
//         let mut v: Vec<u8> = Vec::new();
//         v.resize(sz, 0);
//         v
//     }

//     const LISTENER: Token = Token(0);
//     const SERVER: Token = Token(1);
//     const CLIENT: Token = Token(2);

//     let addr = "127.0.0.1:0".parse().unwrap();

//     // Setup the server socket
//     let mut server = TcpListener::bind(addr).unwrap();
//     println!("Server listening on {:?}", server.local_addr());

//     // Create a poll instance
//     let mut poll = Poll::new().unwrap();

//     // Start listening for incoming connections
//     poll.registry().register(&mut server, LISTENER, Interest::READABLE).unwrap();

//     // Setup the client socket
//     let mut client_sock = TcpStream::connect(server.local_addr().unwrap()).unwrap();
//     println!("Connection ready");

//     // Register the socket
//     poll.registry().register(&mut client_sock, CLIENT, Interest::WRITABLE).unwrap();

//     // Create storage for events
//     let mut events = Events::with_capacity(32);
//     let mut server_sock: Option<TcpStream> = None;

//     let mut client_state = 0;
//     let mut msgs_recvd = 0;
//     let mut read_state = IncrementalBuffer::new_incoming();

//     loop {
//         println!("Starting loop");
//         poll.poll(&mut events, None).unwrap();

//         for event in events.iter() {
//             match event.token() {
//                 LISTENER => {
//                     let mut c = server.accept().unwrap().0;
//                     poll.registry().register(&mut c, SERVER, Interest::READABLE).unwrap();
//                     server_sock = Some(c);
//                     println!("Accepted connection");
//                 }
//                 SERVER => {
//                     if event.is_readable() {
//                         println!("Server is readable");
//                         if server_sock.is_some() {
//                             let msgs = read_message_stream(server_sock.as_mut().unwrap(), &mut read_state).unwrap();
//                             println!(
//                                 "Read {} messages, buffer has {} of {:?} bytes",
//                                 msgs.len(),
//                                 read_state.read,
//                                 read_state.size
//                             );
//                             for msg in msgs {
//                                 println!("Got message: {:?}", msg);
//                                 msgs_recvd += 1;
//                                 match msg {
//                                     Message::PissOff => {
//                                         if let Some(ss) = server_sock.as_mut() {
//                                             println!("We can leave now");
//                                             poll.registry().deregister(ss).unwrap();
//                                             ss.shutdown(Shutdown::Both).unwrap();
//                                         }
//                                         server_sock = None;
//                                     }
//                                     _ => {}
//                                 }
//                             }
//                         }
//                     }
//                 }
//                 CLIENT => {
//                     if event.is_read_closed() || event.is_write_closed() || event.is_error() {
//                         println!("Got error on client");
//                         poll.registry().deregister(&mut client_sock).unwrap();
//                         break;
//                     } else {
//                         if event.is_writable() {
//                             match client_state {
//                                 0 => {
//                                     // A basic message
//                                     send_magic(&mut client_sock).unwrap();
//                                     send_message(&mut client_sock, &Message::YourObedientServant { access_code: 666 }).unwrap();
//                                 }
//                                 1 => {
//                                     // A large message, greater than one package
//                                     send_message(
//                                         &mut client_sock,
//                                         &Message::TaskOutput {
//                                             output_type: OutputType::File(1),
//                                             content: vec_of_size(8192 * 16),
//                                         },
//                                     )
//                                     .unwrap();
//                                 }
//                                 2 => {
//                                     // A bunch 'o messages in a stream
//                                     send_message(&mut client_sock, &Message::TaskDone { exit_code: Some(0) }).unwrap();
//                                     send_message(&mut client_sock, &Message::TaskDone { exit_code: Some(1) }).unwrap();
//                                     send_message(
//                                         &mut client_sock,
//                                         &Message::TaskOutput {
//                                             output_type: OutputType::File(1),
//                                             content: vec_of_size(17),
//                                         },
//                                     )
//                                     .unwrap();
//                                     send_message(&mut client_sock, &Message::TaskDone { exit_code: Some(2) }).unwrap();
//                                     send_message(&mut client_sock, &Message::CancelTask {}).unwrap();
//                                     send_message(&mut client_sock, &Message::PissOff {}).unwrap();
//                                 }
//                                 _ => {}
//                             }
//                             client_sock.flush().unwrap();
//                             println!("Completed client state {}", client_state);
//                             client_state += 1;
//                             poll.registry().reregister(&mut client_sock, CLIENT, Interest::WRITABLE).unwrap();
//                         }
//                     }
//                 }
//                 _ => unreachable!(),
//             }
//         }
//         if server_sock.is_none() {
//             break;
//         }
//     }
//     assert_eq!(msgs_recvd, 8);
// }
