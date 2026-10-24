//! The daemon's memory on disk: one line per record written, the latest
//! line for a pane winning, so a restarted daemon starts where it stopped.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::Locator;
use crate::state::Record;

#[derive(Debug, Serialize, Deserialize)]
struct Line {
    pane: Locator,
    /// Absent when the pane was forgotten.
    record: Option<Record>,
}

pub struct Journal {
    path: PathBuf,
    file: File,
}

impl Journal {
    /// Open for appending, creating the file if needed.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().append(true).create(true).open(path)?;
        Ok(Journal {
            path: path.to_owned(),
            file,
        })
    }

    pub fn write(&mut self, pane: &Locator, record: Option<&Record>) -> io::Result<()> {
        let line = Line {
            pane: pane.clone(),
            record: record.cloned(),
        };
        let mut text = serde_json::to_string(&line)?;
        text.push('\n');
        self.file.write_all(text.as_bytes())
    }

    /// The records a journal describes; a line that doesn't parse is
    /// skipped, so a torn last line costs nothing but itself.
    pub fn replay(path: &Path) -> io::Result<BTreeMap<Locator, Record>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(error) => return Err(error),
        };
        let mut records = BTreeMap::new();
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            let Ok(entry) = serde_json::from_str::<Line>(&line) else {
                continue;
            };
            match entry.record {
                Some(record) => {
                    records.insert(entry.pane, record);
                }
                None => {
                    records.remove(&entry.pane);
                }
            }
        }
        Ok(records)
    }

    /// Replace the journal with one line per current record, in place.
    pub fn rewrite(path: &Path, records: &BTreeMap<Locator, Record>) -> io::Result<Self> {
        let temporary = path.with_extension("jsonl.new");
        {
            let mut file = File::create(&temporary)?;
            for (pane, record) in records {
                let line = Line {
                    pane: pane.clone(),
                    record: Some(record.clone()),
                };
                let mut text = serde_json::to_string(&line)?;
                text.push('\n');
                file.write_all(text.as_bytes())?;
            }
            file.sync_all()?;
        }
        fs::rename(&temporary, path)?;
        Journal::open(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: &str) -> Record {
        Record {
            state: Some(state.parse().unwrap()),
            since: Some(1),
            ..Record::default()
        }
    }

    #[test]
    fn the_last_line_for_a_pane_wins_and_torn_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut journal = Journal::open(&path).unwrap();
        journal
            .write(&Locator::tmux("%1"), Some(&record("working")))
            .unwrap();
        journal
            .write(&Locator::tmux("%2"), Some(&record("done")))
            .unwrap();
        journal
            .write(&Locator::tmux("%1"), Some(&record("done")))
            .unwrap();
        journal.write(&Locator::tmux("%2"), None).unwrap();
        journal
            .file
            .write_all(b"{\"pane\":{\"backend\":\"tmux\"")
            .unwrap();
        drop(journal);

        let records = Journal::replay(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[&Locator::tmux("%1")], record("done"));

        let rewritten = Journal::rewrite(&path, &records).unwrap();
        drop(rewritten);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert_eq!(Journal::replay(&path).unwrap(), records);
        assert!(
            Journal::replay(&dir.path().join("none"))
                .unwrap()
                .is_empty()
        );
    }
}
