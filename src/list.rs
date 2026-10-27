//! The ordered list of agent panes that every front end shows: each pane
//! with its state, the status derived from it and how that status is shown.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::backend::{Locator, PaneInfo};
use crate::config::Config;
use crate::format::{Span, Template};
use crate::harness::Harness;
use crate::label::Table;
use crate::screen::{self, Sighting};
use crate::state::{Outcome, Record, State};
use crate::status::{self, Standing, Status, sort_by_priority};

/// The variables a row format may use; [`Entry::value`] gives each one.
pub const VARIABLES: [&str; 13] = [
    "icon",
    "label",
    "status",
    "style",
    "state",
    "outcome",
    "background",
    "age",
    "location",
    "harness",
    "title",
    "message",
    "pin",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub pane: Locator,
    pub location: String,
    pub title: String,
    pub command: String,
    pub state: Option<State>,
    pub outcome: Option<Outcome>,
    #[serde(default)]
    pub background: u32,
    pub since: Option<u64>,
    pub message: Option<String>,
    pub harness: Option<String>,
    pub session: Option<String>,
    pub agent: Option<String>,
    pub pinned: bool,
    pub viewed: Option<u64>,
    /// What the harness recognised on the pane's screen, read for a state
    /// that can change without a hook; `status` already accounts for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sighting: Option<Sighting>,
    /// `unknown` for an agent no hook has reported; absent for a pinned pane
    /// with no agent.
    pub status: Option<Status>,
    /// How the status is shown, by the configured tables; empty without a
    /// status.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub style: String,
}

impl Entry {
    /// The value of a row variable, empty when the pane has none, or `None`
    /// for a name outside [`VARIABLES`]. `now` is for the age.
    pub fn value(&self, variable: &str, now: u64) -> Option<String> {
        let text = match variable {
            "icon" => self.icon.clone(),
            "label" => self.label.clone(),
            "status" => self
                .status
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            "style" => self.style.clone(),
            "state" => self
                .state
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            "outcome" => self
                .outcome
                .map(|outcome| outcome.as_str().to_owned())
                .unwrap_or_default(),
            "background" => match self.background {
                0 => String::new(),
                count => count.to_string(),
            },
            "age" => age(self.since, now),
            "location" => self.location.clone(),
            "harness" => self.harness.clone().unwrap_or_default(),
            "title" => self.title.clone(),
            "message" => self.message.clone().unwrap_or_default(),
            "pin" => if self.pinned { "*" } else { "" }.to_owned(),
            _ => return None,
        };
        Some(text)
    }

    /// The row for this pane in `format`.
    pub fn row(&self, format: &Template, now: u64) -> Vec<Span> {
        format.render(&|variable| self.value(variable, now))
    }
}

