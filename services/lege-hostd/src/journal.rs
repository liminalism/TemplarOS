//! The on-disk transaction journal (ADR-0003: "resume mid-apt after a restart"). Every phase
//! transition is fsynced to `<state-dir>/transactions/<id>.json` before the in-memory state
//! changes, via write-to-temp-then-rename so a crash mid-write never leaves a half-written file
//! for the next startup to trip over.

use crate::updates::Transaction;
use anyhow::{Context, Result};
use std::{fs, io::Write, path::PathBuf};

#[derive(Debug, Clone)]
pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating journal dir {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Durably persist `txn`. Called after every [`Transaction::advance`] before the caller acts
    /// on the new phase, so a crash can never leave the journal behind reality.
    pub fn save(&self, txn: &Transaction) -> Result<()> {
        let path = self.path_for(&txn.id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(txn)?;
        {
            let mut file =
                fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            file.write_all(&json)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Option<Transaction>> {
        let path = self.path_for(id);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Every transaction recorded, oldest first by id (transaction ids are time-ordered).
    pub fn list(&self) -> Result<Vec<Transaction>> {
        let mut transactions = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let bytes = fs::read(&path)?;
                transactions.push(serde_json::from_slice(&bytes)?);
            }
        }
        transactions.sort_by(|a: &Transaction, b: &Transaction| a.id.cmp(&b.id));
        Ok(transactions)
    }

    /// The transaction that was in flight when the process last stopped, if any — what a restart
    /// must resume. At most one should ever be non-terminal (`plan` refuses a second while one is
    /// in flight), but if the journal somehow holds more than one, the most recently touched wins
    /// and the rest are surfaced as a journal inconsistency rather than silently ignored.
    pub fn unresolved(&self) -> Result<Option<Transaction>> {
        let mut candidates: Vec<Transaction> = self
            .list()?
            .into_iter()
            .filter(|t| !t.phase.is_terminal())
            .collect();
        if candidates.len() > 1 {
            tracing::warn!(
                count = candidates.len(),
                "journal has more than one unresolved transaction; resuming the most recent"
            );
        }
        candidates.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(candidates.pop())
    }

    #[cfg(test)]
    fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updates::{ChangeSet, Phase};

    #[test]
    fn save_load_roundtrip_and_atomic_rename() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path().join("transactions")).unwrap();
        let mut txn = Transaction::new("t1", ChangeSet::default());
        txn.advance(Phase::Downloading, "go").unwrap();
        journal.save(&txn).unwrap();

        // No stray .tmp file left behind.
        let entries: Vec<_> = fs::read_dir(journal.dir()).unwrap().collect();
        assert_eq!(entries.len(), 1);

        let loaded = journal.load("t1").unwrap().unwrap();
        assert_eq!(loaded, txn);
        assert!(journal.load("missing").unwrap().is_none());
    }

    #[test]
    fn unresolved_finds_the_non_terminal_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path()).unwrap();

        let mut done = Transaction::new("a-done", ChangeSet::default());
        done.advance(Phase::Downloading, "").unwrap();
        done.advance(Phase::Failed, "mirror down").unwrap();
        journal.save(&done).unwrap();

        let mut mid_flight = Transaction::new("b-inflight", ChangeSet::default());
        mid_flight.advance(Phase::Downloading, "").unwrap();
        mid_flight.advance(Phase::Staging, "into snapshot").unwrap();
        journal.save(&mid_flight).unwrap();

        let resumed = journal.unresolved().unwrap().unwrap();
        assert_eq!(resumed.id, "b-inflight");
        assert_eq!(resumed.phase, Phase::Staging);
    }

    #[test]
    fn no_unresolved_when_everything_is_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path()).unwrap();
        let mut txn = Transaction::new("t1", ChangeSet::default());
        txn.advance(Phase::Downloading, "").unwrap();
        txn.advance(Phase::Failed, "").unwrap();
        journal.save(&txn).unwrap();
        assert!(journal.unresolved().unwrap().is_none());
    }
}
