use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use tracon::backend::TmuxBackend;
use tracon::backend::tmux::mirror;
use tracon::backend::tmux::setup;
use tracon::client::Client;
use tracon::format;
use tracon::list::Entry;
use tracon::protocol::{self, NextResult, SettingsResult};
use tracon::runtime::Runtime;
use tracon::tmux::{self as tmux_util, Tmux};
use tracon::{Locator, daemon};

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
    /// Record that a pane was viewed, marking it seen if it finished; the
    /// current one by default. The focus hook runs this.
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
    /// Run the daemon in the foreground, or stop the one running. Commands
    /// start it on demand, so this is rarely needed.
    Daemon {
        #[arg(long)]
        stop: bool,
    },
    /// Mirror this tmux server's records into its panes' options until the
    /// server exits. Setup starts it.
    Mirror,
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
        // A reader such as `head` that closed the pipe wants nothing more.
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("tracon: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(command: Command) -> Result<ExitCode> {
    let runtime = Runtime::from_env();
    match command {
        Command::Hook { .. } => unreachable!("handled before dispatch"),
        Command::List { json } => {
            let env = protocol::env();
            let mut client = Client::connect(&runtime)?;
            let settings: SettingsResult =
                client.call("settings", protocol::Settings { env: env.clone() })?;
            let entries: Vec<Entry> = client.call("list", protocol::List { env })?;
            let mut out = std::io::stdout().lock();
            if json {
                serde_json::to_writer_pretty(&mut out, &entries)?;
                writeln!(out)?;
            } else {
                let format = ui::row_format(&settings);
                let now = tracon::now();
                for entry in &entries {
                    writeln!(out, "{}", format::plain(&entry.row(&format, now)))?;
                }
            }
        }
        Command::Focus { pane, client } => {
            let params = protocol::Focus {
                pane: pane.to_string(),
                client: client_hint(client),
                env: protocol::env(),
            };
            Client::connect(&runtime)?.call::<()>("focus", params)?;
        }
        Command::Next { client } => {
            let client = client_hint(client);
            let params = protocol::Next {
                client: client.clone(),
                env: protocol::env(),
            };
            let result: NextResult = Client::connect(&runtime)?.call("next", params)?;
            if result.focused.is_none() {
                let _ = TmuxBackend::default().notify(client.as_deref(), "no other agent panes");
            }
        }
        Command::Pin { pane } => pin(&runtime, pane, true)?,
        Command::Unpin { pane } => pin(&runtime, pane, false)?,
        Command::Seen { pane } => {
            let params = protocol::Seen {
                pane: pane.map(|pane| pane.to_string()),
                env: protocol::env(),
            };
            Client::connect(&runtime)?.call::<()>("seen", params)?;
        }
        Command::Pick { client } => {
            let client = client_hint(client);
            ui::pick(&runtime, client.as_deref())?;
        }
        Command::Popup { client } => {
            let backend = TmuxBackend::default();
            let client = backend.client(client_hint(client).as_deref())?;
            if entries(&runtime)?.is_empty() {
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
            let tmux = Tmux::default();
            if remove {
                setup::remove(&tmux)?;
            } else {
                let binary = std::env::current_exe().context("locating the tracon binary")?;
                setup::install(&tmux, &binary, dir.as_deref(), VERSION)?;
                // Start the daemon now, so it knows this server before the
                // first hook, replacing one left by an earlier installation,
                // and say so rather than fail the tmux load.
                if let Err(error) = Client::connect_current(&runtime) {
                    eprintln!("tracon: the daemon did not start: {error:#}");
                }
            }
        }
        Command::Check { session } => {
            return check::run(&runtime, session.as_deref());
        }
        Command::Daemon { stop: false } => daemon::run(&runtime)?,
        Command::Mirror => {
            let Some(tmux) = Tmux::from_env() else {
                bail!("run inside tmux (TMUX must identify the server)");
            };
            let server = tmux
                .socket()
                .map(|socket| socket.to_string_lossy().into_owned())
                .unwrap_or_default();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(mirror::run(&runtime, &server))?;
        }
        Command::Daemon { stop: true } => match Client::connect_running(&runtime) {
            Ok(client) => client.stop()?,
            Err(tracon::Error::Daemon { .. }) => {}
            Err(error) => return Err(error.into()),
        },
    }
    Ok(ExitCode::SUCCESS)
}

/// The listed panes, ordered by the configured rules.
pub fn entries(runtime: &Runtime) -> Result<Vec<Entry>> {
    let params = protocol::List {
        env: protocol::env(),
    };
    Ok(Client::connect(runtime)?.call("list", params)?)
}

fn pin(runtime: &Runtime, pane: Option<Locator>, pinned: bool) -> Result<()> {
    let params = protocol::Pin {
        pane: pane.map(|pane| pane.to_string()),
        pinned,
        env: protocol::env(),
    };
    Client::connect(runtime)?.call::<()>("pin", params)?;
    Ok(())
}

fn client_hint(client: Option<String>) -> Option<String> {
    client
        .or_else(|| std::env::var("TRACON_CLIENT").ok())
        .filter(|client| !client.is_empty())
}

fn hook(name: &str) -> Result<()> {
    let env = protocol::env();
    // Agents also run outside any pane tracon knows; there is nothing to
    // record, and no daemon to start for it.
    if TmuxBackend::claim(&env).is_none() {
        return Ok(());
    }
    let mut input = String::new();
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        bail!("expected a hook payload on standard input");
    }
    stdin.lock().read_to_string(&mut input)?;
    let payload: serde_json::Value =
        serde_json::from_str(&input).context("parsing the hook payload")?;
    let params = protocol::Event {
        harness: name.to_owned(),
        payload,
        env,
    };
    Client::connect(&Runtime::from_env())?.call::<protocol::EventResult>("event", params)?;
    Ok(())
}
