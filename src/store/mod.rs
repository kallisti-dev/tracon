//! Changes to a pane's record, and the store that holds the records.
//!
//! A change carries a condition on the pane's current record, so an adapter
//! can say "enter `waiting.permission` unless the pane already waits" without
//! a separate read. The daemon applies the condition and the change in one
//! step, under its lock.

pub mod memory;

use crate::state::{Outcome, Record, State};

pub use memory::MemoryStore;

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
/// state with the same outcome, so repeated events don't reset the wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    pub state: State,
    pub outcome: Field<Outcome>,
    /// The number of background tasks, when the event reports it.
    pub background: Option<u32>,
    pub message: Field<String>,
    pub harness: Field<String>,
    pub session: Field<String>,
    pub agent: Field<String>,
}

impl Update {
    pub fn new(state: State) -> Self {
        Update {
            state,
            outcome: Field::Keep,
            background: None,
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
    /// Forget the pane's state, keeping its pin and when it was viewed.
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
    /// The last turn ended this way, or hasn't ended for `None`.
    Outcome(Option<Outcome>),
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
            Cond::Outcome(outcome) => record.outcome == *outcome,
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

    /// Apply to a record at time `now`.
    pub fn apply_to(&self, record: &mut Record, now: u64) {
        if !self.when.eval(record) {
            return;
        }
        match &self.change {
            Change::Clear => {
                *record = Record {
                    pinned: record.pinned,
                    viewed: record.viewed,
                    ..Record::default()
                }
            }
            Change::Set(update) => {
                let before = (record.state.clone(), record.outcome);
                record.state = Some(update.state.clone());
                update.outcome.apply(&mut record.outcome);
                if (record.state.clone(), record.outcome) != before {
                    record.since = Some(now);
                }
                if let Some(background) = update.background {
                    record.background = background;
                }
                update.message.apply(&mut record.message);
                update.harness.apply(&mut record.harness);
                update.session.apply(&mut record.session);
                update.agent.apply(&mut record.agent);
            }
        }
    }
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
    fn since_is_kept_while_the_state_and_outcome_are_unchanged() {
        let mut record = Record::default();
        let working = Guarded::always(Change::Set(Update::new(state("working"))));
        working.apply_to(&mut record, 10);
        working.apply_to(&mut record, 20);
        assert_eq!(record.since, Some(10));
        let mut finished = Update::new(state("ready"));
        finished.outcome = Field::Set(Outcome::Finished);
        finished.background = Some(2);
        Guarded::always(Change::Set(finished.clone())).apply_to(&mut record, 30);
        assert_eq!(record.since, Some(30));
        assert_eq!(record.outcome, Some(Outcome::Finished));
        assert_eq!(record.background, 2);
        // The same again keeps the time; a kept outcome and count stay.
        let mut again = Update::new(state("ready"));
        again.background = None;
        Guarded::always(Change::Set(again)).apply_to(&mut record, 40);
        assert_eq!(record.since, Some(30));
        assert_eq!(
            (record.outcome, record.background),
            (Some(Outcome::Finished), 2)
        );
        // A changed outcome in the same state starts a new wait.
        finished.outcome = Field::Set(Outcome::Error);
        Guarded::always(Change::Set(finished)).apply_to(&mut record, 50);
        assert_eq!(record.since, Some(50));
        assert_eq!(record.outcome, Some(Outcome::Error));
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
        assert!(Cond::Outcome(None).eval(&record));
        record.outcome = Some(Outcome::Interrupted);
        assert!(Cond::Outcome(Some(Outcome::Interrupted)).eval(&record));
        assert!(!Cond::Outcome(Some(Outcome::Finished)).eval(&record));
        record.outcome = None;

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
