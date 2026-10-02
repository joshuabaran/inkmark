use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use ropey::Rope;

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Line-ending style written on save. The buffer itself always holds `\n`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }
}

/// On-disk encoding details we preserve across a load/save round trip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Encoding {
    pub line_ending: LineEnding,
    pub bom: bool,
}

#[derive(Debug)]
pub enum OpenError {
    Io(io::Error),
    /// The file isn't UTF-8; `offset` is the first bad byte (after any BOM).
    InvalidUtf8 {
        offset: usize,
    },
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::InvalidUtf8 { offset } => {
                write!(f, "not valid UTF-8 (first bad byte at offset {offset})")
            }
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::InvalidUtf8 { .. } => None,
        }
    }
}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Splits file bytes into LF-normalized text plus the encoding to write back.
///
/// The line ending is the majority style; a mixed file is saved with that
/// style throughout. Lone `\r` is left as text.
pub fn decode(mut bytes: Vec<u8>) -> Result<(String, Encoding), OpenError> {
    let bom = bytes.starts_with(BOM);
    if bom {
        bytes.drain(..BOM.len());
    }
    let text = String::from_utf8(bytes).map_err(|e| OpenError::InvalidUtf8 {
        offset: e.utf8_error().valid_up_to(),
    })?;
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    let line_ending = if crlf > lf {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    let text = if crlf > 0 {
        text.replace("\r\n", "\n")
    } else {
        text
    };
    Ok((text, Encoding { line_ending, bom }))
}

/// Writes `rope` with `encoding` applied.
pub fn encode(rope: &Rope, encoding: Encoding, out: &mut impl Write) -> io::Result<()> {
    if encoding.bom {
        out.write_all(BOM)?;
    }
    let newline = encoding.line_ending.as_str();
    for chunk in rope.chunks() {
        if encoding.line_ending == LineEnding::Lf {
            out.write_all(chunk.as_bytes())?;
            continue;
        }
        let mut lines = chunk.split('\n');
        if let Some(first) = lines.next() {
            out.write_all(first.as_bytes())?;
        }
        for line in lines {
            out.write_all(newline.as_bytes())?;
            out.write_all(line.as_bytes())?;
        }
    }
    Ok(())
}

/// What we last saw on disk, to notice changes made by other programs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskStamp {
    pub modified: SystemTime,
    pub len: u64,
    /// Which file this is (device, inode): replacing it, as an atomic save
    /// by another program does, gives a new inode.
    pub file_id: (u64, u64),
    /// Status-change time. Every write sets it, and unlike mtime it can't
    /// be set back, so a same-length rewrite that restores mtime still
    /// shows up.
    pub changed: (i64, i64),
}

impl DiskStamp {
    pub fn of(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;

        let meta = fs::metadata(path)?;
        Ok(Self {
            modified: meta.modified()?,
            len: meta.len(),
            file_id: (meta.dev(), meta.ino()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskStatus {
    Unchanged,
    Modified,
    Missing,
}

pub fn disk_status(path: &Path, last_seen: DiskStamp) -> io::Result<DiskStatus> {
    match DiskStamp::of(path) {
        Ok(now) if now == last_seen => Ok(DiskStatus::Unchanged),
        Ok(_) => Ok(DiskStatus::Modified),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DiskStatus::Missing),
        Err(e) => Err(e),
    }
}

/// The file a save to `path` should replace: `path` itself, or what its
/// symlinks point to (followed even when the final target doesn't exist
/// yet, so saving through a dangling link creates the target, not a file
/// in place of the link).
fn resolve_target(path: &Path) -> io::Result<PathBuf> {
    let mut at = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&at)?;
                at = match at.parent() {
                    Some(parent) if link.is_relative() => parent.join(link),
                    _ => link,
                };
            }
            Ok(_) => return fs::canonicalize(&at),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(at),
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("too many levels of symbolic links"))
}

/// `.{name}.inkmark-{pid}-{n}.tmp`, with `name` shortened so the whole
/// stays within the 255-byte file name limit.
fn temp_name(name: &std::ffi::OsStr, pid: u32, n: u64) -> std::ffi::OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    const NAME_MAX: usize = 255;
    let suffix = format!(".inkmark-{pid}-{n}.tmp");
    let room = NAME_MAX - 1 - suffix.len();
    let name = name.as_bytes();
    let mut bytes = Vec::with_capacity(NAME_MAX);
    bytes.push(b'.');
    bytes.extend_from_slice(&name[..name.len().min(room)]);
    bytes.extend_from_slice(suffix.as_bytes());
    std::ffi::OsString::from_vec(bytes)
}

