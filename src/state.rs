//! What the hooks record per pane: the agent's activity, how its last turn
//! ended, and the rest of the record. Nothing derived is stored; see
//! [`crate::status`] for what the list makes of it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;

/// A name in a tree, as dot-separated lowercase segments. Each segment
/// narrows its parent, so a rule naming a node covers every node beneath it.
/// States and statuses are both such names, of two different trees.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(String);

impl Name {
    /// Segments are lowercase ASCII words, so a name can appear unquoted in
    /// tmux formats and option values. `kind` names the tree for the error.
    pub fn parse(kind: &'static str, text: &str) -> Result<Self, Error> {
        let valid = !text.is_empty()
            && text.split('.').all(|segment| {
                let mut chars = segment.chars();
                chars.next().is_some_and(|c| c.is_ascii_lowercase())
                    && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
        if valid {
            Ok(Name(text.to_owned()))
        } else {
            Err(Error::Invalid {
                kind,
                value: text.to_owned(),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The top-level node this one belongs to, such as `waiting` for
    /// `waiting.permission`.
    pub fn root(&self) -> &str {
        self.0.split('.').next().unwrap_or(&self.0)
    }

    /// The node above this one, or `None` at the root.
    pub fn parent(&self) -> Option<Name> {
        self.0
            .rsplit_once('.')
            .map(|(parent, _)| Name(parent.to_owned()))
    }

    /// Whether this node is `ancestor` or lies beneath it.
    pub fn is_under(&self, ancestor: &Name) -> bool {
        self.0
            .strip_prefix(ancestor.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    }
}

/// What the agent is doing, as its harness last reported.
///
/// `working`, `waiting.permission` and `waiting.question` are a turn in
/// progress; `ready` is a pane with no turn running, whose [`Outcome`] says
/// how the last one ended. Adapters report the most specific state their
/// harness distinguishes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct State(Name);

impl State {
    pub const WORKING: &str = "working";
    pub const WAITING: &str = "waiting";
    pub const WAITING_PERMISSION: &str = "waiting.permission";
    pub const WAITING_QUESTION: &str = "waiting.question";
    pub const READY: &str = "ready";

    /// A state known to be well formed, such as one of the constants above.
    pub fn known(name: &'static str) -> Self {
        name.parse().expect("a well-formed state constant")
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn name(&self) -> &Name {
        &self.0
    }

    /// Whether this state is `ancestor` or lies beneath it in the tree.
    pub fn is_under(&self, ancestor: &State) -> bool {
        self.0.is_under(&ancestor.0)
    }
}

impl FromStr for State {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self, Error> {
        Name::parse("state", name).map(State)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for State {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for State {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

/// How the last turn ended. Cleared by the next prompt, and by a report
/// that the turn ended some way the harness didn't say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Finished,
    Error,
    /// The user cut the turn short: an interrupt, a refused prompt, a
    /// cancelled prompt.
    Interrupted,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Finished => "finished",
            Outcome::Error => "error",
            Outcome::Interrupted => "interrupted",
        }
    }
}

/// What a store keeps for one pane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub state: Option<State>,
    pub outcome: Option<Outcome>,
    /// Background tasks still running when the turn ended.
    pub background: u32,
    /// Seconds since the Unix epoch when the state or outcome last changed.
    pub since: Option<u64>,
    pub message: Option<String>,
    pub harness: Option<String>,
    /// The harness's session id, used only to tell a late SessionEnd from an
    /// old session apart from the current one.
    pub session: Option<String>,
    /// The subagent that raised the current `waiting` state, if one did.
    pub agent: Option<String>,
    pub pinned: bool,
    /// Seconds since the Unix epoch when a client last focused the pane.
    pub viewed: Option<u64>,
}

impl Record {
    /// Whether a client has focused the pane since its state last changed.
    pub fn seen(&self) -> bool {
        matches!((self.viewed, self.since), (Some(viewed), Some(since)) if viewed >= since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> State {
        name.parse().unwrap()
    }

    #[test]
    fn states_match_their_ancestors_by_whole_segments() {
        let permission = state("waiting.permission");
        assert!(permission.is_under(&state("waiting")));
        assert!(permission.is_under(&permission));
        assert!(!state("waiting").is_under(&permission));
        assert!(!state("waitingroom").is_under(&state("waiting")));
        assert_eq!(permission.name().root(), "waiting");
        assert_eq!(
            permission.name().parent(),
            Some(state("waiting").name().clone())
        );
        assert_eq!(state("waiting").name().parent(), None);
    }

    #[test]
    fn invalid_states_are_rejected() {
        for name in [
            "", "Waiting", "waiting.", ".waiting", "wait ing", "a,b", "a}b",
        ] {
            let error = name.parse::<State>().unwrap_err().to_string();
            assert_eq!(error, format!("invalid state {name:?}"));
        }
        assert!("waiting.permission".parse::<State>().is_ok());
    }

    #[test]
    fn a_pane_is_seen_once_viewed_after_its_last_change() {
        let mut record = Record {
            since: Some(10),
            ..Record::default()
        };
        assert!(!record.seen());
        record.viewed = Some(9);
        assert!(!record.seen());
        record.viewed = Some(10);
        assert!(record.seen());
        assert!(!Record::default().seen());
    }
}
