//! One directory, read once. The tree decides when, and does not recurse.

use std::cmp::Ordering;
use std::fs::{self, DirEntry};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::sort::natural_cmp;

/// Extensions listed without "show all". `.txt` is an open-dialog filter in
/// the app, not a browser entry.
pub const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `unreadable` and `looped` are set when the folder can't be listed.
    /// A loop is a symlink cycle, not a permissions error.
    Dir { unreadable: bool, looped: bool },
    /// Non-Markdown files are `openable: false` and only appear with show-all.
    File { openable: bool },
}

impl Kind {
    pub fn is_dir(self) -> bool {
        matches!(self, Self::Dir { .. })
    }

    pub fn openable(self) -> bool {
        matches!(self, Self::File { openable: true })
    }

    pub fn looped(self) -> bool {
        matches!(self, Self::Dir { looped: true, .. })
    }

    pub fn unreadable(self) -> bool {
        matches!(
            self,
            Self::Dir {
                unreadable: true,
                ..
            }
        )
    }
}

pub fn is_markdown_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            MARKDOWN_EXTENSIONS
                .iter()
                .any(|markdown| ext.eq_ignore_ascii_case(markdown))
        })
}

/// Lists `dir` itself. Dot-files and non-Markdown files are left out unless
/// `show_all`. Folders sort first, then [`natural_cmp`], then the raw name
/// so the order is total when case is the only difference.
///
/// `epoch` / `ticket` let a slow listing stop when the browser has moved on
/// (a new root, or a collapsed folder). Pass `None` to always finish.
pub fn list_dir(
    dir: &Path,
    show_all: bool,
    cancel: Option<(&AtomicU64, u64)>,
) -> io::Result<Vec<Entry>> {
    if cancelled(cancel) {
        return Err(ErrorKind::Interrupted.into());
    }
    let mut entries = Vec::new();
    for (n, ent) in fs::read_dir(dir)?.enumerate() {
        if n % 64 == 0 && cancelled(cancel) {
            return Err(ErrorKind::Interrupted.into());
        }
        let Ok(ent) = ent else { continue };
        if let Some(entry) = classify(&ent, show_all) {
            entries.push(entry);
        }
    }
    entries.sort_by(cmp_entry);
    Ok(entries)
}

fn cancelled(cancel: Option<(&AtomicU64, u64)>) -> bool {
    cancel.is_some_and(|(epoch, ticket)| epoch.load(AtomicOrdering::Acquire) != ticket)
}

fn cmp_entry(a: &Entry, b: &Entry) -> Ordering {
    // false < true, so folders (is_dir) sort first.
    b.kind
        .is_dir()
        .cmp(&a.kind.is_dir())
        .then_with(|| natural_cmp(&a.name, &b.name))
        .then_with(|| a.name.cmp(&b.name))
}

fn classify(ent: &DirEntry, show_all: bool) -> Option<Entry> {
    let name = ent.file_name().to_string_lossy().into_owned();
    if name == "." || name == ".." {
        return None;
    }
    let path = ent.path();
    let dotted = name.starts_with('.');
    match fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => {
            if dotted && !show_all {
                return None;
            }
            Some(Entry {
                name,
                path,
                kind: Kind::Dir {
                    unreadable: false,
                    looped: false,
                },
            })
        }
        Ok(meta) if meta.is_file() => file_entry(name, path, dotted, show_all),
        Ok(_) => other_entry(name, path, show_all),
        Err(error) => match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() && is_symlink_loop(&error) => {
                if dotted && !show_all {
                    return None;
                }
                // A cycle can't be listed; show it so expanding doesn't spin.
                Some(Entry {
                    name,
                    path,
                    kind: Kind::Dir {
                        unreadable: false,
                        looped: true,
                    },
                })
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                // Dangling link: there is no target to classify, so the name
                // decides, the same as a file.
                file_entry(name, path, dotted, show_all)
            }
            Ok(meta) if meta.is_dir() => {
                if dotted && !show_all {
                    return None;
                }
                Some(Entry {
                    name,
                    path,
                    kind: Kind::Dir {
                        unreadable: true,
                        looped: false,
                    },
                })
            }
            Ok(_) => file_entry(name, path, dotted, show_all),
            Err(_) => None,
        },
    }
}

/// `ErrorKind::FilesystemLoop` is still unstable. ELOOP is 40 on Linux and
/// 62 on macOS; both mean the symlink chain closed on itself.
pub(crate) fn is_symlink_loop(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(40 | 62))
}

fn file_entry(name: String, path: PathBuf, dotted: bool, show_all: bool) -> Option<Entry> {
    let openable = is_markdown_name(&name);
    if !show_all && (dotted || !openable) {
        return None;
    }
    Some(Entry {
        name,
        path,
        kind: Kind::File { openable },
    })
}

fn other_entry(name: String, path: PathBuf, show_all: bool) -> Option<Entry> {
    if !show_all {
        return None;
    }
    Some(Entry {
        name,
        path,
        kind: Kind::File { openable: false },
    })
}
