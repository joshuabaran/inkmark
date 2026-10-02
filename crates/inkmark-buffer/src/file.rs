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
}

impl DiskStamp {
    pub fn of(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        Ok(Self {
            modified: meta.modified()?,
            len: meta.len(),
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

    let target = match fs::canonicalize(path) {
        Ok(real) => real,
        Err(e) if e.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e),
    };
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = dir.join(format!(
        ".{}.inkmark-{}-{}.tmp",
        name.to_string_lossy(),
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
        fs::rename(&tmp, &target)?;
        File::open(&dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
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
}
