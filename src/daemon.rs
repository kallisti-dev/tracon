//! The core as a long-lived process: the records, the backends that find and
//! act on panes, the journal, and the two sockets clients use.
//!
//! One lock covers the records and the backends, and every request runs
//! under it on the blocking pool, since backends run tmux synchronously. A
//! change is applied, journaled and published before the next request is
//! handled. A subscriber gets a snapshot taken under the same lock, then
//! every change through a broadcast channel.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

use crate::backend::{Backend, Env, Locator, PaneInfo, TMUX, TmuxBackend};
use crate::harness;
use crate::journal::Journal;
use crate::list::{self, Entry};
use crate::protocol::{self, Message, Request, Response, RpcError, Stored};
use crate::runtime::Runtime;
use crate::state::Record;
use crate::store::{Change, Cond, Field, Guarded, MemoryStore, Update};
use crate::tmux::{self, Tmux};
use crate::{Config, Error, State};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a write to a subscriber may take before it is dropped.
const SUBSCRIBER_TIMEOUT: Duration = Duration::from_secs(1);
/// Changes a slow subscriber may fall behind by before it is dropped; it
/// reconnects for a fresh snapshot.
const SUBSCRIBER_BACKLOG: usize = 1024;

/// A finished pane becomes idle once seen; any other state is left alone.
pub fn seen() -> Guarded {
    let mut update = Update::new(State::known(State::IDLE));
    update.agent = Field::Clear;
    Guarded::when(
        Change::Set(update),
        Cond::StateUnder(State::known(State::DONE)),
    )
}

/// The backends the daemon talks to: one per tmux server it has heard of,
/// from a client's environment or from the journal.
#[derive(Default)]
struct Registry {
    tmux: BTreeMap<String, TmuxBackend>,
}

impl Registry {
    fn tmux(&mut self, server: &str) -> &TmuxBackend {
        self.tmux
            .entry(server.to_owned())
            .or_insert_with(|| TmuxBackend::new(Tmux::at(server)))
    }

    fn server_in(env: &Env) -> Option<String> {
        env.get("TMUX")
            .and_then(|value| tmux::socket_in(value))
            .map(|socket| socket.to_string_lossy().into_owned())
    }

    /// Note the server `env` names, and return the pane the client runs in.
    fn claim(&mut self, env: &Env) -> Option<Locator> {
        if let Some(server) = Self::server_in(env) {
            self.tmux(&server);
        }
        TmuxBackend::claim(env)
    }

    /// The backend for the client's own server, or the only one known.
    fn own(&mut self, env: &Env) -> Option<&TmuxBackend> {
        match Self::server_in(env) {
            Some(server) => Some(self.tmux(&server)),
            None if self.tmux.len() == 1 => self.tmux.values().next(),
            None => None,
        }
    }

    fn backend(&mut self, pane: &Locator) -> Result<&TmuxBackend, RpcError> {
        if pane.backend != TMUX {
            return Err(RpcError::new(
                RpcError::FAILED,
                format!("unsupported backend {:?}", pane.backend),
            ));
        }
        match &pane.server {
            Some(server) => Ok(self.tmux(server)),
            None => Err(RpcError::new(
                RpcError::UNCLAIMED,
                format!("{pane} names no server"),
            )),
        }
    }

    /// A locator from `text`, on the client's own server when it names none.
    fn resolve(&mut self, text: &str, env: &Env) -> Result<Locator, RpcError> {
        let locator: Locator = text
            .parse()
            .map_err(|error: Error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))?;
        if locator.server.is_some() {
            return Ok(locator);
        }
        let server = self
            .own(env)
            .and_then(|backend| backend.server().map(str::to_owned));
        match server {
            Some(server) => Ok(locator.on(Some(&server))),
            None => Err(RpcError::new(
                RpcError::UNCLAIMED,
                format!("{locator} needs a server, as in {locator}@/path/to/socket"),
            )),
        }
    }

    /// Every pane on every server. A server that no longer answers is
    /// forgotten, with its panes.
    fn panes(&mut self) -> Vec<PaneInfo> {
        let mut panes = Vec::new();
        self.tmux.retain(|server, backend| match backend.panes() {
            Ok(found) => {
                panes.extend(found);
                true
            }
            Err(error) => {
                eprintln!("tracon: forgetting tmux server {server}: {error}");
                false
            }
        });
        panes
    }

    fn text(&self, pane: &Locator) -> Option<String> {
        let server = pane.server.as_deref()?;
        self.tmux.get(server)?.text(pane).ok()
    }
}

struct Inner {
    store: MemoryStore,
    journal: Journal,
    backends: Registry,
    /// The number of the last change published.
    seq: u64,
}

/// A `stop` request, answered once the daemon has let go of its directory,
/// so that the client can start the next daemon at once.
struct Stopping {
    reply: OwnedWriteHalf,
    line: String,
}

