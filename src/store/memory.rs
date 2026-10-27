//! The daemon's records, in memory. The daemon journals and publishes what
//! changes; this only keeps and changes records.

use std::collections::BTreeMap;

use super::Guarded;
use crate::backend::Locator;
use crate::state::Record;

#[derive(Debug, Default)]
pub struct MemoryStore {
    records: BTreeMap<Locator, Record>,
}

impl MemoryStore {
    pub fn new(records: BTreeMap<Locator, Record>) -> Self {
        MemoryStore { records }
    }

    pub fn records(&self) -> &BTreeMap<Locator, Record> {
        &self.records
    }

    pub fn get(&self, pane: &Locator) -> Option<&Record> {
        self.records.get(pane)
    }

    /// Apply `change` to `pane` at time `now`, returning the record when it
    /// changed. A record left with nothing in it is dropped, and `None` then
    /// means the pane was forgotten.
    pub fn apply(&mut self, pane: &Locator, change: &Guarded, now: u64) -> Option<Option<&Record>> {
        self.edit(pane, |record| change.apply_to(record, now))
    }

    pub fn set_pinned(&mut self, pane: &Locator, pinned: bool) -> Option<Option<&Record>> {
        self.edit(pane, |record| record.pinned = pinned)
    }

    pub fn set_viewed(&mut self, pane: &Locator, at: u64) -> Option<Option<&Record>> {
        self.edit(pane, |record| record.viewed = Some(at))
    }

    /// Forget `pane`, returning whether anything was recorded for it.
    pub fn remove(&mut self, pane: &Locator) -> bool {
        self.records.remove(pane).is_some()
    }

    /// Run `edit` on the pane's record, present or new, and report the
    /// outcome as [`MemoryStore::apply`] does.
    fn edit(&mut self, pane: &Locator, edit: impl FnOnce(&mut Record)) -> Option<Option<&Record>> {
        let before = self.records.get(pane).cloned();
        let mut after = before.clone().unwrap_or_default();
        edit(&mut after);
        if before.as_ref() == Some(&after) || (before.is_none() && after == Record::default()) {
            return None;
        }
        if after == Record::default() {
            self.records.remove(pane);
            return Some(None);
        }
        self.records.insert(pane.clone(), after);
        Some(self.records.get(pane))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::State;
    use crate::store::{Change, Cond, Update};

    #[test]
    fn changes_report_what_they_did() {
        let mut store = MemoryStore::default();
        let pane = Locator::tmux("%1");
        let working = Guarded::always(Change::Set(Update::new(State::known(State::WORKING))));
        assert!(store.apply(&pane, &working, 5).unwrap().is_some());
        // The same state again changes nothing.
        assert!(store.apply(&pane, &working, 6).is_none());
        assert_eq!(store.get(&pane).unwrap().since, Some(5));

        let refused = Guarded::when(
            Change::Set(Update::new(State::known(State::READY))),
            Cond::StateUnder(State::known(State::WAITING)),
        );
        assert!(store.apply(&pane, &refused, 7).is_none());

        assert!(store.set_pinned(&pane, true).unwrap().is_some());
        assert!(store.set_pinned(&pane, true).is_none());
        // Clearing keeps the pin, so the record stays.
        let cleared = store
            .apply(&pane, &Guarded::always(Change::Clear), 8)
            .unwrap();
        assert_eq!(cleared.map(|record| record.pinned), Some(true));
        assert!(store.set_pinned(&pane, false).unwrap().is_none());
        assert!(store.get(&pane).is_none());
        assert!(!store.remove(&pane));
        // Unpinning an unknown pane is not a change.
        assert!(store.set_pinned(&pane, false).is_none());
    }
}
