use std::collections::BTreeMap;
use std::path::Path;

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
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Journal {
    /// Keyed by stable action id (e.g. `compile:app:src/main.c`).
    pub actions: BTreeMap<String, JournalEntry>,
}

const JOURNAL_REL_PATH: &str = ".frost/journal.json";

impl Journal {
    pub fn load(workspace_root: &Path) -> Self {
        // The file stores just the action map, keeping the on-disk format
        // minimal ahead of the binary journal planned in issue #19. A missing
        // or corrupt journal degrades to a full rebuild, never an error.
        let path = workspace_root.join(JOURNAL_REL_PATH);
        let actions: BTreeMap<String, JournalEntry> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { actions }
    }

    pub fn save(&self, workspace_root: &Path) -> Result<()> {
        let path = workspace_root.join(JOURNAL_REL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.actions)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("failed to persist {}", path.display()))?;
        Ok(())
    }
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
        std::fs::write(dir.join(".frost/journal.json"), "{ not json").unwrap();
        assert!(Journal::load(&dir).actions.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
