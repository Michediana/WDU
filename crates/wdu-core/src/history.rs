//! Installation history as JSON lines, one record per installation attempt.
//!
//! An append-only text file is enough until the service keeps its state in SQLite.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::provider::Source;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    /// Installed, found broken, and the previous driver restored.
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryRecord {
    /// RFC 3339, UTC.
    pub at: String,
    pub source: Source,
    pub id: String,
    pub title: String,
    /// Instance IDs of the devices the update was meant for.
    pub devices: Vec<String>,
    pub outcome: Outcome,
    pub reboot_required: bool,
    /// Install method on success, error on failure.
    pub detail: Option<String>,
}

impl HistoryRecord {
    pub fn new(source: Source, id: &str, title: &str, devices: Vec<String>) -> Self {
        HistoryRecord {
            at: crate::time::now_rfc3339(),
            source,
            id: id.to_string(),
            title: title.to_string(),
            devices,
            outcome: Outcome::Failed,
            reboot_required: false,
            detail: None,
        }
    }
}

pub struct History {
    path: PathBuf,
}

impl History {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        History { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &HistoryRecord) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("cannot open {}", self.path.display()))?;
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// All records, oldest first; a missing file is an empty history.
    pub fn read(&self) -> Result<Vec<HistoryRecord>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).context(format!("cannot read {}", self.path.display())),
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, l)| {
                serde_json::from_str(l).with_context(|| format!("line {} of history", i + 1))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read_back() {
        let path = std::env::temp_dir().join(format!("wdu-history-{}.jsonl", std::process::id()));
        let _ = fs::remove_file(&path);
        let history = History::new(&path);
        assert!(history.read().unwrap().is_empty());

        let mut record = HistoryRecord::new(Source::WindowsUpdate, "abc", "Intel - Net", vec![]);
        history.append(&record).unwrap();
        record.outcome = Outcome::Succeeded;
        record.reboot_required = true;
        history.append(&record).unwrap();

        let read = history.read().unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[1], record);
        let _ = fs::remove_file(&path);
    }
}