pub struct Daemon {
    inner: Mutex<Inner>,
    events: broadcast::Sender<Message>,
    stop: mpsc::UnboundedSender<Stopping>,
}

/// This executable, resolved through symlinks, or empty once it is gone.
fn binary() -> String {
    std::env::current_exe()
        .and_then(fs::canonicalize)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Whether an error only says the peer hung up, which is how every
/// connection ends.
fn hung_up(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
    )
}

fn failed(error: Error) -> RpcError {
    let code = match error {
        Error::PaneGone(_) => RpcError::PANE_GONE,
        _ => RpcError::FAILED,
    };
    RpcError::new(code, error.to_string())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Daemon {
    /// Apply `change` to `pane`, and publish what it did.
    fn apply(&self, inner: &mut Inner, pane: &Locator, change: &Guarded, now: u64) {
        let outcome = inner
            .store
            .apply(pane, change, now)
            .map(|record| record.cloned());
        if let Some(record) = outcome {
            self.changed(inner, pane, record.as_ref());
        }
    }

    /// Record that a client views `pane` now, and mark a finished turn seen.
    fn view(&self, inner: &mut Inner, pane: &Locator, now: u64) {
        let outcome = inner
            .store
            .set_viewed(pane, now)
            .map(|record| record.cloned());
        if let Some(record) = outcome {
            self.changed(inner, pane, record.as_ref());
        }
        self.apply(inner, pane, &seen(), now);
    }

    /// Journal and publish a changed record, or its removal.
    fn changed(&self, inner: &mut Inner, pane: &Locator, record: Option<&Record>) {
        inner.seq += 1;
        let seq = inner.seq;
        if let Err(error) = inner.journal.write(pane, record) {
            eprintln!("tracon: journal: {error}");
        }
        let message = match record {
            Some(record) => Message::Change {
                seq,
                pane: pane.clone(),
                record: record.clone(),
            },
            None => Message::Removed {
                seq,
                pane: pane.clone(),
            },
        };
        // Nobody listening is not an error.
        let _ = self.events.send(message);
    }

    /// Forget records of panes that no longer exist; `panes` is every live
    /// pane on every known server.
    fn reconcile(&self, inner: &mut Inner, panes: &[PaneInfo]) {
        let live: HashSet<&Locator> = panes.iter().map(|info| &info.pane).collect();
        let dead: Vec<Locator> = inner
            .store
            .records()
            .keys()
            .filter(|pane| !live.contains(pane))
            .cloned()
            .collect();
        for pane in dead {
            inner.store.remove(&pane);
            self.changed(inner, &pane, None);
        }
    }

    fn config(inner: &mut Inner, env: &Env) -> Result<Config, RpcError> {
        match inner.backends.own(env) {
            Some(backend) => Config::from_tmux(backend.tmux()).map_err(failed),
            None => Ok(Config::default()),
        }
    }

    fn entries(&self, inner: &mut Inner, env: &Env) -> Result<Vec<Entry>, RpcError> {
        let config = Self::config(inner, env)?;
        let panes = inner.backends.panes();
        self.reconcile(inner, &panes);
        let records: Vec<_> = inner
            .store
            .records()
            .iter()
            .map(|(pane, record)| (pane.clone(), record.clone()))
            .collect();
        let backends = &inner.backends;
        Ok(list::entries(
            panes,
            records,
            &harness::all(),
            &config,
            |pane| backends.text(pane),
        ))
    }

    fn hello(&self, params: protocol::Hello) -> protocol::HelloResult {
        lock(&self.inner).backends.claim(&params.env);
        protocol::HelloResult {
            version: VERSION.to_owned(),
            protocol: protocol::PROTOCOL,
            binary: binary(),
        }
    }

    fn event(&self, params: protocol::Event) -> Result<protocol::EventResult, RpcError> {
        let mut inner = lock(&self.inner);
        let Some(pane) = inner.backends.claim(&params.env) else {
            return Ok(protocol::EventResult { pane: None });
        };
        let harness = harness::by_name(&params.harness).map_err(failed)?;
        if let Some(change) = harness.interpret(&params.payload).map_err(failed)? {
            let now = crate::now();
            self.apply(&mut inner, &pane, &change, now);
            // The focus hook only fires on a change of focus, so a turn that
            // ends in view is seen here.
            let finishes = matches!(&change.change, Change::Set(update)
                if update.state.is_under(&State::known(State::DONE)));
            if finishes
                && inner
                    .backends
                    .backend(&pane)?
                    .in_view(&pane)
                    .unwrap_or(false)
            {
                self.view(&mut inner, &pane, now);
            }
        }
        Ok(protocol::EventResult { pane: Some(pane) })
    }

    fn list(&self, params: protocol::List) -> Result<Vec<Entry>, RpcError> {
        let mut inner = lock(&self.inner);
        self.entries(&mut inner, &params.env)
    }

    fn focus(&self, params: protocol::Focus) -> Result<(), RpcError> {
        let mut inner = lock(&self.inner);
        let pane = inner.backends.resolve(&params.pane, &params.env)?;
        inner
            .backends
            .backend(&pane)?
            .focus(&pane, params.client.as_deref())
            .map_err(failed)
    }

    fn next(&self, params: protocol::Next) -> Result<protocol::NextResult, RpcError> {
        let mut inner = lock(&self.inner);
        let current = match inner.backends.own(&params.env) {
            Some(backend) => backend.current(params.client.as_deref()).map_err(failed)?,
            None => None,
        };
        let entries = self.entries(&mut inner, &params.env)?;
        let Some(entry) = entries
            .into_iter()
            .find(|entry| Some(&entry.pane) != current.as_ref())
        else {
            return Ok(protocol::NextResult { focused: None });
        };
        inner
            .backends
            .backend(&entry.pane)?
            .focus(&entry.pane, params.client.as_deref())
            .map_err(failed)?;
        Ok(protocol::NextResult {
            focused: Some(entry.pane),
        })
    }

    /// The pane a command names, or the client's own.
    fn target(inner: &mut Inner, pane: Option<&str>, env: &Env) -> Result<Locator, RpcError> {
        match pane {
            Some(pane) => inner.backends.resolve(pane, env),
            None => inner.backends.claim(env).ok_or_else(|| {
                RpcError::new(RpcError::UNCLAIMED, "name a pane, or run inside one")
            }),
        }
    }

    fn pin(&self, params: protocol::Pin) -> Result<(), RpcError> {
        let mut inner = lock(&self.inner);
        let pane = Self::target(&mut inner, params.pane.as_deref(), &params.env)?;
        let outcome = inner
            .store
            .set_pinned(&pane, params.pinned)
            .map(|record| record.cloned());
        if let Some(record) = outcome {
            self.changed(&mut inner, &pane, record.as_ref());
        }
        Ok(())
    }

    fn seen(&self, params: protocol::Seen) -> Result<(), RpcError> {
        let mut inner = lock(&self.inner);
        let pane = Self::target(&mut inner, params.pane.as_deref(), &params.env)?;
        self.view(&mut inner, &pane, crate::now());
        Ok(())
    }

    fn capture(&self, params: protocol::Capture) -> Result<String, RpcError> {
        let mut inner = lock(&self.inner);
        let backend = inner.backends.backend(&params.pane)?;
        if params.plain {
            backend.text(&params.pane).map_err(failed)
        } else {
            backend.capture(&params.pane).map_err(failed)
        }
    }

    fn settings(&self, params: protocol::Settings) -> Result<protocol::SettingsResult, RpcError> {
        let mut inner = lock(&self.inner);
        let config = Self::config(&mut inner, &params.env)?;
        Ok(protocol::SettingsResult {
            preview_interval_ms: config.preview_interval.as_millis() as u64,
        })
    }

    /// Answer one request. `stop` is handed to the main loop, which answers
    /// it after shutting down.
    fn handle(&self, request: &Request) -> Result<Value, RpcError> {
        fn json(value: impl Serialize) -> Result<Value, RpcError> {
            serde_json::to_value(value)
                .map_err(|error| RpcError::new(RpcError::FAILED, error.to_string()))
        }
        match request.method.as_str() {
            "hello" => json(self.hello(request.params()?)),
            "event" => json(self.event(request.params()?)?),
            "list" => json(self.list(request.params()?)?),
            "focus" => json(self.focus(request.params()?)?),
            "next" => json(self.next(request.params()?)?),
            "pin" => json(self.pin(request.params()?)?),
            "seen" => json(self.seen(request.params()?)?),
            "capture" => json(self.capture(request.params()?)?),
            "settings" => json(self.settings(request.params()?)?),
            "stop" => json(()),
            other => Err(RpcError::new(
                RpcError::METHOD_NOT_FOUND,
                format!("unknown method {other:?}"),
            )),
        }
    }

    /// Answer one client's requests until it hangs up.
    async fn serve(self: Arc<Self>, stream: UnixStream) -> io::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let (response, stopping) = match serde_json::from_str::<Request>(&line) {
                Ok(request) => {
                    let stopping = request.method == "stop";
                    let daemon = Arc::clone(&self);
                    let response =
                        tokio::task::spawn_blocking(move || match daemon.handle(&request) {
                            Ok(result) => Response::result(request.id.clone(), result),
                            Err(error) => Response::error(request.id.clone(), error),
                        })
                        .await
                        .map_err(io::Error::other)?;
                    (response, stopping)
                }
                Err(error) => (
                    Response::error(
                        Value::Null,
                        RpcError::new(RpcError::INVALID_PARAMS, error.to_string()),
                    ),
                    false,
                ),
            };
            let line = line_of(&response)?;
            if stopping {
                let _ = self.stop.send(Stopping {
                    reply: writer,
                    line,
                });
                return Ok(());
            }
            writer.write_all(line.as_bytes()).await?;
        }
        Ok(())
    }

    /// Send a new subscriber the greeting and a snapshot, then every change
    /// until it hangs up or falls too far behind.
    async fn subscribe(self: Arc<Self>, mut stream: UnixStream) -> io::Result<()> {
        let (snapshot, mut changes) = {
            // Subscribe under the lock, so no change falls between the
            // snapshot and the stream.
            let inner = lock(&self.inner);
            let snapshot = Message::Snapshot {
                seq: inner.seq,
                records: inner
                    .store
                    .records()
                    .iter()
                    .map(|(pane, record)| Stored {
                        pane: pane.clone(),
                        record: record.clone(),
                    })
                    .collect(),
            };
            (snapshot, self.events.subscribe())
        };
        let hello = Message::Hello {
            version: VERSION.to_owned(),
            protocol: protocol::PROTOCOL,
        };
        send(&mut stream, &hello).await?;
        send(&mut stream, &snapshot).await?;
        loop {
            match changes.recv().await {
                Ok(message) => send(&mut stream, &message).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "subscriber fell behind",
                    ));
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }
}

