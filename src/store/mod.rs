//! Where pane state lives between hook invocations.
//!
//! A change carries a condition on the pane's current record, so an adapter
//! can say "enter `waiting.permission` unless the pane already waits" without
//! a separate read. Stores apply the condition and the change atomically.

pub mod tmux;

use crate::state::{Record, State};
use crate::{Error, Locator};

pub use tmux::TmuxStore;

/// How a change treats one field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Field<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl<T: Clone> Field<T> {
    fn apply(&self, value: &mut Option<T>) {
        match self {
            Field::Keep => {}
            Field::Clear => *value = None,
            Field::Set(new) => *value = Some(new.clone()),
        }
    }
}

/// Enter a state. `since` is kept when the pane is already in that exact
/// state, so repeated events don't reset the wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    pub state: State,
    pub message: Field<String>,
    pub harness: Field<String>,
    pub session: Field<String>,
    pub agent: Field<String>,
}

impl Update {
    pub fn new(state: State) -> Self {
        Update {
            state,
            message: Field::Keep,
            harness: Field::Keep,
            session: Field::Keep,
            agent: Field::Keep,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Set(Update),
    /// Forget the pane's state, keeping its pin.
    Clear,
}

/// A condition on a pane's current record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cond {
    Always,
    /// The pane's state is this state or beneath it.
    StateUnder(State),
    /// The current wait was raised by this subagent, or by the main agent
    /// for `None`.
    Agent(Option<String>),
    /// The recorded harness session is this one.
    Session(String),
    Not(Box<Cond>),
    All(Vec<Cond>),
}

impl Cond {
    pub fn negated(cond: Cond) -> Cond {
        Cond::Not(Box::new(cond))
    }

    pub fn eval(&self, record: &Record) -> bool {
        match self {
            Cond::Always => true,
            Cond::StateUnder(state) => record.state.as_ref().is_some_and(|s| s.is_under(state)),
            Cond::Agent(agent) => {
                record.agent.as_deref().unwrap_or("") == agent.as_deref().unwrap_or("")
            }
            Cond::Session(session) => record.session.as_deref() == Some(session.as_str()),
            Cond::Not(cond) => !cond.eval(record),
            Cond::All(conds) => conds.iter().all(|cond| cond.eval(record)),
        }
    }
}

/// A change and the condition under which a store applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guarded {
    pub change: Change,
    pub when: Cond,
}

impl Guarded {
    pub fn always(change: Change) -> Self {
        Guarded {
            change,
            when: Cond::Always,
        }
    }

    pub fn when(change: Change, when: Cond) -> Self {
        Guarded { change, when }
    }

    /// Apply to an in-memory record at time `now`: the reference semantics
    /// every store implements.
    pub fn apply_to(&self, record: &mut Record, now: u64) {
        if !self.when.eval(record) {
            return;
        }
        match &self.change {
            Change::Clear => {
                *record = Record {
                    pinned: record.pinned,
                    ..Record::default()
                }
            }
            Change::Set(update) => {
                if record.state.as_ref() != Some(&update.state) {
                    record.since = Some(now);
                }
                record.state = Some(update.state.clone());
                update.message.apply(&mut record.message);
                update.harness.apply(&mut record.harness);
                update.session.apply(&mut record.session);
                update.agent.apply(&mut record.agent);
            }
        }
    }
}

pub trait Store {
    /// Every pane's record, including panes with only a pin.
    fn load(&self) -> Result<Vec<(Locator, Record)>, Error>;
    /// Apply `change` to `pane` if its condition holds, at time `now`.
    fn apply(&self, pane: &Locator, change: &Guarded, now: u64) -> Result<(), Error>;
    fn set_pinned(&self, pane: &Locator, pinned: bool) -> Result<(), Error>;
}

/// Session and agent ids appear in store queries, so they are limited to the
/// characters harnesses use for them.
pub fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> State {
        name.parse().unwrap()
    }

    #[test]
    fn since_is_kept_while_the_state_is_unchanged() {
        let mut record = Record::default();
        let working = Guarded::always(Change::Set(Update::new(state("working"))));
        working.apply_to(&mut record, 10);
        working.apply_to(&mut record, 20);
        assert_eq!(record.since, Some(10));
        Guarded::always(Change::Set(Update::new(state("done")))).apply_to(&mut record, 30);
        assert_eq!(record.since, Some(30));
    }

    #[test]
    fn conditions_guard_changes() {
        let mut record = Record {
            state: Some(state("waiting.question")),
            agent: Some("a1".into()),
            session: Some("s1".into()),
            pinned: true,
            ..Record::default()
        };
        let unless_waiting = Guarded::when(
            Change::Set(Update::new(state("waiting.permission"))),
            Cond::negated(Cond::StateUnder(state("waiting"))),
        );
        unless_waiting.apply_to(&mut record, 1);
        assert_eq!(record.state, Some(state("waiting.question")));

        assert!(Cond::Agent(Some("a1".into())).eval(&record));
        assert!(!Cond::Agent(None).eval(&record));
        assert!(Cond::Agent(None).eval(&Record::default()));

        Guarded::when(Change::Clear, Cond::Session("s2".into())).apply_to(&mut record, 2);
        assert!(record.state.is_some());
        Guarded::when(Change::Clear, Cond::Session("s1".into())).apply_to(&mut record, 3);
        assert_eq!(
            record,
            Record {
                pinned: true,
                ..Record::default()
            }
        );
    }

    #[test]
    fn ids_are_restricted() {
        assert!(valid_id("ae48a9ce1c4008eda"));
        assert!(valid_id("403d5457-3ea6-4a34-8e30-a706b39e4067"));
        for id in ["", "a,b", "a}", "a b", &"x".repeat(129)] {
            assert!(!valid_id(id), "{id:?}");
        }
    }
}
