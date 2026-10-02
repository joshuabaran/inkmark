//! Which folder the file browser roots at, from the command line.

use std::path::{Path, PathBuf};

/// What `inkmark [path]` should open. A missing path is a file: the editor
/// treats that as a new file, and the browser roots at its parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// Browse `root` and open `file`.
    File { root: PathBuf, file: PathBuf },
    /// Browse `root` with no file open.
    Folder { root: PathBuf },
}

/// `arg` is the optional command-line path. `cwd` is the process's current
/// directory, used when there is no argument and when a relative path has
/// no parent (`inkmark notes.md`).
pub fn choose_root(arg: Option<&Path>, cwd: &Path) -> Launch {
    let Some(arg) = arg else {
        return Launch::Folder {
            root: cwd.to_path_buf(),
        };
    };
    let path = if arg.is_absolute() {
        arg.to_path_buf()
    } else {
        cwd.join(arg)
    };
    // `is_dir` follows symlinks, so a link to a folder browses that folder.
    // Anything else, including a path that does not exist yet, is a file.
    if path.is_dir() {
        Launch::Folder { root: path }
    } else {
        let root = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => cwd.to_path_buf(),
        };
        Launch::File { root, file: path }
    }
}
