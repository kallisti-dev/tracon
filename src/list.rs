//! The ordered list of agent panes that every front end shows.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::backend::{Locator, PaneInfo};
use crate::config::Config;
use crate::harness::Harness;
use crate::screen::{self, Sighting};
use crate::state::{Record, Standing, State, sort_by_priority};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub pane: Locator,
    pub location: String,
    pub title: String,
    pub command: String,
    /// `unknown` for an agent no hook has reported; absent for a pinned pane
    /// with no agent state.
    pub state: Option<State>,
    pub since: Option<u64>,
    pub message: Option<String>,
    pub harness: Option<String>,
    pub session: Option<String>,
    pub agent: Option<String>,
    pub pinned: bool,
    pub viewed: Option<u64>,
    /// What the harness recognised on the pane's screen, read for a state
    /// that can change without a hook; `state` already accounts for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sighting: Option<Sighting>,
}

/// Join live panes with their records, keep the panes that have an agent
/// state, a pin or a recognised agent process, and sort them as `config`
/// says. Records for panes that no longer exist are dropped. `text` captures
/// a pane's screen as plain text, for the corrections in [`crate::screen`].
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
            if record.state.is_none()
                && let Some(harness) = harnesses.iter().find(|h| h.runs_as(&info.command))
            {
                record.state = Some(State::known(State::UNKNOWN));
                record.harness = Some(harness.name().to_owned());
            }
            let sighting = record
                .state
                .as_ref()
                .filter(|state| screen::correctable(state))
                .and(record.harness.as_deref())
                .and_then(|name| harnesses.iter().find(|h| h.name() == name))
                .and_then(|harness| text(&info.pane).and_then(|text| harness.read_screen(&text)));
            if let (Some(state), Some(sighting)) = (&record.state, sighting)
                && let Some(corrected) = screen::correct(state, sighting)
            {
                record.state = Some(corrected);
            }
            (record.state.is_some() || record.pinned).then_some(Entry {
                pane: info.pane,
                location: info.location,
                title: info.title,
                command: info.command,
                state: record.state,
                since: record.since,
                message: record.message,
                harness: record.harness,
                session: record.session,
                agent: record.agent,
                pinned: record.pinned,
                viewed: record.viewed,
                sighting,
            })
        })
        .collect();
    sort_by_priority(&mut entries, &config.order, &config.recent_first, |entry| {
        Standing {
            state: entry.state.as_ref(),
            pinned: entry.pinned,
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

    fn record(state: &str, since: u64) -> Record {
        Record {
            state: Some(state.parse().unwrap()),
            since: Some(since),
            ..Record::default()
        }
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
            (Locator::tmux("%3"), record("done", 20)),
            (Locator::tmux("%4"), record("waiting.permission", 30)),
            (
                Locator::tmux("%5"),
                Record {
                    pinned: true,
                    ..Record::default()
                },
            ),
            (Locator::tmux("%6"), record("done", 10)),
            (Locator::tmux("%9"), record("waiting", 1)),
        ];
        let listed = entries(panes, records, &harness::all(), &Config::default(), |_| {
            None
        });
        let summary: Vec<_> = listed
            .iter()
            .map(|e| (e.pane.id.as_str(), e.state.as_ref().map(State::as_str)))
            .collect();
        assert_eq!(
            summary,
            [
                ("%4", Some("waiting.permission")),
                ("%5", None),
                ("%6", Some("done")),
                ("%3", Some("done")),
                ("%2", Some("unknown")),
            ]
        );
        assert_eq!(listed[4].harness.as_deref(), Some("claude"));
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
            ("%3", "working", "interrupted", "idle"),
            ("%4", "waiting.question", "unrecognised", "waiting.question"),
            ("%5", "done", "interrupted", "done"),
        ];
        let panes = cases.iter().map(|(id, ..)| pane(id, "reader")).collect();
        let records = cases
            .iter()
            .map(|(id, state, ..)| {
                let record = Record {
                    harness: Some("reader".to_owned()),
                    ..record(state, 30)
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
                entry.state.as_ref().map(State::as_str),
                Some(expected),
                "{id}"
            );
        }
        let sighting = |id: &str| listed.iter().find(|e| e.pane.id == id).unwrap().sighting;
        assert_eq!(sighting("%1"), Some(Sighting::Working));
        assert_eq!(sighting("%4"), None);
        // A finished pane's screen isn't read.
        assert_eq!(sighting("%5"), None);
    }
}
