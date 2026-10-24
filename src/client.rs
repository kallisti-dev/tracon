//! A client of the daemon: one request and response at a time on the
//! request socket, and a reader of the event socket.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Error;
use crate::protocol::{Request, Response};
use crate::runtime::{DIR_VAR, Runtime};

/// How long a freshly started daemon gets to open its sockets.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one request may take; a list captures every pane's screen.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Client {
    /// Connect, starting a daemon first when none answers.
    pub fn connect(runtime: &Runtime) -> Result<Self, Error> {
        match Self::connect_running(runtime) {
            Err(Error::Daemon { .. }) => {}
            result => return result,
        }
        start(runtime)?;
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match Self::connect_running(runtime) {
                Err(Error::Daemon { .. }) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(25));
                }
                result => return result,
            }
        }
    }

    /// Connect to a daemon that is already running.
    pub fn connect_running(runtime: &Runtime) -> Result<Self, Error> {
        let path = runtime.rpc_socket();
        let stream = UnixStream::connect(&path).map_err(|source| Error::Daemon {
            path: path.display().to_string(),
            source,
        })?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        Ok(Client {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
            next_id: 1,
        })
    }

    pub fn call<R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, Error> {
        let request = Request::new(self.next_id, method, params);
        self.next_id += 1;
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            return Err(Error::Daemon {
                path: "the request socket".to_owned(),
                source: std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the daemon hung up",
                ),
            });
        }
        let response: Response = serde_json::from_str(&reply)?;
        if let Some(error) = response.error {
            return Err(Error::Rpc(error));
        }
        Ok(serde_json::from_value(response.result.unwrap_or_default())?)
    }

    /// The event socket, one message per line, starting with the greeting
    /// and a snapshot.
    pub fn subscribe(runtime: &Runtime) -> Result<BufReader<UnixStream>, Error> {
        let path = runtime.events_socket();
        let stream = UnixStream::connect(&path).map_err(|source| Error::Daemon {
            path: path.display().to_string(),
            source,
        })?;
        Ok(BufReader::new(stream))
    }
}

/// Start a daemon for `runtime` in its own process group, so it outlives
/// this process and whatever kills this process's group. Its standard
/// error goes to the runtime directory's log.
fn start(runtime: &Runtime) -> Result<(), Error> {
    let program = std::env::current_exe()?;
    runtime.ensure()?;
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .open(runtime.log())?;
    Command::new(&program)
        .arg("daemon")
        .env(DIR_VAR, runtime.dir())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|source| Error::Spawn {
            program: program.display().to_string(),
            source,
        })?;
    Ok(())
}