/// Replaces `path` with whatever `write` produces, all or nothing.
///
/// Writes a temp file in the same directory, fsyncs it, renames it over the
/// target and fsyncs the directory. Symlinks are followed so the link itself
/// survives, and the original file's permissions are kept.
pub fn write_atomic(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<&File>) -> io::Result<()>,
) -> io::Result<DiskStamp> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let target = resolve_target(path)?;
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = dir.join(temp_name(
        name,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));

    let result = (|| {
        let file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        let mut out = BufWriter::new(&file);
        write(&mut out)?;
        out.flush()?;
        drop(out);
        file.sync_all()?;
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    // The new bytes are in place once the rename succeeds. Syncing the
    // directory makes the rename itself durable; if that can't be done (a
    // directory without read permission can't be opened), the save still
    // happened, so it isn't reported as failed.
    let _ = File::open(&dir).and_then(|d| d.sync_all());
    DiskStamp::of(&target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(bytes: &[u8]) -> Vec<u8> {
        let (text, encoding) = decode(bytes.to_vec()).unwrap();
        let mut out = Vec::new();
        encode(&Rope::from_str(&text), encoding, &mut out).unwrap();
        out
    }

    #[test]
    fn lf_and_crlf_round_trip() {
        assert_eq!(round_trip(b"a\nb\n"), b"a\nb\n");
        assert_eq!(round_trip(b"a\r\nb\r\n"), b"a\r\nb\r\n");
        assert_eq!(round_trip(b"\xEF\xBB\xBF# hi\r\n"), b"\xEF\xBB\xBF# hi\r\n");
        assert_eq!(round_trip(b""), b"");
    }

    #[test]
    fn crlf_is_normalized_in_the_buffer() {
        let (text, encoding) = decode(b"a\r\nb\r\nc".to_vec()).unwrap();
        assert_eq!(text, "a\nb\nc");
        assert_eq!(
            encoding,
            Encoding {
                line_ending: LineEnding::CrLf,
                bom: false
            }
        );
    }

    #[test]
    fn mixed_endings_use_the_majority() {
        assert_eq!(round_trip(b"a\r\nb\r\nc\n"), b"a\r\nb\r\nc\r\n");
        assert_eq!(round_trip(b"a\nb\nc\r\n"), b"a\nb\nc\n");
        // Lone CR is not a line ending we touch.
        assert_eq!(round_trip(b"a\rb\n"), b"a\rb\n");
    }

    #[test]
    fn crlf_split_across_rope_chunks() {
        let text = "line\n".repeat(10_000);
        let mut out = Vec::new();
        let encoding = Encoding {
            line_ending: LineEnding::CrLf,
            bom: false,
        };
        encode(&Rope::from_str(&text), encoding, &mut out).unwrap();
        assert_eq!(out, "line\r\n".repeat(10_000).as_bytes());
    }

    #[test]
    fn invalid_utf8_reports_offset() {
        let err = decode(b"\xEF\xBB\xBFok \xFF".to_vec()).unwrap_err();
        assert!(matches!(err, OpenError::InvalidUtf8 { offset: 3 }));
    }

    #[test]
    fn atomic_write_replaces_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        let stamp = write_atomic(&path, |w| w.write_all(b"new")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(stamp.len, 3);
        // Only the target is left behind, no temp files.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_write_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.md");
        let link = dir.path().join("link.md");
        fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomic(&link, |w| w.write_all(b"new")).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
    }

    #[test]
    fn saving_through_a_dangling_symlink_keeps_the_link() {
        // Regression for #19.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link.md");
        std::os::unix::fs::symlink("real.md", &link).unwrap();
        write_atomic(&link, |w| w.write_all(b"new")).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("real.md")).unwrap(),
            "new"
        );
        // A target in a missing folder fails and leaves the link alone.
        let broken = dir.path().join("broken.md");
        std::os::unix::fs::symlink("nowhere/real.md", &broken).unwrap();
        assert!(write_atomic(&broken, |w| w.write_all(b"x")).is_err());
        assert!(
            fs::symlink_metadata(&broken)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn long_file_names_save() {
        // Regression for #20: the temp name must fit NAME_MAX too.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{}.md", "n".repeat(252)));
        assert_eq!(path.file_name().unwrap().len(), 255);
        write_atomic(&path, |w| w.write_all(b"new")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_write_leaves_original_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        fs::write(&path, "old").unwrap();

        let err = write_atomic(&path, |_| Err(io::Error::other("boom"))).unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn detects_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let stamp = write_atomic(&path, |w| w.write_all(b"v1")).unwrap();
        assert_eq!(disk_status(&path, stamp).unwrap(), DiskStatus::Unchanged);
        fs::write(&path, "version two").unwrap();
        assert_eq!(disk_status(&path, stamp).unwrap(), DiskStatus::Modified);
        fs::remove_file(&path).unwrap();
        assert_eq!(disk_status(&path, stamp).unwrap(), DiskStatus::Missing);
    }

    #[test]
    fn a_rewrite_that_keeps_length_and_mtime_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one\n").unwrap();
        let seen = DiskStamp::of(&path).unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        // Same length, same mtime, different bytes, written in place.
        fs::write(&path, "two\n").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 4);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        assert_eq!(disk_status(&path, seen).unwrap(), DiskStatus::Modified);
    }

    #[test]
    fn a_replaced_file_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one\n").unwrap();
        let seen = DiskStamp::of(&path).unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let other = dir.path().join("b.md");
        fs::write(&other, "two\n").unwrap();
        File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        fs::rename(&other, &path).unwrap();
        assert_eq!(disk_status(&path, seen).unwrap(), DiskStatus::Modified);
    }

    #[test]
    fn a_save_counts_even_when_the_folder_cant_be_synced() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("locked");
        fs::create_dir(&sub).unwrap();
        let path = sub.join("a.md");
        fs::write(&path, "old\n").unwrap();
        // Writable and searchable, but not readable: the folder can't be
        // opened to fsync it.
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o311)).unwrap();
        let result = write_atomic(&path, |out| out.write_all(b"new\n"));
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        let stamp = result.expect("the bytes were saved");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(disk_status(&path, stamp).unwrap(), DiskStatus::Unchanged);
    }
}
