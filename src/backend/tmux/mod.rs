//! tmux, the baseline backend.

pub mod setup;

use super::{Backend, Capabilities, Host, Locator, PaneInfo, TMUX};
use crate::Error;
use crate::tmux::{self, SEPARATOR, Tmux};

#[derive(Clone, Debug, Default)]
pub struct TmuxBackend {
    tmux: Tmux,
}

fn pane_id(pane: &Locator) -> Result<&str, Error> {
    if pane.backend != TMUX {
        return Err(Error::UnsupportedBackend(pane.backend.clone()));
    }
    if !tmux::valid_pane_id(&pane.id) {
        return Err(Error::InvalidPane(pane.id.clone()));
    }
    Ok(&pane.id)
}

fn joined(fields: &[&str]) -> String {
    fields
        .iter()
        .map(|field| format!("#{{{field}}}"))
        .collect::<Vec<_>>()
        .join(&SEPARATOR.to_string())
}

impl TmuxBackend {
    pub fn new(tmux: Tmux) -> Self {
        TmuxBackend { tmux }
    }

    pub fn tmux(&self) -> &Tmux {
        &self.tmux
    }

    /// The client to act on: `client` if given, otherwise the most recently
    /// active attached client.
    pub fn client(&self, client: Option<&str>) -> Result<String, Error> {
        if let Some(client) = client.filter(|client| !client.is_empty()) {
            return Ok(client.to_owned());
        }
        let output = self.tmux.run([
            "list-clients",
            "-F",
            &joined(&["client_activity", "client_name"]),
        ])?;
        output
            .lines()
            .filter_map(|line| line.split_once(SEPARATOR))
            .max_by_key(|(activity, _)| activity.parse::<u64>().unwrap_or(0))
            .map(|(_, name)| name.to_owned())
            .ok_or(Error::NoClient)
    }

    /// Run `command`, a shell command, in a popup on `client`, with
    /// `TRACON_CLIENT` naming the client in its environment.
    pub fn popup(&self, client: &str, title: &str, command: &str) -> Result<(), Error> {
        self.tmux.run([
            "display-popup",
            "-c",
            client,
            "-E",
            "-w",
            "80%",
            "-h",
            "70%",
            "-T",
            &tmux::escape_format(title),
            "-e",
            &format!("TRACON_CLIENT={client}"),
            &tmux::escape_format(command),
        ])?;
        Ok(())
    }

    /// Show `message` in the message line of `client`, or of the most
    /// recently active client.
    pub fn notify(&self, client: Option<&str>, message: &str) -> Result<(), Error> {
        let client = self.client(client)?;
        self.tmux.run([
            "display-message",
            "-c",
            &client,
            "-d",
            "5000",
            &tmux::escape_format(&format!("tracon: {message}")),
        ])?;
        Ok(())
    }
}

impl Backend for TmuxBackend {
    fn name(&self) -> &'static str {
        TMUX
    }

    fn locate(&self) -> Option<Locator> {
        if !Tmux::inside() {
            return None;
        }
        let pane = std::env::var("TMUX_PANE").ok()?;
        tmux::valid_pane_id(&pane).then(|| Locator::tmux(&pane))
    }

    fn panes(&self) -> Result<Vec<PaneInfo>, Error> {
        let format = joined(&[
            "pane_id",
            "session_name",
            "window_index",
            "pane_index",
            "pane_title",
            "pane_current_command",
        ]);
        let output = self.tmux.run(["list-panes", "-a", "-F", &format])?;
        Ok(output
            .lines()
            .filter_map(|line| {
                let values: Vec<&str> = line.split(SEPARATOR).collect();
                let [pane, session, window, index, title, command] = values[..] else {
                    return None;
                };
                Some(PaneInfo {
                    pane: Locator::tmux(pane),
                    location: format!("{session}:{window}.{index}"),
                    title: title.to_owned(),
                    command: command.to_owned(),
                })
            })
            .collect())
    }

    fn focus(&self, pane: &Locator, client: Option<&str>) -> Result<(), Error> {
        let pane = pane_id(pane)?;
        let client = self.client(client)?;
        // A pane id resolves as a session and window target too, so one call
        // moves the client however far the pane is.
        let result = self.tmux.run([
            "switch-client",
            "-c",
            &client,
            "-t",
            pane,
            ";",
            "select-window",
            "-t",
            pane,
            ";",
            "select-pane",
            "-t",
            pane,
        ]);
        match result {
            Err(error) if !self.alive(&Locator::tmux(pane))? => {
                drop(error);
                Err(Error::PaneGone(pane.to_owned()))
            }
            result => result.map(drop),
        }
    }

    fn current(&self, client: Option<&str>) -> Result<Option<Locator>, Error> {
        let client = match self.client(client) {
            Ok(client) => client,
            Err(Error::NoClient) => return Ok(None),
            Err(error) => return Err(error),
        };
        let pane = self
            .tmux
            .run(["display-message", "-p", "-c", &client, "#{pane_id}"])?;
        let pane = pane.trim();
        Ok(tmux::valid_pane_id(pane).then(|| Locator::tmux(pane)))
    }

    fn in_view(&self, pane: &Locator) -> Result<bool, Error> {
        let pane = pane_id(pane)?;
        let output =
            self.tmux
                .run(["list-clients", "-F", &joined(&["client_flags", "pane_id"])])?;
        Ok(output.lines().any(|line| {
            line.split_once(SEPARATOR).is_some_and(|(flags, shown)| {
                shown == pane && flags.split(',').any(|flag| flag == "focused")
            })
        }))
    }

    fn alive(&self, pane: &Locator) -> Result<bool, Error> {
        let pane = pane_id(pane)?;
        let output = self.tmux.run(["list-panes", "-a", "-F", "#{pane_id}"])?;
        Ok(output.lines().any(|line| line == pane))
    }

    fn capture(&self, pane: &Locator) -> Result<String, Error> {
        let pane = pane_id(pane)?;
        self.tmux.run(["capture-pane", "-e", "-p", "-t", pane])
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            popup: true,
            ansi_capture: true,
        }
    }

    fn host(&self, pane: &Locator) -> Result<Option<Host>, Error> {
        let pane = pane_id(pane)?;
        let session = self
            .tmux
            .run(["display-message", "-p", "-t", pane, "#{session_id}"])?;
        let output = self.tmux.run([
            "list-clients",
            "-t",
            session.trim(),
            "-F",
            &joined(&["client_activity", "client_pid", "client_tty"]),
        ])?;
        Ok(output
            .lines()
            .filter_map(|line| {
                let values: Vec<&str> = line.split(SEPARATOR).collect();
                let [activity, pid, tty] = values[..] else {
                    return None;
                };
                Some((
                    activity.parse::<u64>().unwrap_or(0),
                    pid.parse().ok(),
                    tty.to_owned(),
                ))
            })
            .max_by_key(|(activity, _, _)| *activity)
            .map(|(_, pid, tty)| Host {
                pid,
                tty: (!tty.is_empty()).then_some(tty),
            }))
    }
}
