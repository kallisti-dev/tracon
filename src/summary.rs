//! The summary: how many panes are in each group of the list, for a status
//! line segment such as `#(tracon summary)`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::format::{Span, Style, Template};
use crate::list::Entry;
use crate::status::{Rule, Status};

/// The variables a summary format may use; [`Group::value`] gives each one.
pub const VARIABLES: [&str; 5] = ["count", "status", "label", "icon", "style"];

/// The panes under one status, with how that status is shown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub status: Status,
    pub count: usize,
    pub label: String,
    pub icon: String,
    pub style: String,
}

impl Group {
    /// The value of a summary variable, or `None` for a name outside
    /// [`VARIABLES`].
    pub fn value(&self, variable: &str) -> Option<String> {
        let text = match variable {
            "count" => self.count.to_string(),
            "status" => self.status.to_string(),
            "label" => self.label.clone(),
            "icon" => self.icon.clone(),
            "style" => self.style.clone(),
            _ => return None,
        };
        Some(text)
    }

    pub fn render(&self, format: &Template) -> Vec<Span> {
        format.render(&|variable| self.value(variable))
    }
}

/// Count the entries by the first status in `config.order` that covers
/// theirs, in the order's order, then any status the order doesn't cover
/// under its own name. Entries without a status, such as a pinned pane with
/// no agent, aren't counted, and empty groups aren't listed.
pub fn groups(entries: &[Entry], config: &Config) -> Vec<Group> {
    let mut counted = vec![false; entries.len()];
    let mut groups: Vec<(Status, usize)> = Vec::new();
    for rule in &config.order {
        let Rule::Status(status) = rule else {
            continue;
        };
        let mut count = 0;
        for (index, entry) in entries.iter().enumerate() {
            if !counted[index] && entry.status.as_ref().is_some_and(|s| s.is_under(status)) {
                counted[index] = true;
                count += 1;
            }
        }
        if count > 0 {
            groups.push((status.clone(), count));
        }
    }
    let mut rest: BTreeMap<Status, usize> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let (false, Some(status)) = (counted[index], &entry.status) {
            *rest.entry(status.clone()).or_default() += 1;
        }
    }
    groups.extend(rest);
    groups
        .into_iter()
        .map(|(status, count)| Group {
            label: config.label(&status),
            icon: config.icon(&status),
            style: config.style(&status),
            status,
            count,
        })
        .collect()
}

/// Every group in `format`, with `separator` between them.
pub fn render(groups: &[Group], format: &Template, separator: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            spans.push(Span {
                text: separator.to_owned(),
                style: Style::default(),
            });
        }
        spans.extend(group.render(format));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Locator;
    use crate::format::plain;
    use crate::status::parse_order;

    fn entry(status: Option<&str>) -> Entry {
        Entry {
            pane: Locator::tmux("%1"),
            location: String::new(),
            title: String::new(),
            command: String::new(),
            state: None,
            outcome: None,
            background: 0,
            since: None,
            message: None,
            harness: None,
            session: None,
            agent: None,
            pinned: status.is_none(),
            viewed: None,
            sighting: None,
            status: status.map(|name| name.parse().unwrap()),
            label: String::new(),
            icon: String::new(),
            style: String::new(),
        }
    }

    fn counts(groups: &[Group]) -> Vec<(&str, usize)> {
        groups
            .iter()
            .map(|group| (group.status.as_str(), group.count))
            .collect()
    }

    #[test]
    fn panes_are_counted_under_the_groups_of_the_order() {
        let entries: Vec<Entry> = [
            Some("waiting.permission"),
            Some("done"),
            Some("waiting.question"),
            Some("review"),
            None,
            Some("idle.interrupted"),
            Some("waiting.permission"),
        ]
        .into_iter()
        .map(entry)
        .collect();
        let config = Config::default();
        let counted = groups(&entries, &config);
        assert_eq!(
            counts(&counted),
            [("waiting", 3), ("done", 1), ("idle", 1), ("review", 1)]
        );
        assert_eq!(counted[0].label, "needs you");
        assert_eq!(counted[0].icon, "?");
        assert_eq!(counted[0].style, "bold yellow");
        assert_eq!(counted[3].label, "review");

        // A more specific rule takes its panes before a general one.
        let split = Config {
            order: parse_order("waiting.permission pinned waiting done").unwrap(),
            ..Config::default()
        };
        assert_eq!(
            counts(&groups(&entries, &split)),
            [
                ("waiting.permission", 2),
                ("waiting", 1),
                ("done", 1),
                ("idle.interrupted", 1),
                ("review", 1),
            ]
        );
        assert!(groups(&[], &config).is_empty());
    }

    #[test]
    fn the_summary_joins_the_groups() {
        let entries: Vec<Entry> = [Some("waiting.permission"), Some("done"), Some("done")]
            .into_iter()
            .map(entry)
            .collect();
        let config = Config::default();
        let counted = groups(&entries, &config);
        let spans = render(&counted, &config.summary.format, &config.summary.separator);
        assert_eq!(plain(&spans), "? 1 · ✓ 2");
        assert_eq!(spans[0].style, Style::parse("bold yellow").unwrap());
        assert_eq!(spans[1].style, Style::default());
        let words = Template::parse("{count} {label}").unwrap();
        assert_eq!(
            plain(&render(&counted, &words, ", ")),
            "1 needs you, 2 done"
        );
        assert_eq!(plain(&render(&[], &words, ", ")), "");
    }
}
