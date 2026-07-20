use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Result of one action execution, recorded for incremental rebuilds.
/// This is the constructive-trace store: an action whose key digest matches
/// its journal entry, and whose recorded outputs are intact on disk, can be
/// skipped without running (which also yields early cutoff for downstream
/// actions, because their keys are computed from output *content* hashes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Action key digest at the time of the recorded run, including
    /// discovered (depfile) inputs.
    pub key: String,
    /// path -> content digest of every input that fed the key.
    pub inputs: BTreeMap<String, String>,
    /// Inputs discovered from the depfile (subset of `inputs` keys).
    pub discovered: Vec<String>,
    /// path -> content digest of every declared output after the run.
    pub outputs: BTreeMap<String, String>,
    pub duration_ms: u64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Journal {
    /// Keyed by stable action id (e.g. `compile:app:src/main.c`).
    pub actions: BTreeMap<String, JournalEntry>,
    /// Reused by the execution engine so a 10k-action build does not
    /// open/close the same append-only file 10k times. Each record is still
    /// flushed before the action is reported complete, preserving crash-tail
    /// recovery.
    #[serde(skip)]
    writer: Option<(PathBuf, std::fs::File)>,
}

pub const JOURNAL_REL_PATH: &str = ".frost/journal.bin";
const LEGACY_JOURNAL_REL_PATH: &str = ".frost/journal.json";
const MAGIC: &[u8; 8] = b"FRSTJR01";

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    id: String,
    entry: JournalEntry,
}

impl Journal {
    pub fn load(workspace_root: &Path) -> Self {
        let path = workspace_root.join(JOURNAL_REL_PATH);
        let mut actions = BTreeMap::new();
        if let Ok(mut file) = std::fs::File::open(&path) {
            let mut bytes = Vec::new();
            if file.read_to_end(&mut bytes).is_ok() {
                actions = decode_bytes(&bytes);
            }
        }
        if actions.is_empty() {
            let legacy = workspace_root.join(LEGACY_JOURNAL_REL_PATH);
            actions = std::fs::read_to_string(&legacy)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();
        }
        Self {
            actions,
            writer: None,
        }
    }

    /// Append one completed action. A torn final frame is ignored on load.
    pub fn record(&mut self, workspace_root: &Path, id: String, entry: JournalEntry) -> Result<()> {
        let path = workspace_root.join(JOURNAL_REL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let reuse = self
            .writer
            .as_ref()
            .is_some_and(|(writer_path, _)| writer_path == &path);
        if !reuse {
            let new_file = !path.exists() || std::fs::metadata(&path)?.len() == 0;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            if new_file {
                file.write_all(MAGIC)?;
            }
            self.writer = Some((path.clone(), file));
        }
        let file = &mut self.writer.as_mut().unwrap().1;
        let payload = postcard::to_allocvec(&Record {
            id: id.clone(),
            entry: entry.clone(),
        })?;
        file.write_all(&(payload.len() as u32).to_le_bytes())?;
        file.write_all(&payload)?;
        file.flush()?;
        self.actions.insert(id, entry);
        Ok(())
    }

    pub fn save(&self, workspace_root: &Path) -> Result<()> {
        let path = workspace_root.join(JOURNAL_REL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("bin.tmp");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(MAGIC)?;
        for (id, entry) in &self.actions {
            let payload = postcard::to_allocvec(&Record {
                id: id.clone(),
                entry: entry.clone(),
            })?;
            file.write_all(&(payload.len() as u32).to_le_bytes())?;
            file.write_all(&payload)?;
        }
        file.flush()?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("failed to persist {}", path.display()))?;
        Ok(())
    }
}

/// Total decoder used by startup and fuzzing. Invalid/torn data yields the
/// prefix of fully validated records, never a panic or false record.
pub fn decode_bytes(bytes: &[u8]) -> BTreeMap<String, JournalEntry> {
    let mut actions = BTreeMap::new();
    if !bytes.starts_with(MAGIC) {
        return actions;
    }
    let mut cursor = MAGIC.len();
    while cursor + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;
        let Some(end) = cursor.checked_add(len) else {
            break;
        };
        if end > bytes.len() {
            break;
        }
        match postcard::from_bytes::<Record>(&bytes[cursor..end]) {
            Ok(record) => {
                actions.insert(record.id, record.entry);
            }
            Err(_) => break,
        }
        cursor = end;
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("frost-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut journal = Journal::default();
        journal.actions.insert(
            "compile:app:src/main.c".into(),
            JournalEntry {
                key: "abc".into(),
                inputs: BTreeMap::from([("src/main.c".into(), "h1".into())]),
                discovered: vec!["include/util.h".into()],
                outputs: BTreeMap::from([(".frost/obj/app/src/main.c.o".into(), "h2".into())]),
                duration_ms: 12,
                reason: "input changed: src/main.c".into(),
            },
        );
        journal.save(&dir).unwrap();

        let loaded = Journal::load(&dir);
        assert_eq!(loaded.actions.len(), 1);
        assert_eq!(loaded.actions["compile:app:src/main.c"].key, "abc");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_or_corrupt_journal_loads_empty() {
        let dir =
            std::env::temp_dir().join(format!("frost-journal-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".frost")).unwrap();
        assert!(Journal::load(&dir).actions.is_empty());
        std::fs::write(dir.join(JOURNAL_REL_PATH), b"FRSTJR01\xff\xff").unwrap();
        assert!(Journal::load(&dir).actions.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incomplete_tail_preserves_completed_records() {
        let dir = std::env::temp_dir().join(format!("frost-journal-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry = JournalEntry {
            key: "k".into(),
            inputs: BTreeMap::new(),
            discovered: Vec::new(),
            outputs: BTreeMap::new(),
            duration_ms: 1,
            reason: "first".into(),
        };
        let mut journal = Journal::default();
        journal.record(&dir, "a".into(), entry).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(JOURNAL_REL_PATH))
            .unwrap();
        file.write_all(&100u32.to_le_bytes()).unwrap();
        file.write_all(b"partial").unwrap();
        assert!(Journal::load(&dir).actions.contains_key("a"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
