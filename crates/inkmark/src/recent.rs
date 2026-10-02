//! Recently opened files, most recent first, kept in
//! `$XDG_STATE_HOME/inkmark/recent` (default `~/.local/state`).

use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 10;

pub struct Recent {
    /// Where the list is stored; `None` if there's no home to store it in.
    store: Option<PathBuf>,
    entries: Vec<PathBuf>,
}

impl Recent {
    pub fn load() -> Self {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")));
        Self::from_store(state.map(|s| s.join("inkmark/recent")))
    }

    pub(crate) fn from_store(store: Option<PathBuf>) -> Self {
        let entries = store
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| {
                s.lines()
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .take(MAX_ENTRIES)
                    .collect()
            })
            .unwrap_or_default();
        Self { store, entries }
    }

    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// The file the list is stored in, when there is somewhere to put it.
    pub(crate) fn store_path(&self) -> Option<&Path> {
        self.store.as_deref()
    }

    /// Moves `path` to the front and saves. Failures to save are ignored:
    /// the list is a convenience.
    pub fn add(&mut self, path: &Path) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.entries.retain(|p| *p != path);
        self.entries.insert(0, path);
        self.entries.truncate(MAX_ENTRIES);
        let Some(store) = &self.store else { return };
        if let Some(dir) = store.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut text: String = self
            .entries
            .iter()
            .filter_map(|p| p.to_str())
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
        let _ = std::fs::write(store, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_first_deduplicated_capped_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("state/inkmark/recent");
        let files: Vec<PathBuf> = (0..12)
            .map(|i| {
                let f = dir.path().join(format!("{i}.md"));
                std::fs::write(&f, "").unwrap();
                f
            })
            .collect();
        let mut recent = Recent::from_store(Some(store.clone()));
        assert!(recent.entries().is_empty());
        for f in &files {
            recent.add(f);
        }
        recent.add(&files[3]);
        assert_eq!(recent.entries().len(), MAX_ENTRIES);
        assert_eq!(recent.entries()[0], files[3].canonicalize().unwrap());
        assert_eq!(recent.entries()[1], files[11].canonicalize().unwrap());
        // A fresh load sees the same list.
        let reloaded = Recent::from_store(Some(store));
        assert_eq!(reloaded.entries(), recent.entries());
    }
}
