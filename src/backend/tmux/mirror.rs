//! The tmux side of the bus: a subscriber that writes this server's records
//! into its panes' `@tracon_*` options, for tmux formats and scripts. tracon
//! never reads the options back. Setup starts one mirror per server as a
//! tmux job, so it dies with the server; a second one for the same server
//! finds the lock held and leaves.

use std::fs::{File, TryLockError};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::sleep;

use crate::Error;
use crate::client::Client;
use crate::protocol::{Message, Stored};
use crate::runtime::Runtime;
use crate::state::Record;
use crate::tmux::{self, CHANNEL, SEPARATOR, Tmux};

pub const STATE: &str = "@tracon_state";
pub const SINCE: &str = "@tracon_since";
pub const MESSAGE: &str = "@tracon_message";
pub const HARNESS: &str = "@tracon_harness";
pub const SESSION: &str = "@tracon_session";
pub const AGENT: &str = "@tracon_agent";
pub const PINNED: &str = "@tracon_pinned";
pub const VIEWED: &str = "@tracon_viewed";
/// The running mirror's process id, in the server's global options.
pub const PID_OPTION: &str = "@tracon-mirror-pid";

/// How long to wait between attempts to reach the daemon.
const RETRY: Duration = Duration::from_millis(250);
/// How often an idle mirror checks that its server still runs.
const LIVENESS: Duration = Duration::from_secs(5);

/// The options as (name, value) pairs, a `None` value unsetting the option.
fn options(record: Option<&Record>) -> Vec<(&'static str, Option<String>)> {
    let record = record.cloned().unwrap_or_default();
    let text = |value: Option<String>| value.filter(|value| !value.is_empty());
    let number = |value: Option<u64>| value.map(|value| value.to_string());
    vec![
        (STATE, record.state.map(|state| state.to_string())),
        (SINCE, number(record.since)),
        (MESSAGE, text(record.message)),
        (HARNESS, text(record.harness)),
        (SESSION, text(record.session)),
        (AGENT, text(record.agent)),
        (PINNED, record.pinned.then(|| "1".to_owned())),
        (VIEWED, number(record.viewed)),
    ]
}

/// `value` as one tmux argument. An argument ending in `;` would end the
/// command; a backslash before that semicolon keeps it in the value.
fn argument(value: &str) -> String {
    match value.strip_suffix(';') {
        Some(head) => format!("{head}\\;"),
        None => value.to_owned(),
    }
}

/// Append the `set-option` commands that write `record` to `pane`, or
/// clear its options for `None`, each followed by a separator.
fn writes(pane: &str, record: Option<&Record>, args: &mut Vec<String>) {
    for (option, value) in options(record) {
        match value {
            Some(value) => args.extend(
                ["set-option", "-p", "-t", pane, option, &argument(&value)].map(str::to_owned),
            ),
            None => args.extend(["set-option", "-pu", "-t", pane, option].map(str::to_owned)),
        }
        args.push(";".to_owned());
    }
}

/// The record for `pane` on `server` among `records`.
fn record_for<'a>(records: &'a [Stored], server: &str, pane: &str) -> Option<&'a Record> {
    records
        .iter()
        .find(|stored| stored.pane.id == pane && stored.pane.server.as_deref() == Some(server))
        .map(|stored| &stored.record)
}

/// Whether a tmux error means the server is gone, so the mirror is done.
fn server_gone(error: &Error) -> bool {
    matches!(error, Error::Tmux { stderr, .. }
        if stderr.contains("no server running") || stderr.contains("error connecting"))
}

