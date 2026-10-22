//! Harness adapters: turn a harness's hook payloads into state changes.

pub mod claude;

use serde_json::Value;

use crate::Error;
use crate::screen::Sighting;
use crate::store::Guarded;

pub use claude::Claude;

pub trait Harness {
    /// The name hooks pass to `tracon hook` and the store records.
    fn name(&self) -> &'static str;

    /// The change one hook payload makes, or `None` for an event that says
    /// nothing about the pane's state.
    fn interpret(&self, payload: &Value) -> Result<Option<Guarded>, Error>;

    /// Whether a pane's foreground command, as tmux reports it, is this
    /// harness. Panes running it that no hook has reported are `unknown`.
    fn runs_as(&self, command: &str) -> bool;

    /// What this harness recognises on a pane's visible screen, given as
    /// plain text. See [`crate::screen`] for how the list uses it.
    fn read_screen(&self, _text: &str) -> Option<Sighting> {
        None
    }
}

/// Every built-in harness.
pub fn all() -> Vec<Box<dyn Harness>> {
    vec![Box::new(Claude)]
}

pub fn by_name(name: &str) -> Result<Box<dyn Harness>, Error> {
    all()
        .into_iter()
        .find(|harness| harness.name() == name)
        .ok_or_else(|| Error::UnknownHarness(name.to_owned()))
}

/// Whether `command` names `program`, allowing a directory, a leading dot
/// and a suffix such as Nix's `.claude-wrapped`, which tmux truncates to
/// `.claude-unwrapp`.
pub fn command_is(command: &str, program: &str) -> bool {
    let name = command.rsplit('/').next().unwrap_or(command);
    let name = name.strip_prefix('.').unwrap_or(name);
    name.strip_prefix(program)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', '.', ' ']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_match_wrapped_programs() {
        for command in [
            "claude",
            ".claude-unwrapp",
            ".claude-wrapped",
            "/bin/claude",
            "claude.exe",
        ] {
            assert!(command_is(command, "claude"), "{command}");
        }
        for command in ["claudette", "zsh", "", "nclaude"] {
            assert!(!command_is(command, "claude"), "{command}");
        }
    }
}
