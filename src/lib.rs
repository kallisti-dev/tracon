//! tracon: terminal approach control for coding agents.
//!
//! Harness adapters turn hook payloads into guarded state changes, a store
//! keeps the state per pane, and backends find, focus and capture panes.
//! Front ends read the ordered list and call commands; none is privileged.

pub mod backend;
pub mod config;
pub mod harness;
pub mod list;
pub mod screen;
pub mod state;
pub mod store;
pub mod tmux;

use std::time::{SystemTime, UNIX_EPOCH};

pub use backend::Locator;
pub use config::Config;
pub use state::{Record, Rule, State};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid state {0:?}")]
    InvalidState(String),
    #[error("invalid pane {0:?}")]
    InvalidPane(String),
    #[error("invalid {kind} {value:?}")]
    InvalidId { kind: &'static str, value: String },
    #[error("unsupported backend {0:?}")]
    UnsupportedBackend(String),
    #[error("unknown harness {0:?}")]
    UnknownHarness(String),
    #[error("invalid hook payload: {0}")]
    Payload(String),
    #[error("no tmux client is attached")]
    NoClient,
    #[error("pane {0} no longer exists")]
    PaneGone(String),
    #[error("could not run {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("tmux {command} failed: {stderr}")]
    Tmux { command: String, stderr: String },
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Collapse `text` to one line of at most `max` characters, replacing control
/// characters and runs of whitespace with a single space.
pub fn one_line(text: &str, max: usize) -> String {
    let mut line = String::new();
    for word in text.split(|c: char| c.is_whitespace() || c.is_control()) {
        if word.is_empty() {
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        if line.chars().count() > max {
            break;
        }
    }
    if line.chars().count() > max {
        line = line.chars().take(max.saturating_sub(1)).collect();
        line.push('…');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_collapses_whitespace_and_truncates() {
        assert_eq!(one_line("  a\n\n b\tc\u{1f}d ", 20), "a b c d");
        assert_eq!(one_line("abcdefghij", 5), "abcd…");
        assert_eq!(one_line("", 5), "");
    }
}
