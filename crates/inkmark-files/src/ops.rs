//! Renaming, moving and trashing files and folders from the browser. None
//! of these overwrite anything: an existing name is refused, atomically.

use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, RenameFlags, renameat_with};
use rustix::io::Errno;

use crate::list::is_markdown_name;

#[derive(Debug, PartialEq, Eq)]
pub enum OpError {
    /// No name, or a name that is a path (`a/b`, `..`).
    Invalid,
    /// Something already has the new name.
    Exists(PathBuf),
    /// A folder can't move into itself or a folder inside it.
    IntoItself,
    /// Moving to another filesystem would mean copying; not done.
    OtherFilesystem,
    NotFound,
    Io(String),
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => write!(f, "that isn't a valid name"),
            Self::Exists(path) => write!(
                f,
                "{} already exists",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Self::IntoItself => write!(f, "a folder can't move into itself"),
            Self::OtherFilesystem => write!(f, "moving to another drive isn't supported"),
            Self::NotFound => write!(f, "it no longer exists"),
            Self::Io(message) => f.write_str(message),
        }
    }
}

/// Renames `path` within its folder. A file whose new name has no
/// extension keeps its Markdown extension (`notes.md` renamed to `ideas`
/// becomes `ideas.md`). Returns the new path.
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf, OpError> {
    let name = valid_name(new_name)?;
    let is_dir = path.is_dir();
    let mut file_name = name.to_owned();
    if !is_dir
        && Path::new(name).extension().is_none()
        && let Some(ext) = path.extension().filter(|_| is_markdown_path(path))
    {
        file_name = format!("{name}.{}", ext.to_string_lossy());
    }
    let dir = path.parent().ok_or(OpError::Invalid)?;
    let to = dir.join(file_name);
    if to == path {
        return Ok(to);
    }
    rename_no_replace(path, &to)?;
    Ok(to)
}

/// Moves `path` into the folder `dir`, keeping its name. Moving into the
/// folder it's already in does nothing.
pub fn move_into(path: &Path, dir: &Path) -> Result<PathBuf, OpError> {
    let name = path.file_name().ok_or(OpError::Invalid)?;
    let to = dir.join(name);
    if path.parent() == Some(dir) {
        return Ok(path.to_path_buf());
    }
    if path.is_dir() {
        let from = path.canonicalize().map_err(io_error)?;
        let into = dir.canonicalize().map_err(io_error)?;
        if into.starts_with(&from) {
            return Err(OpError::IntoItself);
        }
    }
    rename_no_replace(path, &to)?;
    Ok(to)
}

/// Where deleted files go. The app uses [`SystemTrash`]; tests use a
/// folder of their own so they never touch the user's trash.
pub trait Trash {
    fn trash(&self, path: &Path) -> io::Result<()>;
}

/// The freedesktop trash (`~/.local/share/Trash`, or a `.Trash-$uid` at
/// the top of another filesystem), restorable from a file manager.
pub struct SystemTrash;

impl Trash for SystemTrash {
    fn trash(&self, path: &Path) -> io::Result<()> {
        trash::delete(path).map_err(io::Error::other)
    }
}

fn valid_name(name: &str) -> Result<&str, OpError> {
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\0']) || name == "." || name == ".." {
        return Err(OpError::Invalid);
    }
    Ok(name)
}

fn is_markdown_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_markdown_name)
}

fn rename_no_replace(from: &Path, to: &Path) -> Result<(), OpError> {
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(OpError::Exists(to.to_path_buf())),
        Err(Errno::XDEV) => Err(OpError::OtherFilesystem),
        Err(Errno::NOENT) => Err(OpError::NotFound),
        // Filesystems without RENAME_NOREPLACE (some network ones): check,
        // then rename. Not atomic, but still never overwrites knowingly.
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Err(OpError::Exists(to.to_path_buf()));
            }
            std::fs::rename(from, to).map_err(io_error)
        }
        Err(errno) => Err(OpError::Io(io::Error::from(errno).to_string())),
    }
}

fn io_error(error: io::Error) -> OpError {
    match error.kind() {
        io::ErrorKind::NotFound => OpError::NotFound,
        _ => OpError::Io(error.to_string()),
    }
}
