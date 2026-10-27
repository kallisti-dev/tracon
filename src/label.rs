//! How a status is shown: its label, icon and style, each from a table
//! keyed by status. A lookup walks up the status tree, so an entry for
//! `waiting` covers both waits unless a more specific one exists.

use std::collections::BTreeMap;

use crate::Error;
use crate::status::Status;

const LABELS: [(&str, &str); 9] = [
    ("waiting", "needs you"),
    ("waiting.permission", "needs approval"),
    ("waiting.question", "has a question"),
    ("error", "failed"),
    ("done", "done"),
    ("working", "working"),
    ("idle", "idle"),
    ("idle.interrupted", "interrupted"),
    ("unknown", "not reporting"),
];

const ICONS: [(&str, &str); 6] = [
    ("waiting", "?"),
    ("error", "!"),
    ("done", "✓"),
    ("working", "◐"),
    ("idle", "○"),
    ("unknown", "·"),
];

const STYLES: [(&str, &str); 6] = [
    ("waiting", "bold yellow"),
    ("error", "bold red"),
    ("done", "green"),
    ("working", "blue"),
    ("idle", "dim"),
    ("unknown", "dim"),
];

/// Text per status, looked up along the status tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table(BTreeMap<Status, String>);

impl Table {
    fn new(entries: &[(&str, &str)]) -> Self {
        Table(
            entries
                .iter()
                .map(|(status, text)| {
                    let status = status.parse().expect("a well-formed default status");
                    (status, (*text).to_owned())
                })
                .collect(),
        )
    }

    /// The default labels.
    pub fn labels() -> Self {
        Table::new(&LABELS)
    }

    /// The default icons.
    pub fn icons() -> Self {
        Table::new(&ICONS)
    }

    /// The default styles, as [`crate::format::Style`] reads them.
    pub fn styles() -> Self {
        Table::new(&STYLES)
    }

    /// This table with the entries of `overrides` added or replaced, from
    /// an option value such as `waiting.permission=approve?,done=unread`.
    pub fn with(mut self, option: &'static str, overrides: &str) -> Result<Self, Error> {
        for entry in overrides.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((status, text)) = entry.split_once('=') else {
                return Err(Error::InvalidOption {
                    option,
                    reason: format!("expected status=text, found {entry:?}"),
                });
            };
            let status = status
                .trim()
                .parse()
                .map_err(|error: Error| Error::InvalidOption {
                    option,
                    reason: error.to_string(),
                })?;
            self.0.insert(status, text.trim().to_owned());
        }
        Ok(self)
    }

    /// The entry for `status`, or for the nearest status above it.
    pub fn get(&self, status: &Status) -> Option<&str> {
        let mut status = Some(status.clone());
        while let Some(current) = status {
            if let Some(text) = self.0.get(&current) {
                return Some(text);
            }
            status = current.parent();
        }
        None
    }

    pub fn values(&self) -> impl Iterator<Item = &str> {
        self.0.values().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str) -> Status {
        name.parse().unwrap()
    }

    #[test]
    fn lookups_walk_up_the_tree() {
        let labels = Table::labels();
        assert_eq!(
            labels.get(&status("waiting.permission")),
            Some("needs approval")
        );
        assert_eq!(labels.get(&status("waiting.other")), Some("needs you"));
        assert_eq!(labels.get(&status("idle.interrupted")), Some("interrupted"));
        assert_eq!(labels.get(&status("review")), None);
        assert_eq!(Table::icons().get(&status("idle.interrupted")), Some("○"));
    }

    #[test]
    fn overrides_replace_and_add_entries() {
        let labels = Table::labels()
            .with(
                "@tracon-labels",
                " waiting=needs you!, done = unread,, review=in review",
            )
            .unwrap();
        assert_eq!(
            labels.get(&status("waiting.permission")),
            Some("needs approval")
        );
        assert_eq!(labels.get(&status("waiting.other")), Some("needs you!"));
        assert_eq!(labels.get(&status("done")), Some("unread"));
        assert_eq!(labels.get(&status("review")), Some("in review"));

        let error = |text: &str| {
            Table::labels()
                .with("@tracon-labels", text)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            error("done"),
            "invalid @tracon-labels: expected status=text, found \"done\""
        );
        assert_eq!(
            error("Done=x"),
            "invalid @tracon-labels: invalid status \"Done\""
        );
    }
}