async fn run_tmux(tmux: &Tmux, args: &[String]) -> Result<String, Error> {
    let output = tokio::process::Command::from(tmux.command())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| Error::Spawn {
            program: "tmux".to_owned(),
            source,
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(Error::Tmux {
            command: args.first().cloned().unwrap_or_default(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Run a batch of option writes, then signal the channel. A pane that died
/// between the change and the write is not an error.
async fn apply(tmux: &Tmux, mut args: Vec<String>) -> Result<(), Error> {
    args.extend(["wait-for", "-S", CHANNEL].map(str::to_owned));
    match run_tmux(tmux, &args).await {
        Ok(_) => Ok(()),
        Err(error) if server_gone(&error) => Err(error),
        Err(_) => Ok(()),
    }
}

/// Bring every pane's options in line with a snapshot: write each record
/// of this server, and clear the options of panes that have some but no
/// record.
async fn sync(tmux: &Tmux, server: &str, records: &[Stored]) -> Result<(), Error> {
    let format = format!("#{{pane_id}}{SEPARATOR}#{{{STATE}}}#{{{PINNED}}}#{{{VIEWED}}}");
    let listed = run_tmux(
        tmux,
        &["list-panes", "-a", "-F", &format].map(str::to_owned),
    )
    .await?;
    let mut args = Vec::new();
    for line in listed.lines() {
        let Some((pane, written)) = line.split_once(SEPARATOR) else {
            continue;
        };
        match record_for(records, server, pane) {
            Some(record) => writes(pane, Some(record), &mut args),
            None if !written.is_empty() => writes(pane, None, &mut args),
            None => {}
        }
    }
    if args.is_empty() {
        return Ok(());
    }
    apply(tmux, args).await
}

async fn write(tmux: &Tmux, pane: &str, record: Option<&Record>) -> Result<(), Error> {
    if !tmux::valid_pane_id(pane) {
        return Ok(());
    }
    let mut args = Vec::new();
    writes(pane, record, &mut args);
    apply(tmux, args).await
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The lock that keeps one mirror per server, or `None` when another
/// holds it.
fn lock(runtime: &Runtime, server: &str) -> Result<Option<File>, Error> {
    runtime.ensure()?;
    let path = runtime
        .dir()
        .join(format!("mirror-{:016x}.lock", fnv(server)));
    let file = File::create(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Tell the daemon about this server, so it lists the server's panes before
/// any hook from them arrives. A daemon that isn't running is left alone:
/// starting one is a client's job, and the mirror only follows.
async fn register(runtime: &Runtime) {
    let runtime = runtime.clone();
    let _ = tokio::task::spawn_blocking(move || {
        Client::connect_running(&runtime)
            .and_then(|mut client| client.hello())
            .map(drop)
    })
    .await;
}

/// Mirror `server`'s records until the server exits.
pub async fn run(runtime: &Runtime, server: &str) -> Result<(), Error> {
    let Some(_lock) = lock(runtime, server)? else {
        return Ok(());
    };
    let tmux = Tmux::at(server);
    let pid = std::process::id().to_string();
    run_tmux(
        &tmux,
        &["set-option", "-g", PID_OPTION, &pid].map(str::to_owned),
    )
    .await?;
    let result = follow(runtime, &tmux, server).await;
    if !result.as_ref().is_err_and(server_gone) {
        let _ = run_tmux(&tmux, &["set-option", "-gu", PID_OPTION].map(str::to_owned)).await;
    }
    result
}

async fn follow(runtime: &Runtime, tmux: &Tmux, server: &str) -> Result<(), Error> {
    loop {
        let Ok(stream) = UnixStream::connect(runtime.events_socket()).await else {
            if let Err(error) =
                run_tmux(tmux, &["display-message", "-p", ""].map(str::to_owned)).await
                && server_gone(&error)
            {
                return Ok(());
            }
            sleep(RETRY).await;
            continue;
        };
        register(runtime).await;
        let mut lines = BufReader::new(stream).lines();
        loop {
            let line = tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => line,
                    // The daemon stopped; wait for the next one.
                    _ => break,
                },
                _ = sleep(LIVENESS) => {
                    if let Err(error) = run_tmux(tmux, &["display-message", "-p", ""].map(str::to_owned)).await
                        && server_gone(&error)
                    {
                        return Ok(());
                    }
                    continue;
                }
            };
            let Ok(message) = serde_json::from_str::<Message>(&line) else {
                continue;
            };
            let result = match message {
                Message::Snapshot { records, .. } => sync(tmux, server, &records).await,
                Message::Change { pane, record, .. } if pane.server.as_deref() == Some(server) => {
                    write(tmux, &pane.id, Some(&record)).await
                }
                Message::Removed { pane, .. } if pane.server.as_deref() == Some(server) => {
                    write(tmux, &pane.id, None).await
                }
                _ => Ok(()),
            };
            match result {
                Ok(()) => {}
                Err(error) if server_gone(&error) => return Ok(()),
                Err(error) => eprintln!("tracon: mirror: {error}"),
            }
        }
        sleep(RETRY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::State;

    #[test]
    fn every_option_is_set_or_unset() {
        let record = Record {
            state: Some(State::known(State::DONE)),
            since: Some(7),
            message: Some("it's #{done};".into()),
            pinned: true,
            ..Record::default()
        };
        let mut args = Vec::new();
        writes("%1", Some(&record), &mut args);
        let args = args.join(" ");
        assert!(args.starts_with("set-option -p -t %1 @tracon_state done ;"));
        assert!(args.contains("set-option -p -t %1 @tracon_since 7 ;"));
        // Arguments reach tmux verbatim, except a trailing semicolon.
        assert!(args.contains("set-option -p -t %1 @tracon_message it's #{done}\\; ;"));
        assert!(args.contains("set-option -pu -t %1 @tracon_harness ;"));
        assert!(args.contains("set-option -p -t %1 @tracon_pinned 1 ;"));
        assert!(args.contains("set-option -pu -t %1 @tracon_viewed ;"));
        let mut cleared = Vec::new();
        writes("%1", None, &mut cleared);
        assert_eq!(cleared.iter().filter(|arg| *arg == "-pu").count(), 8);
        assert_eq!(argument(";"), "\\;");
        assert_eq!(argument("a;b"), "a;b");
    }

    #[test]
    fn server_errors_are_told_apart() {
        let gone = Error::Tmux {
            command: "set-option".into(),
            stderr: "no server running on /tmp/s".into(),
        };
        let pane = Error::Tmux {
            command: "set-option".into(),
            stderr: "no such pane: %1".into(),
        };
        assert!(server_gone(&gone));
        assert!(!server_gone(&pane));
        assert_ne!(fnv("/tmp/a"), fnv("/tmp/b"));
    }
}
