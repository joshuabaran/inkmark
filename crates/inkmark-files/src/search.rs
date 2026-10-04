//! Every Markdown note under a folder. The sidebar lists one directory at a
//! time; a search has to walk the whole tree, and it does that off the UI
//! thread. Symlink cycles are skipped.

use std::collections::HashSet;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::list::{is_markdown_name, is_symlink_loop};
use crate::sort::natural_cmp;

/// A Markdown file the sidebar would treat as openable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteFile {
    /// The file name, as the sidebar shows it.
    pub name: String,
    /// Path relative to the folder root, with `/` between parts.
    pub relative: String,
    pub path: PathBuf,
}

/// Markdown files under `root`. Dot-files and dot-directories are left out
/// unless `show_all`, matching the sidebar. A symlink cycle is walked once.
///
/// `cancel` stops the walk when the search has moved on. Pass `None` to
/// always finish.
pub fn walk_notes(
    root: &Path,
    show_all: bool,
    cancel: Option<(&AtomicU64, u64)>,
) -> io::Result<Vec<NoteFile>> {
    let mut notes = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(root.canonicalize()?);
    let mut stack = vec![Dir {
        path: root.to_path_buf(),
        relative: String::new(),
    }];
    let mut scanned = 0usize;
    while let Some(dir) = stack.pop() {
        if cancelled(cancel) {
            return Err(ErrorKind::Interrupted.into());
        }
        let entries = match fs::read_dir(&dir.path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for ent in entries {
            scanned += 1;
            if scanned.is_multiple_of(64) && cancelled(cancel) {
                return Err(ErrorKind::Interrupted.into());
            }
            let Ok(ent) = ent else { continue };
            let name = ent.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." || (name.starts_with('.') && !show_all) {
                continue;
            }
            let path = ent.path();
            let meta = match fs::metadata(&path) {
                Ok(meta) => meta,
                Err(error) if is_symlink_loop(&error) => continue,
                Err(_) => continue,
            };
            if meta.is_dir() {
                let canon = match path.canonicalize() {
                    Ok(canon) => canon,
                    Err(error) if is_symlink_loop(&error) => continue,
                    Err(_) => continue,
                };
                if !seen.insert(canon) {
                    continue;
                }
                stack.push(Dir {
                    path,
                    relative: join_rel(&dir.relative, &name),
                });
            } else if meta.is_file() && is_markdown_name(&name) {
                notes.push(NoteFile {
                    relative: join_rel(&dir.relative, &name),
                    name,
                    path,
                });
            }
        }
    }
    notes.sort_by(|a, b| {
        natural_cmp(&a.relative, &b.relative).then_with(|| a.relative.cmp(&b.relative))
    });
    // Two links to one file are one note. The first path in sort order stays.
    let mut files = HashSet::new();
    notes.retain(|note| match note.path.canonicalize() {
        Ok(canon) => files.insert(canon),
        Err(_) => false,
    });
    Ok(notes)
}

struct Dir {
    path: PathBuf,
    relative: String,
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn cancelled(cancel: Option<(&AtomicU64, u64)>) -> bool {
    cancel.is_some_and(|(epoch, ticket)| epoch.load(AtomicOrdering::Acquire) != ticket)
}
