//! What a pane's screen shows, for states that change without a hook.
//!
//! Hooks are the only writers of state, but some changes fire none: Claude
//! Code reports neither an approved prompt, nor an interrupt, nor a prompt
//! cancelled before it replied. The list reads the plain-text screen of each
//! pane in a state that can go stale, its harness says what it recognises
//! there ([`Harness::read_screen`]), and the status function
//! ([`crate::status::status`]) takes the sighting into account. Nothing is
//! written back, and a screen the harness doesn't recognise changes nothing.
//!
//! [`Harness::read_screen`]: crate::harness::Harness::read_screen

use serde::{Deserialize, Serialize};

use crate::State;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sighting {
    /// A prompt for the user is open.
    Prompt,
    /// The agent is working.
    Working,
    /// The user refused a prompt or interrupted the agent, which now waits
    /// for input.
    Interrupted,
    /// The latest turn on screen has ended and no prompt has followed it.
    Finished,
}

/// Whether a pane in `state` can change without a hook, so its screen is
/// worth reading.
pub fn worth_reading(state: &State) -> bool {
    state.is_under(&State::known(State::WAITING)) || *state == State::known(State::WORKING)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_turn_in_progress_goes_stale() {
        for state in ["waiting.permission", "waiting.question", "working"] {
            assert!(worth_reading(&state.parse().unwrap()), "{state}");
        }
        assert!(!worth_reading(&State::known(State::READY)));
    }
}
