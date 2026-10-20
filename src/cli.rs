use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use tracon::backend::tmux::setup;
use tracon::backend::{Backend, TmuxBackend};
use tracon::harness::{self, Harness};
use tracon::list::{self, Entry};
use tracon::store::{Change, Cond, Guarded, Store, TmuxStore, Update};
use tracon::tmux::{self as tmux_util, Tmux};
use tracon::{Config, Locator, State};

use crate::{check, ui};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Terminal approach control for coding agents.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record a harness hook event read from standard input.
    Hook {
        /// The harness that ran the hook, such as `claude`.
        harness: String,
    },
    /// List agent panes, highest priority and longest wait first.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Focus a pane on a client.
    Focus {
        pane: Locator,
        /// The client to switch; defaults to $TRACON_CLIENT, then to the
        /// most recently active client.
        #[arg(long)]
        client: Option<String>,
    },
    /// Focus the top of the list, skipping the pane the client shows.
    Next {
        #[arg(long)]
        client: Option<String>,
    },
    /// Pin a pane, the current one by default.
    Pin { pane: Option<Locator> },
    /// Unpin a pane, the current one by default.
    Unpin { pane: Option<Locator> },
    /// Mark a finished pane as seen, the current one by default.
    Seen { pane: Option<Locator> },
    /// Choose a pane interactively.
    Pick {
        #[arg(long)]
        client: Option<String>,
    },
    /// Open the picker in a popup on a client.
    Popup { client: Option<String> },
    /// Register tracon with the tmux server, or remove it.
    Setup {
        #[arg(long)]
        remove: bool,
        /// The tmux plugin directory, recorded for diagnostics.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Report problems with the tmux and Claude Code setup.
    Check {
        /// A Claude session id this pane should be recording.
        #[arg(long)]
        session: Option<String>,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    // A hook never fails the agent's tool call: report and succeed.
    if let Command::Hook { harness } = &cli.command {
        if let Err(error) = hook(harness) {
            eprintln!("tracon: {error:#}");
        }
        return ExitCode::SUCCESS;
    }
    match dispatch(cli.command) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("tracon: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(command: Command) -> Result<ExitCode> {
    let tmux = Tmux;
    let backend = TmuxBackend::new(tmux.clone());
    let store = TmuxStore::new(tmux.clone());
    match command {
        Command::Hook { .. } => unreachable!("handled before dispatch"),
        Command::List { json } => {
            let entries = entries(&tmux, &backend, &store)?;
            let mut out = std::io::stdout().lock();
            if json {
                serde_json::to_writer_pretty(&mut out, &entries)?;
                writeln!(out)?;
            } else {
                let now = tracon::now();
                for entry in &entries {
                    writeln!(out, "{}", ui::plain_row(entry, now))?;
                }
            }
        }
        Command::Focus { pane, client } => backend.focus(&pane, client_hint(client).as_deref())?,
        Command::Next { client } => {
            let client = client_hint(client);
            let current = backend.current(client.as_deref())?;
            let entries = entries(&tmux, &backend, &store)?;
            match entries
                .iter()
                .find(|entry| Some(&entry.pane) != current.as_ref())
            {
                Some(entry) => backend.focus(&entry.pane, client.as_deref())?,
                None => {
                    let _ = backend.notify(client.as_deref(), "no other agent panes");
                }
            }
        }
        Command::Pin { pane } => store.set_pinned(&target(&backend, pane)?, true)?,
        Command::Unpin { pane } => store.set_pinned(&target(&backend, pane)?, false)?,
        Command::Seen { pane } => {
            let mut update = Update::new(State::known(State::IDLE));
            update.agent = tracon::store::Field::Clear;
            let seen = Guarded::when(
                Change::Set(update),
                Cond::StateUnder(State::known(State::DONE)),
            );
            store.apply(&target(&backend, pane)?, &seen, tracon::now())?;
        }
        Command::Pick { client } => {
            let client = client_hint(client);
            let config = Config::from_tmux(&tmux)?;
            ui::pick(&tmux, &backend, &store, &config, client.as_deref())?;
        }
        Command::Popup { client } => {
            let client = backend.client(client_hint(client).as_deref())?;
            if entries(&tmux, &backend, &store)?.is_empty() {
                backend.notify(Some(&client), "no agent panes")?;
            } else {
                let binary = std::env::current_exe().context("locating the tracon binary")?;
                let command = format!("{} pick", tmux_util::shell_quote(&binary.to_string_lossy()));
                backend.popup(&client, " tracon ", &command)?;
            }
        }
        Command::Setup { remove, dir } => {
            if !Tmux::inside() {
                bail!("run inside tmux (TMUX must identify the server)");
            }
            if remove {
                setup::remove(&tmux)?;
            } else {
                let binary = std::env::current_exe().context("locating the tracon binary")?;
                setup::install(&tmux, &binary, dir.as_deref(), VERSION)?;
            }
        }
        Command::Check { session } => {
            return check::run(&tmux, &backend, &store, session.as_deref());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The listed panes, ordered by the configured rules.
pub fn entries(tmux: &Tmux, backend: &impl Backend, store: &impl Store) -> Result<Vec<Entry>> {
    let config = Config::from_tmux(tmux)?;
    let records = store.load()?;
    let panes = backend.panes()?;
    Ok(list::entries(
        panes,
        records,
        &harness::all(),
        &config.order,
    ))
}

fn client_hint(client: Option<String>) -> Option<String> {
    client
        .or_else(|| std::env::var("TRACON_CLIENT").ok())
        .filter(|client| !client.is_empty())
}

fn target(backend: &impl Backend, pane: Option<Locator>) -> Result<Locator> {
    match pane.or_else(|| backend.locate()) {
        Some(pane) => Ok(pane),
        None => bail!("name a pane, or run inside one"),
    }
}

fn hook(name: &str) -> Result<()> {
    let backend = TmuxBackend::default();
    // Agents also run outside tmux; there is nothing to record.
    let Some(pane) = backend.locate() else {
        return Ok(());
    };
    let harness: Box<dyn Harness> = harness::by_name(name)?;
    let mut input = String::new();
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        bail!("expected a hook payload on standard input");
    }
    stdin.lock().read_to_string(&mut input)?;
    let payload: serde_json::Value =
        serde_json::from_str(&input).context("parsing the hook payload")?;
    if let Some(change) = harness.interpret(&payload)? {
        TmuxStore::default().apply(&pane, &change, tracon::now())?;
    }
    Ok(())
}
