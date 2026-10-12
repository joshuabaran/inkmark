//! Open tabs to put back next launch, stored beside the layout file in
//! `$XDG_STATE_HOME/inkmark/tabs`.
//!
//! The file is paths, which tab was active, whether a tab was a preview,
//! and a caret and scroll position. It never holds the text of a document.
//! A file that is not this format is ignored. A path that is not a file
//! when the editor opens is skipped by the caller.

use std::path::{Path, PathBuf};

pub(crate) struct StoredTab {
    pub path: PathBuf,
    pub preview: bool,
    /// Byte offset of the caret in the file as it was when we saved.
    pub caret: usize,
    pub scroll_line: usize,
    /// Fraction of the way through `scroll_line`, from 0 to 1.
    pub scroll_frac: f32,
}

pub(crate) struct Session {
    pub active: usize,
    pub tabs: Vec<StoredTab>,
}

pub(crate) fn load(path: &Path) -> Option<Session> {
    let text = std::fs::read_to_string(path).ok()?;
    parse(&text)
}

pub(crate) fn save(path: &Path, session: &Session) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, encode(session))
}

pub(crate) fn encode(session: &Session) -> String {
    let active = if session.tabs.is_empty() {
        0
    } else {
        session.active.min(session.tabs.len() - 1)
    };
    let mut out = format!("1\n{active}\n");
    for tab in &session.tabs {
        let path = tab.path.to_string_lossy();
        let kind = if tab.preview { "preview" } else { "pinned" };
        let millis = frac_millis(tab.scroll_frac);
        out.push_str(&format!(
            "{kind} {} {} {millis} {path}\n",
            tab.caret, tab.scroll_line
        ));
    }
    out
}

/// `None` when `text` is not a session file. An empty tab list is a real
/// session: the window had nothing open.
pub(crate) fn parse(text: &str) -> Option<Session> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    if lines.next()?.trim() != "1" {
        return None;
    }
    let active: usize = lines.next()?.trim().parse().ok()?;
    let mut tabs = Vec::new();
    for line in lines {
        tabs.push(parse_tab(line)?);
    }
    if !tabs.is_empty() && active >= tabs.len() {
        return None;
    }
    Some(Session { active, tabs })
}

fn parse_tab(line: &str) -> Option<StoredTab> {
    let mut rest = line.trim_start();
    let kind = take(&mut rest)?;
    let caret: usize = take(&mut rest)?.parse().ok()?;
    let scroll_line: usize = take(&mut rest)?.parse().ok()?;
    let millis: u32 = take(&mut rest)?.parse().ok()?;
    let path = rest.trim_start();
    if path.is_empty() {
        return None;
    }
    let preview = match kind {
        "preview" => true,
        "pinned" => false,
        _ => return None,
    };
    Some(StoredTab {
        path: PathBuf::from(path),
        preview,
        caret,
        scroll_line,
        scroll_frac: (millis.min(1000) as f32) / 1000.0,
    })
}

fn take<'a>(rest: &mut &'a str) -> Option<&'a str> {
    *rest = rest.trim_start();
    if rest.is_empty() {
        return None;
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let field = &rest[..end];
    *rest = &rest[end..];
    Some(field)
}

fn frac_millis(frac: f32) -> u32 {
    if !frac.is_finite() {
        return 0;
    }
    (frac.clamp(0.0, 1.0) * 1000.0).round() as u32
}

/// One spelling for a path, so a file is stored once. A file that is not
/// there yet keeps its folder's canonical path plus its name.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|dir| dir.join(name))
            .unwrap_or(absolute),
        _ => absolute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(path: &str, preview: bool, caret: usize, line: usize, frac: f32) -> StoredTab {
        StoredTab {
            path: PathBuf::from(path),
            preview,
            caret,
            scroll_line: line,
            scroll_frac: frac,
        }
    }

    #[test]
    fn a_session_round_trips_including_a_path_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs");
        let session = Session {
            active: 1,
            tabs: vec![
                tab("/notes/a.md", false, 12, 3, 0.5),
                tab("/notes/my file.md", true, 0, 0, 0.0),
            ],
        };
        save(&path, &session).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.active, 1);
        assert_eq!(loaded.tabs.len(), 2);
        assert_eq!(loaded.tabs[0].path, PathBuf::from("/notes/a.md"));
        assert!(!loaded.tabs[0].preview);
        assert_eq!(loaded.tabs[0].caret, 12);
        assert_eq!(loaded.tabs[0].scroll_line, 3);
        assert!((loaded.tabs[0].scroll_frac - 0.5).abs() < 0.001);
        assert_eq!(loaded.tabs[1].path, PathBuf::from("/notes/my file.md"));
        assert!(loaded.tabs[1].preview);
    }

    #[test]
    fn an_empty_session_round_trips_and_garbage_is_ignored() {
        let empty = parse("1\n0\n").unwrap();
        assert!(empty.tabs.is_empty());
        assert_eq!(encode(&empty), "1\n0\n");

        assert!(parse("").is_none());
        assert!(parse("nope\n").is_none());
        assert!(parse("1\n").is_none());
        assert!(parse("1\n0\npinned 1 2\n").is_none());
        assert!(parse("1\n1\npinned 0 0 0 /a.md\n").is_none());
        assert!(parse("1\nx\npinned 0 0 0 /a.md\n").is_none());
        assert!(parse("2\n0\n").is_none());
    }
}
