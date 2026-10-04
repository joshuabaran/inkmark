//! Creating one new folder inside another.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum NewFolderError {
    Empty,
    /// The name is a path, not a single folder in `dir`.
    Invalid,
    Exists(PathBuf),
    Io(String),
}

/// Creates an empty folder in `dir`. The name is kept as typed, with
/// surrounding whitespace removed. Any existing directory entry, including
/// a dangling symlink, is left untouched.
pub fn create_new_folder(dir: &Path, name: &str) -> Result<PathBuf, NewFolderError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(NewFolderError::Empty);
    }
    if name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
        return Err(NewFolderError::Invalid);
    }
    let path = dir.join(name);
    match std::fs::create_dir(&path) {
        Ok(()) => Ok(path),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Err(NewFolderError::Exists(path)),
        Err(error) => Err(NewFolderError::Io(error.to_string())),
    }
}
