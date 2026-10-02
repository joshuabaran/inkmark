//! inotify watches for the folders the tree currently has open. Collapsed
//! folders are not watched: a large tree would hit the kernel watch limit.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use notify::event::EventKind;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

pub struct Watch {
    watcher: RecommendedWatcher,
    rx: Receiver<Result<Event, notify::Error>>,
    dirs: HashSet<PathBuf>,
}

impl Watch {
    /// `wake` runs on the watch thread when something changes, so the UI can
    /// repaint without polling on a timer.
    pub fn new(wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let watcher = RecommendedWatcher::new(
            move |result| {
                let _ = tx.send(result);
                wake();
            },
            notify::Config::default(),
        )
        .map_err(std::io::Error::other)?;
        Ok(Self {
            watcher,
            rx,
            dirs: HashSet::new(),
        })
    }

    /// Watches exactly `dirs`, non-recursively. Extra watches are dropped and
    /// new ones added. A failure to watch one folder leaves the others alone.
    pub fn sync(&mut self, dirs: &[PathBuf]) {
        let next: HashSet<PathBuf> = dirs.iter().cloned().collect();
        for dir in self.dirs.difference(&next) {
            let _ = self.watcher.unwatch(dir);
        }
        for dir in next.difference(&self.dirs) {
            let _ = self.watcher.watch(dir, RecursiveMode::NonRecursive);
        }
        self.dirs = next;
    }

    /// Watched directories that gained, lost or renamed an entry since the
    /// last call. Content reads are ignored, so listing a folder doesn't
    /// schedule another listing.
    pub fn changed(&mut self) -> Vec<PathBuf> {
        let mut out = HashSet::new();
        while let Ok(event) = self.rx.try_recv() {
            let Ok(event) = event else { continue };
            if matches!(event.kind, EventKind::Access(_) | EventKind::Other) {
                continue;
            }
            for path in &event.paths {
                if let Some(dir) = self.watched_dir(path) {
                    out.insert(dir);
                }
            }
        }
        out.into_iter().collect()
    }

    fn watched_dir(&self, path: &Path) -> Option<PathBuf> {
        if self.dirs.contains(path) {
            return Some(path.to_path_buf());
        }
        path.parent()
            .filter(|parent| self.dirs.contains(*parent))
            .map(Path::to_path_buf)
    }
}
