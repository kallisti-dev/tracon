//! What a pane's screen shows, for states that change without a hook.
//!
//! Hooks are the only writers of state, but some changes fire none: Claude
//! Code reports neither an approved prompt, nor an interrupt, nor a prompt
//! cancelled before it replied. The list reads the plain-text screen of each
//! pane in a state that can go stale, its harness says what it recognises
//! there ([`Harness::read_screen`]), and [`correct`] decides what the list
//! shows instead. Nothing is written back.
//! Every correction needs a sighting, so a screen the harness doesn't
//! recognise leaves the recorded state.
//!
//! [`Harness::read_screen`]: crate::harness::Harness::read_screen

use serde::Serialize;

use crate::State;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
pub fn correctable(state: &State) -> bool {
    state.is_under(&State::known(State::WAITING)) || *state == State::known(State::WORKING)
}

/// The state to list for a pane recorded in `state` whose screen shows
/// `sighting`, or `None` to list the recorded state.
pub fn correct(state: &State, sighting: Sighting) -> Option<State> {
    if !correctable(state) {
        return None;
    }
    match sighting {
        Sighting::Working if state.is_under(&State::known(State::WAITING)) => {
            Some(State::known(State::WORKING))
        }
        Sighting::Interrupted | Sighting::Finished => Some(State::known(State::IDLE)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sightings_correct_only_states_that_go_stale() {
        let corrected = |state: &str, sighting| {
            correct(&state.parse().unwrap(), sighting).map(|state| state.as_str().to_owned())
        };
        let working = Some("working".to_owned());
        let idle = Some("idle".to_owned());
        assert_eq!(corrected("waiting.permission", Sighting::Working), working);
        assert_eq!(corrected("waiting.question", Sighting::Interrupted), idle);
        assert_eq!(corrected("working", Sighting::Interrupted), idle);
        assert_eq!(corrected("working", Sighting::Finished), idle);
        assert_eq!(corrected("waiting.permission", Sighting::Finished), idle);
        assert_eq!(corrected("waiting.permission", Sighting::Prompt), None);
        assert_eq!(corrected("working", Sighting::Prompt), None);
        assert_eq!(corrected("working", Sighting::Working), None);
        for state in ["working.background", "done", "idle", "error", "unknown"] {
            for sighting in [
                Sighting::Prompt,
                Sighting::Working,
                Sighting::Interrupted,
                Sighting::Finished,
            ] {
                assert_eq!(corrected(state, sighting), None, "{state} {sighting:?}");
            }
        }
    }
}