/// Write one message to a subscriber, giving up on one that doesn't read.
async fn send(stream: &mut UnixStream, message: &Message) -> io::Result<()> {
    let line = line_of(message)?;
    tokio::time::timeout(SUBSCRIBER_TIMEOUT, stream.write_all(line.as_bytes()))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "subscriber too slow"))?
}

fn line_of(value: &impl Serialize) -> io::Result<String> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// Bind `path`, replacing a socket file a dead daemon left.
fn bind(path: &Path) -> io::Result<UnixListener> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    UnixListener::bind(path)
}

/// Run the daemon in this process until `stop` is requested.
pub fn run(runtime: &Runtime) -> Result<(), Error> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(runtime))
}

async fn serve(runtime: &Runtime) -> Result<(), Error> {
    runtime.ensure()?;
    let lock_file = File::create(runtime.lock_file())?;
    if lock_file.try_lock().is_err() {
        return Err(Error::DaemonRunning(runtime.dir().display().to_string()));
    }

    let records = Journal::replay(&runtime.journal())?;
    let mut backends = Registry::default();
    for pane in records.keys() {
        if let Some(server) = &pane.server {
            backends.tmux(server);
        }
    }
    let (events, _) = broadcast::channel(SUBSCRIBER_BACKLOG);
    let (stop, mut stops) = mpsc::unbounded_channel();
    let daemon = Arc::new(Daemon {
        inner: Mutex::new(Inner {
            store: MemoryStore::new(records),
            journal: Journal::open(&runtime.journal())?,
            backends,
            seq: 0,
        }),
        events,
        stop,
    });
    {
        // Drop what died while no daemon ran, and compact the journal to
        // what is left.
        let mut inner = lock(&daemon.inner);
        let panes = inner.backends.panes();
        daemon.reconcile(&mut inner, &panes);
        inner.journal = Journal::rewrite(&runtime.journal(), inner.store.records())?;
    }

    let subscribers = bind(&runtime.events_socket())?;
    let requests = bind(&runtime.rpc_socket())?;
    let accepting = Arc::clone(&daemon);
    tokio::spawn(async move {
        while let Ok((stream, _)) = subscribers.accept().await {
            let daemon = Arc::clone(&accepting);
            tokio::spawn(async move {
                if let Err(error) = daemon.subscribe(stream).await
                    && !hung_up(&error)
                {
                    eprintln!("tracon: subscriber: {error}");
                }
            });
        }
    });
    let stopping = loop {
        tokio::select! {
            accepted = requests.accept() => {
                let (stream, _) = accepted?;
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    if let Err(error) = daemon.serve(stream).await
                        && !hung_up(&error)
                    {
                        eprintln!("tracon: client: {error}");
                    }
                });
            }
            stopping = stops.recv() => break stopping,
        }
    };
    let _ = fs::remove_file(runtime.rpc_socket());
    let _ = fs::remove_file(runtime.events_socket());
    drop(lock_file);
    // Only now can the next daemon start, so only now are the stops
    // answered.
    let mut stops_to_answer: Vec<_> = stopping.into_iter().collect();
    while let Ok(stopping) = stops.try_recv() {
        stops_to_answer.push(stopping);
    }
    for mut stopping in stops_to_answer {
        let _ = stopping.reply.write_all(stopping.line.as_bytes()).await;
    }
    Ok(())
}
