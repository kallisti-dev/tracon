//! What the user cares about: the status derived from a pane's record and,
//! as a last resort, from its screen; and the order panes are listed in,
//! which rules on statuses decide.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;
use crate::screen::Sighting;
use crate::state::{Name, Outcome, Record, State};

/// A status in the status tree, such as `idle` or `idle.interrupted`.
///
/// Rules, labels and icons name a status and cover every status beneath
/// it, so `waiting` covers both waits.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Status(Name);

impl Status {
    pub const WAITING: &str = "waiting";
    pub const WAITING_PERMISSION: &str = "waiting.permission";
    pub const WAITING_QUESTION: &str = "waiting.question";
    pub const ERROR: &str = "error";
    pub const DONE: &str = "done";
    pub const WORKING: &str = "working";
    pub const IDLE: &str = "idle";
    pub const IDLE_INTERRUPTED: &str = "idle.interrupted";
    pub const UNKNOWN: &str = "unknown";

    /// A status known to be well formed, such as one of the constants above.
    pub fn known(name: &'static str) -> Self {
        name.parse().expect("a well-formed status constant")
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn name(&self) -> &Name {
        &self.0
    }

    /// Whether this status is `ancestor` or lies beneath it in the tree.
    pub fn is_under(&self, ancestor: &Status) -> bool {
        self.0.is_under(&ancestor.0)
    }

    /// The status above this one, or `None` at the root.
    pub fn parent(&self) -> Option<Status> {
        self.0.parent().map(Status)
    }
}

impl FromStr for Status {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self, Error> {
        Name::parse("status", name).map(Status)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Status {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

/// The default status function: the status of a pane with `record`, whose
/// screen shows `sighting` when it was read. The first rule that applies:
///
/// | State | Sighting | Status |
/// | --- | --- | --- |
/// | `waiting.*` | the agent works | `working` |
/// | `waiting.*`, `working` | interrupted, or a turn ended with no prompt after it | `idle.interrupted` |
/// | `waiting.*` | | the same |
/// | `working` | | `working` |
/// | `ready`, outcome `error` | | `error` |
/// | `ready`, outcome `interrupted` | | `idle.interrupted` |
/// | `ready`, outcome `finished`, not seen since | | `done` |
/// | `ready` otherwise | | `idle` |
///
/// Background tasks and pins modify a status rather than make one. A record
/// with no state has no status; the list gives a harness no hook has
/// reported `unknown`.
pub fn status(record: &Record, sighting: Option<Sighting>) -> Option<Status> {
    let state = record.state.as_ref()?;
    let waiting = state.is_under(&State::known(State::WAITING));
    let working = *state == State::known(State::WORKING);
    let status = match sighting {
        Some(Sighting::Working) if waiting => Status::known(Status::WORKING),
        Some(Sighting::Interrupted | Sighting::Finished) if waiting || working => {
            Status::known(Status::IDLE_INTERRUPTED)
        }
        _ if waiting => Status(state.name().clone()),
        _ if working => Status::known(Status::WORKING),
        _ => match record.outcome {
            Some(Outcome::Error) => Status::known(Status::ERROR),
            Some(Outcome::Interrupted) => Status::known(Status::IDLE_INTERRUPTED),
            Some(Outcome::Finished) if !record.seen() => Status::known(Status::DONE),
            _ => Status::known(Status::IDLE),
        },
    };
    Some(status)
}

/// One entry of the `order` list: a status, matching everything beneath it,
/// or a modifier, `pinned` matching pinned panes and `background` panes
/// with background tasks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    Status(Status),
    Pinned,
    Background,
}

impl Rule {
    pub fn matches(&self, standing: &Standing<'_>) -> bool {
        match self {
            Rule::Pinned => standing.pinned,
            Rule::Background => standing.background,
            Rule::Status(rule) => standing.status.is_some_and(|status| status.is_under(rule)),
        }
    }

    /// Whether every pane this rule matches, `other` matches too.
    fn within(&self, other: &Rule) -> bool {
        match (self, other) {
            (Rule::Pinned, Rule::Pinned) | (Rule::Background, Rule::Background) => true,
            (Rule::Status(status), Rule::Status(other)) => status.is_under(other),
            _ => false,
        }
    }
}

impl FromStr for Rule {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self, Error> {
        match name {
            "pinned" => Ok(Rule::Pinned),
            "background" => Ok(Rule::Background),
            _ => name.parse().map(Rule::Status),
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rule::Pinned => f.write_str("pinned"),
            Rule::Background => f.write_str("background"),
            Rule::Status(status) => status.fmt(f),
        }
    }
}

/// Parse a whitespace-separated order, such as `waiting pinned error`.
pub fn parse_order(order: &str) -> Result<Vec<Rule>, Error> {
    order.split_whitespace().map(str::parse).collect()
}

/// What the rules and the sort read from one item.
pub struct Standing<'a> {
    pub status: Option<&'a Status>,
    pub pinned: bool,
    /// Background tasks run.
    pub background: bool,
    /// When the item entered its state.
    pub since: Option<u64>,
    /// When a client last focused it.
    pub viewed: Option<u64>,
}

/// The group an item sorts into: the index of the first rule it matches, or
/// `rules.len()` when it matches none, so unmatched items go last.
pub fn rank(rules: &[Rule], standing: &Standing<'_>) -> usize {
    rules
        .iter()
        .position(|rule| rule.matches(standing))
        .unwrap_or(rules.len())
}

/// Sort items by the group their status falls in. Within a group, the item
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
        let standing = key(item);
        let group = rank(rules, &standing);
        let by_view = rules
            .get(group)
            .is_some_and(|rule| recent_first.iter().any(|recent| rule.within(recent)));
        let within = if by_view {
            (
                standing.viewed.is_none(),
                latest_first(standing.viewed),
                latest_first(standing.since),
            )
        } else {
            (false, standing.since.unwrap_or(u64::MAX), 0)
        };
        (group, within)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_named(name: &str) -> Status {
        name.parse().unwrap()
    }