/// How long ago `since` was, such as `42s`, `5m` or `3h`.
pub fn age(since: Option<u64>, now: u64) -> String {
    let Some(since) = since else {
        return String::new();
    };
    let seconds = now.saturating_sub(since);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

/// Join live panes with their records, keep the panes that have an agent
/// state, a pin or a recognised agent process, derive each one's status and
/// how it is shown, and sort them as `config` says. Records for panes that
/// no longer exist are dropped. `text` captures a pane's screen as plain
/// text, for the sightings in [`crate::screen`].
pub fn entries(
    panes: Vec<PaneInfo>,
    records: Vec<(Locator, Record)>,
    harnesses: &[Box<dyn Harness>],
    config: &Config,
    text: impl Fn(&Locator) -> Option<String>,
) -> Vec<Entry> {
    let mut records: HashMap<Locator, Record> = records.into_iter().collect();
    let mut entries: Vec<Entry> = panes
        .into_iter()
        .filter_map(|info| {
            let mut record = records.remove(&info.pane).unwrap_or_default();
            let mut status = None;
            if record.state.is_none()
                && let Some(harness) = harnesses.iter().find(|h| h.runs_as(&info.command))
            {
                status = Some(Status::known(Status::UNKNOWN));
                record.harness = Some(harness.name().to_owned());
            }
            let sighting = record
                .state
                .as_ref()
                .filter(|state| screen::worth_reading(state))
                .and(record.harness.as_deref())
                .and_then(|name| harnesses.iter().find(|h| h.name() == name))
                .and_then(|harness| text(&info.pane).and_then(|text| harness.read_screen(&text)));
            let status = status.or_else(|| status::status(&record, sighting));
            if status.is_none() && !record.pinned {
                return None;
            }
            let shown = |table: &Table| {
                status
                    .as_ref()
                    .and_then(|status| table.get(status))
                    .unwrap_or_default()
                    .to_owned()
            };
            // A status without a label is shown by its name.
            let label = match &status {
                Some(status) => config
                    .labels
                    .get(status)
                    .unwrap_or(status.as_str())
                    .to_owned(),
                None => String::new(),
            };
            Some(Entry {
                pane: info.pane,
                location: info.location,
                title: info.title,
                command: info.command,
                state: record.state,
                outcome: record.outcome,
                background: record.background,
                since: record.since,
                message: record.message,
                harness: record.harness,
                session: record.session,
                agent: record.agent,
                pinned: record.pinned,
                viewed: record.viewed,
                sighting,
                icon: shown(&config.icons),
                style: shown(&config.styles),
                label,
                status,
            })
        })
        .collect();
    sort_by_priority(&mut entries, &config.order, &config.recent_first, |entry| {
        Standing {
            status: entry.status.as_ref(),
            pinned: entry.pinned,
            background: entry.background > 0,
            since: entry.since,
            viewed: entry.viewed,
        }
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness;

    fn pane(id: &str, command: &str) -> PaneInfo {
        PaneInfo {
            pane: Locator::tmux(id),
            location: format!("s:0.{id}"),
            title: String::new(),
            command: command.to_owned(),
        }
    }

    fn record(state: &str, outcome: Option<Outcome>, since: u64) -> Record {
        Record {
            state: Some(state.parse().unwrap()),
            outcome,
            since: Some(since),
            ..Record::default()
        }
    }

    fn listed(entries: &[Entry]) -> Vec<(&str, Option<&str>)> {
        entries
            .iter()
            .map(|e| (e.pane.id.as_str(), e.status.as_ref().map(Status::as_str)))
            .collect()
    }

    #[test]
    fn panes_are_listed_by_priority_and_wait() {
        let panes = vec![
            pane("%1", "zsh"),
            pane("%2", ".claude-unwrapp"),
            pane("%3", "claude"),
            pane("%4", "claude"),
            pane("%5", "nvim"),
            pane("%6", "zsh"),
        ];
        let records = vec![
            (
                Locator::tmux("%3"),
                record("ready", Some(Outcome::Finished), 20),
            ),
            (Locator::tmux("%4"), record("waiting.permission", None, 30)),
            (
                Locator::tmux("%5"),
                Record {
                    pinned: true,
                    ..Record::default()
                },
            ),
            (
                Locator::tmux("%6"),
                record("ready", Some(Outcome::Finished), 10),
            ),
            (Locator::tmux("%9"), record("waiting", None, 1)),
        ];
        let entries = entries(panes, records, &harness::all(), &Config::default(), |_| {
            None
        });
        assert_eq!(
            listed(&entries),
            [
                ("%4", Some("waiting.permission")),
                ("%5", None),
                ("%6", Some("done")),
                ("%3", Some("done")),
                ("%2", Some("unknown")),
            ]
        );
        assert_eq!(entries[4].harness.as_deref(), Some("claude"));
        assert_eq!(entries[4].label, "not reporting");
        assert_eq!(entries[0].icon, "?");
        assert_eq!(entries[0].style, "bold yellow");
        assert_eq!(entries[1].label, "");
    }

    #[test]
    fn rows_follow_the_format() {
        let panes = vec![pane("%1", "claude"), pane("%2", "claude")];
        let records = vec![
            (
                Locator::tmux("%1"),
                Record {
                    background: 2,
                    message: Some("ok".into()),
                    harness: Some("claude".into()),
                    ..record("ready", Some(Outcome::Finished), 40)
                },
            ),
            (
                Locator::tmux("%2"),
                Record {
                    pinned: true,
                    harness: Some("claude".into()),
                    ..record("working", None, 90)
                },
            ),
        ];
        let config = Config::default();
        let entries = entries(panes, records, &harness::all(), &config, |_| None);
        let rows: Vec<String> = entries
            .iter()
            .map(|entry| crate::format::plain(&entry.row(&config.format, 100)))
            .collect();
        // The pinned pane sorts first. Each column is padded as the default
        // format says, and the title stands in for a missing message.
        let row = |pin, icon, label, age, message| {
            format!(
                "{pin}{icon} {label:<16} {age:>4}  {:<18} {:<7} {message}",
                "s:0.%2", "claude"
            )
            .replace("%2", if label == "done" { "%1" } else { "%2" })
        };
        assert_eq!(
            rows,
            [
                row("*", "◐", "working", "10s", ""),
                row(" ", "✓", "done", "1m", "ok · 2 in background"),
            ]
        );
        assert_eq!(entries[1].value("status", 100).as_deref(), Some("done"));
        assert_eq!(
            entries[1].value("outcome", 100).as_deref(),
            Some("finished")
        );
        assert_eq!(entries[0].value("background", 100).as_deref(), Some(""));
        assert_eq!(entries[0].value("colour", 100), None);
    }

    #[test]
    fn ages_use_the_largest_whole_unit() {
        assert_eq!(age(Some(100), 142), "42s");
        assert_eq!(age(Some(0), 300), "5m");
        assert_eq!(age(Some(0), 3 * 3600 + 5), "3h");
        assert_eq!(age(Some(0), 2 * 86400), "2d");
        assert_eq!(age(Some(10), 5), "0s");
        assert_eq!(age(None, 5), "");
    }

    /// A harness whose screens are the names of sightings.
    struct Reader;

    impl Harness for Reader {
        fn name(&self) -> &'static str {
            "reader"
        }

        fn interpret(
            &self,
            _: &serde_json::Value,
        ) -> Result<Option<crate::store::Guarded>, crate::Error> {
            Ok(None)
        }

        fn runs_as(&self, _: &str) -> bool {
            false
        }

        fn read_screen(&self, text: &str) -> Option<Sighting> {
            match text {
                "prompt" => Some(Sighting::Prompt),
                "working" => Some(Sighting::Working),
                "interrupted" => Some(Sighting::Interrupted),
                _ => None,
            }
        }
    }

    #[test]
    fn screens_correct_states_that_change_without_a_hook() {
        let cases = [
            ("%1", "waiting.permission", "working", "working"),
            ("%2", "waiting.permission", "prompt", "waiting.permission"),
            ("%3", "working", "interrupted", "idle.interrupted"),
            ("%4", "waiting.question", "unrecognised", "waiting.question"),
            ("%5", "ready", "interrupted", "idle"),
        ];
        let panes = cases.iter().map(|(id, ..)| pane(id, "reader")).collect();
        let records = cases
            .iter()
            .map(|(id, state, ..)| {
                let record = Record {
                    harness: Some("reader".to_owned()),
                    ..record(state, None, 30)
                };
                (Locator::tmux(id), record)
            })
            .collect();
        let harnesses: Vec<Box<dyn Harness>> = vec![Box::new(Reader)];
        let listed = entries(panes, records, &harnesses, &Config::default(), |pane| {
            cases
                .iter()
                .find(|(id, ..)| *id == pane.id)
                .map(|(_, _, text, _)| text.to_string())
        });
        for (id, _, _, expected) in cases {
            let entry = listed.iter().find(|entry| entry.pane.id == id).unwrap();
            assert_eq!(
                entry.status.as_ref().map(Status::as_str),
                Some(expected),
                "{id}"
            );
        }
        let sighting = |id: &str| listed.iter().find(|e| e.pane.id == id).unwrap().sighting;
        assert_eq!(sighting("%1"), Some(Sighting::Working));
        assert_eq!(sighting("%4"), None);
        // A ready pane's screen isn't read.
        assert_eq!(sighting("%5"), None);
        let interrupted = listed.iter().find(|e| e.pane.id == "%3").unwrap();
        assert_eq!(interrupted.label, "interrupted");
        assert_eq!(interrupted.icon, "○");
    }
}
