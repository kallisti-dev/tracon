//! The ordered list of agent panes that every front end shows.

use std::collections::HashMap;

use serde::Serialize;

use crate::backend::{Locator, PaneInfo};
use crate::harness::Harness;
use crate::state::{Record, Rule, State, sort_by_priority};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
}

/// Join live panes with their records, keep the panes that have an agent
/// state, a pin or a recognised agent process, and sort them by `rules`.
/// Records for panes that no longer exist are dropped. `screen` captures a
/// waiting pane, which is listed as working if its harness shows work there.
pub fn entries(
    panes: Vec<PaneInfo>,
    records: Vec<(Locator, Record)>,
    harnesses: &[Box<dyn Harness>],
    rules: &[Rule],
    screen: impl Fn(&Locator) -> Option<String>,
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
            if let (Some(state), Some(name)) = (&record.state, &record.harness)
                && state.is_under(&State::known(State::WAITING))
                && let Some(harness) = harnesses.iter().find(|h| h.name() == name)
                && screen(&info.pane).is_some_and(|screen| harness.shows_work(&screen))
            {
                record.state = Some(State::known(State::WORKING));
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
            })
        })
        .collect();
    sort_by_priority(&mut entries, rules, |entry| {
        (entry.state.as_ref(), entry.pinned, entry.since)
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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
        let listed = entries(
            panes,
            records,
            &harness::all(),
            &Config::default().order,
            |_| None,
        );
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

    #[test]
    fn a_waiting_pane_that_shows_work_is_listed_as_working() {
        let mut waiting = record("waiting.permission", 30);
        waiting.harness = Some("claude".to_owned());
        let panes = vec![pane("%1", "claude"), pane("%2", "claude")];
        let records = vec![
            (Locator::tmux("%1"), waiting.clone()),
            (Locator::tmux("%2"), waiting),
        ];
        let listed = entries(
            panes,
            records,
            &harness::all(),
            &Config::default().order,
            |pane| (pane.id == "%1").then(|| "✶ Kerfuffling… (21s · ↓ 109 tokens)".to_owned()),
        );
        let summary: Vec<_> = listed
            .iter()
            .map(|e| (e.pane.id.as_str(), e.state.as_ref().map(State::as_str)))
            .collect();
        assert_eq!(
            summary,
            [("%2", Some("waiting.permission")), ("%1", Some("working"))]
        );
    }
}
