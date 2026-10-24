//! Agent states, per-pane records, and the order panes are listed in.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;

/// A state in the state tree, such as `waiting` or `waiting.permission`.
///
/// Each dot-separated segment narrows its parent, so a rule naming a state
/// matches every state beneath it. Adapters report the most specific state
/// their harness distinguishes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct State(String);

impl State {
    pub const WAITING: &str = "waiting";
    pub const WAITING_PERMISSION: &str = "waiting.permission";
    pub const WAITING_QUESTION: &str = "waiting.question";
    pub const ERROR: &str = "error";
    pub const DONE: &str = "done";
    pub const IDLE: &str = "idle";
    pub const WORKING: &str = "working";
    pub const WORKING_BACKGROUND: &str = "working.background";
    pub const UNKNOWN: &str = "unknown";

    /// A state known to be well formed, such as one of the constants above.
    pub fn known(name: &'static str) -> Self {
        debug_assert!(valid_state(name), "invalid state constant {name}");
        State(name.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The top-level state this one belongs to, such as `waiting` for
    /// `waiting.permission`.
    pub fn root(&self) -> &str {
        self.0.split('.').next().unwrap_or(&self.0)
    }

    /// Whether this state is `ancestor` or lies beneath it in the tree.
    pub fn is_under(&self, ancestor: &State) -> bool {
        self.0
            .strip_prefix(ancestor.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    }
}

/// Segments are lowercase ASCII words, so a state can appear unquoted in tmux
/// formats and option values.
fn valid_state(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|segment| {
            let mut chars = segment.chars();
            chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

impl FromStr for State {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self, Error> {
        if valid_state(name) {
            Ok(State(name.to_owned()))
        } else {
            Err(Error::InvalidState(name.to_owned()))
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for State {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for State {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

/// What a store keeps for one pane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub state: Option<State>,
    /// Seconds since the Unix epoch when the pane entered its state.
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

/// One entry of the `order` list: a state, matching everything beneath it, or
/// the `pinned` pseudo-state, matching pinned panes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    State(State),
    Pinned,
}

impl Rule {
    pub fn matches(&self, state: Option<&State>, pinned: bool) -> bool {
        match self {
            Rule::Pinned => pinned,
            Rule::State(rule) => state.is_some_and(|state| state.is_under(rule)),
        }
    }

    /// Whether every pane this rule matches, `other` matches too.
    fn within(&self, other: &Rule) -> bool {
        match (self, other) {
            (Rule::Pinned, Rule::Pinned) => true,
            (Rule::State(state), Rule::State(other)) => state.is_under(other),
            _ => false,
        }
    }
}

impl FromStr for Rule {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self, Error> {
        if name == "pinned" {
            Ok(Rule::Pinned)
        } else {
            name.parse().map(Rule::State)
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rule::Pinned => f.write_str("pinned"),
            Rule::State(state) => state.fmt(f),
        }
    }
}

/// Parse a whitespace-separated order, such as `waiting pinned error`.
pub fn parse_order(order: &str) -> Result<Vec<Rule>, Error> {
    order.split_whitespace().map(str::parse).collect()
}

/// The group a pane sorts into: the index of the first rule it matches, or
/// `rules.len()` when it matches none, so unmatched panes go last.
pub fn rank(rules: &[Rule], state: Option<&State>, pinned: bool) -> usize {
    rules
        .iter()
        .position(|rule| rule.matches(state, pinned))
        .unwrap_or(rules.len())
}

/// What sorting reads from one item.
pub struct Standing<'a> {
    pub state: Option<&'a State>,
    pub pinned: bool,
    /// When the item entered its state.
    pub since: Option<u64>,
    /// When a client last focused it.
    pub viewed: Option<u64>,
}

/// Sort items by the group their state falls in. Within a group, the item
/// longest in its state comes first. In a group whose rule lies within one
/// of `recent_first`, the item viewed most recently comes first instead,
/// then items never viewed, newest state first. A missing time sorts last.
pub fn sort_by_priority<T>(
    items: &mut [T],
    rules: &[Rule],
    recent_first: &[Rule],
    key: impl Fn(&T) -> Standing<'_>,
) {
    let latest_first = |time: Option<u64>| time.map_or(u64::MAX, |time| u64::MAX - time);
    items.sort_by_cached_key(|item| {
        let Standing {
            state,
            pinned,
            since,
            viewed,
        } = key(item);
        let group = rank(rules, state, pinned);
        let by_view = rules
            .get(group)
            .is_some_and(|rule| recent_first.iter().any(|recent| rule.within(recent)));
        let within = if by_view {
            (viewed.is_none(), latest_first(viewed), latest_first(since))
        } else {
            (false, since.unwrap_or(u64::MAX), 0)
        };
        (group, within)
    });
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
        assert_eq!(permission.root(), "waiting");
    }

    #[test]
    fn invalid_states_are_rejected() {
        for name in [
            "", "Waiting", "waiting.", ".waiting", "wait ing", "a,b", "a}b",
        ] {
            assert!(name.parse::<State>().is_err(), "{name:?} parsed");
        }
        assert!("working.background".parse::<State>().is_ok());
    }

    #[test]
    fn pinned_is_a_pseudo_state() {
        let rules = parse_order("waiting pinned idle").unwrap();
        assert_eq!(rank(&rules, Some(&state("idle")), true), 1);
        assert_eq!(rank(&rules, Some(&state("waiting.question")), true), 0);
        assert_eq!(rank(&rules, Some(&state("idle")), false), 2);
        assert_eq!(rank(&rules, Some(&state("done")), false), 3);
        assert_eq!(rank(&rules, None, true), 1);
    }

    #[test]
    fn sorting_groups_by_rule_then_longest_wait() {
        let rules = parse_order("waiting pinned error done working idle unknown").unwrap();
        let mut panes = vec![
            ("idle", false, Some(1)),
            ("working", false, Some(50)),
            ("waiting.permission", false, Some(30)),
            ("done", true, Some(5)),
            ("waiting.question", false, Some(10)),
            ("working.background", false, Some(40)),
            ("unknown", false, None),
            ("other", false, Some(0)),
        ];
        let parsed: Vec<_> = panes.iter().map(|(name, _, _)| state(name)).collect();
        sort_by_priority(&mut panes, &rules, &[], |(name, pinned, since)| {
            let index = parsed.iter().position(|s| s.as_str() == *name).unwrap();
            Standing {
                state: Some(&parsed[index]),
                pinned: *pinned,
                since: *since,
                viewed: None,
            }
        });
        let names: Vec<_> = panes.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(
            names,
            [
                "waiting.question",
                "waiting.permission",
                "done",
                "working.background",
                "working",
                "idle",
                "unknown",
                "other",
            ]
        );
    }

    #[test]
    fn recent_first_groups_lead_with_the_pane_viewed_last() {
        let rules = parse_order("done idle").unwrap();
        let recent_first = parse_order("idle").unwrap();
        // (name, state, since, viewed)
        let mut panes = vec![
            ("old idle, viewed early", "idle", 10, Some(100)),
            ("new idle, never viewed", "idle", 50, None),
            ("old idle, never viewed", "idle", 5, None),
            ("idle, viewed last", "idle", 40, Some(200)),
            ("new done, viewed last", "done", 60, Some(300)),
            ("old done", "done", 20, None),
        ];
        let parsed: Vec<_> = panes.iter().map(|(_, name, ..)| state(name)).collect();
        sort_by_priority(
            &mut panes,
            &rules,
            &recent_first,
            |(_, name, since, viewed)| Standing {
                state: parsed.iter().find(|s| s.as_str() == *name),
                pinned: false,
                since: Some(*since),
                viewed: *viewed,
            },
        );
        let names: Vec<_> = panes.iter().map(|(name, ..)| *name).collect();
        assert_eq!(
            names,
            [
                "old done",
                "new done, viewed last",
                "idle, viewed last",
                "old idle, viewed early",
                "new idle, never viewed",
                "old idle, never viewed",
            ]
        );
    }
}
