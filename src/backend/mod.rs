//! Backends: the multiplexers and terminals that hold agent panes.

pub mod tmux;

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Error;

pub use tmux::TmuxBackend;

pub const TMUX: &str = "tmux";

/// Environment variables by which a backend recognises the pane and the
/// server a process runs in, such as `TMUX` and `TMUX_PANE`.
pub type Env = BTreeMap<String, String>;

/// A pane as one backend names it: the backend, the server instance when
/// the backend can run several, and an id only that backend interprets,
/// such as tmux's `%3`. The server is absent for a pane named from inside
/// it, and filled in by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Locator {
    pub backend: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

impl Locator {
    pub fn tmux(id: &str) -> Self {
        Locator {
            backend: TMUX.to_owned(),
            id: id.to_owned(),
            server: None,
        }
    }

    pub fn tmux_at(server: &str, id: &str) -> Self {
        Locator {
            server: Some(server.to_owned()),
            ..Locator::tmux(id)
        }
    }

    /// The same pane on `server`, when this locator names none.
    pub fn on(self, server: Option<&str>) -> Self {
        Locator {
            server: self.server.or_else(|| server.map(str::to_owned)),
            ..self
        }
    }
}

/// `tmux:%3`, a bare `%3`, which only tmux uses, or `tmux:%3@/path/to/socket`
/// naming the server.
impl FromStr for Locator {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        let (backend, rest) = text.split_once(':').unwrap_or((TMUX, text));
        if backend != TMUX {
            return Err(Error::UnsupportedBackend(backend.to_owned()));
        }
        let (id, server) = match rest.split_once('@') {
            Some((id, server)) if !server.is_empty() => (id, Some(server)),
            _ => (rest, None),
        };
        if !crate::tmux::valid_pane_id(id) {
            return Err(Error::InvalidPane(text.to_owned()));
        }
        Ok(Locator::tmux(id).on(server))
    }
}

impl fmt::Display for Locator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.backend, self.id)?;
        if let Some(server) = &self.server {
            write!(f, "@{server}")?;
        }
        Ok(())
    }
}

/// What a backend reports about a live pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PaneInfo {
    pub pane: Locator,
    /// Where the pane is, in the backend's own terms, such as `work:2.1`.
    pub location: String,
    pub title: String,
    /// The foreground command, used to spot agents no hook has reported.
    pub command: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// Can open a floating window over the current view.
    pub popup: bool,
    /// Captures keep colours and attributes as escape sequences.
    pub ansi_capture: bool,
}

/// The process showing a pane, for backends stacked inside a terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Host {
    pub pid: Option<u32>,
    pub tty: Option<String>,
}

pub trait Backend {
    fn name(&self) -> &'static str;
    /// The server instance this backend talks to, as its locators carry it.
    fn server(&self) -> Option<&str>;
    fn panes(&self) -> Result<Vec<PaneInfo>, Error>;
    /// Bring `pane` into view on `client`, or on the most recently active
    /// client when none is given.
    fn focus(&self, pane: &Locator, client: Option<&str>) -> Result<(), Error>;
    /// The pane `client` (or the most recently active client) shows.
    fn current(&self, client: Option<&str>) -> Result<Option<Locator>, Error>;
    /// Whether a client whose terminal has focus shows `pane`.
    fn in_view(&self, pane: &Locator) -> Result<bool, Error>;
    fn alive(&self, pane: &Locator) -> Result<bool, Error>;
    /// The pane's visible screen.
    fn capture(&self, pane: &Locator) -> Result<String, Error>;
    /// The pane's visible screen without colours or attributes.
    fn text(&self, pane: &Locator) -> Result<String, Error>;
    fn capabilities(&self) -> Capabilities;
    fn host(&self, pane: &Locator) -> Result<Option<Host>, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locators_parse_with_or_without_a_backend_and_server() {
        assert_eq!("%3".parse::<Locator>().unwrap(), Locator::tmux("%3"));
        assert_eq!("tmux:%3".parse::<Locator>().unwrap(), Locator::tmux("%3"));
        assert_eq!(Locator::tmux("%3").to_string(), "tmux:%3");
        let at = Locator::tmux_at("/tmp/s", "%3");
        assert_eq!(at.to_string(), "tmux:%3@/tmp/s");
        assert_eq!(at.to_string().parse::<Locator>().unwrap(), at);
        assert_eq!(Locator::tmux("%3").on(Some("/tmp/s")), at);
        assert_eq!(at.clone().on(Some("/tmp/other")), at);
        assert!("kitty:3".parse::<Locator>().is_err());
        assert!("3".parse::<Locator>().is_err());
        assert!("%3@".parse::<Locator>().is_err());
    }
}