    fn record(state: &str, outcome: Option<Outcome>) -> Record {
        Record {
            state: Some(state.parse().unwrap()),
            outcome,
            since: Some(10),
            ..Record::default()
        }
    }

    fn standing<'a>(status: Option<&'a Status>, pinned: bool, background: bool) -> Standing<'a> {
        Standing {
            status,
            pinned,
            background,
            since: None,
            viewed: None,
        }
    }

    #[test]
    fn the_status_follows_the_state_and_outcome() {
        let of = |record: &Record| status(record, None).unwrap().as_str().to_owned();
        assert_eq!(of(&record("working", None)), "working");
        assert_eq!(
            of(&record("waiting.permission", None)),
            "waiting.permission"
        );
        assert_eq!(of(&record("waiting.question", None)), "waiting.question");
        assert_eq!(of(&record("ready", None)), "idle");
        assert_eq!(of(&record("ready", Some(Outcome::Error))), "error");
        assert_eq!(
            of(&record("ready", Some(Outcome::Interrupted))),
            "idle.interrupted"
        );
        let finished = record("ready", Some(Outcome::Finished));
        assert_eq!(of(&finished), "done");
        let seen = Record {
            viewed: Some(10),
            ..finished.clone()
        };
        assert_eq!(of(&seen), "idle");
        let failed_and_seen = Record {
            viewed: Some(10),
            ..record("ready", Some(Outcome::Error))
        };
        assert_eq!(of(&failed_and_seen), "error");
        // Background tasks modify the status rather than make one.
        let background = Record {
            background: 2,
            ..finished
        };
        assert_eq!(of(&background), "done");
        assert_eq!(status(&Record::default(), None), None);
    }

    #[test]
    fn sightings_correct_only_states_that_go_stale() {
        let of = |state: &str, sighting| {
            status(&record(state, None), Some(sighting))
                .unwrap()
                .as_str()
                .to_owned()
        };
        assert_eq!(of("waiting.permission", Sighting::Working), "working");
        assert_eq!(
            of("waiting.question", Sighting::Interrupted),
            "idle.interrupted"
        );
        assert_eq!(of("working", Sighting::Interrupted), "idle.interrupted");
        assert_eq!(of("working", Sighting::Finished), "idle.interrupted");
        assert_eq!(
            of("waiting.permission", Sighting::Finished),
            "idle.interrupted"
        );
        assert_eq!(
            of("waiting.permission", Sighting::Prompt),
            "waiting.permission"
        );
        assert_eq!(of("working", Sighting::Prompt), "working");
        assert_eq!(of("working", Sighting::Working), "working");
        for sighting in [
            Sighting::Prompt,
            Sighting::Working,
            Sighting::Interrupted,
            Sighting::Finished,
        ] {
            assert_eq!(of("ready", sighting), "idle", "{sighting:?}");
        }
    }

    #[test]
    fn invalid_statuses_are_rejected() {
        let error = "Done".parse::<Status>().unwrap_err().to_string();
        assert_eq!(error, "invalid status \"Done\"");
        assert_eq!(
            status_named("idle.interrupted").parent(),
            Some(status_named("idle"))
        );
    }

    #[test]
    fn pinned_and_background_are_modifiers() {
        let rules = parse_order("waiting pinned background idle").unwrap();
        let idle = status_named("idle");
        let question = status_named("waiting.question");
        let done = status_named("done");
        assert_eq!(rank(&rules, &standing(Some(&idle), true, false)), 1);
        assert_eq!(rank(&rules, &standing(Some(&question), true, false)), 0);
        assert_eq!(rank(&rules, &standing(Some(&idle), false, false)), 3);
        assert_eq!(rank(&rules, &standing(Some(&done), false, true)), 2);
        assert_eq!(rank(&rules, &standing(Some(&done), false, false)), 4);
        assert_eq!(rank(&rules, &standing(None, true, false)), 1);
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
            ("idle.interrupted", false, Some(40)),
            ("unknown", false, None),
            ("other", false, Some(0)),
        ];
        let parsed: Vec<_> = panes
            .iter()
            .map(|(name, _, _)| status_named(name))
            .collect();
        sort_by_priority(&mut panes, &rules, &[], |(name, pinned, since)| {
            let index = parsed.iter().position(|s| s.as_str() == *name).unwrap();
            Standing {
                status: Some(&parsed[index]),
                pinned: *pinned,
                background: false,
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
                "working",
                "idle",
                "idle.interrupted",
                "unknown",
                "other",
            ]
        );
    }

    #[test]
    fn recent_first_groups_lead_with_the_pane_viewed_last() {
        let rules = parse_order("done idle").unwrap();
        let recent_first = parse_order("idle").unwrap();
        // (name, status, since, viewed)
        let mut panes = vec![
            ("old idle, viewed early", "idle", 10, Some(100)),
            ("new idle, never viewed", "idle", 50, None),
            ("old idle, never viewed", "idle", 5, None),
            ("idle, viewed last", "idle", 40, Some(200)),
            ("new done, viewed last", "done", 60, Some(300)),
            ("old done", "done", 20, None),
        ];
        let parsed: Vec<_> = panes
            .iter()
            .map(|(_, name, ..)| status_named(name))
            .collect();
        sort_by_priority(
            &mut panes,
            &rules,
            &recent_first,
            |(_, name, since, viewed)| Standing {
                status: parsed.iter().find(|s| s.as_str() == *name),
                pinned: false,
                background: false,
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
