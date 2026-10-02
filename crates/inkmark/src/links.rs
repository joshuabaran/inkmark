//! Where a followed link leads, from its destination as written.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Opened in the default app: `http(s)` and `mailto` only.
    External(String),
    /// A heading in the open document (`#anchor`).
    Anchor(String),
    /// A Markdown file, opened in inkmark, maybe at a heading.
    File {
        path: PathBuf,
        anchor: Option<String>,
    },
    /// Not followed; the reason, for the status bar.
    Refused(String),
}

/// Resolves `dest` (as written in the link). Relative paths are relative to
/// `base`, the open file's folder (or the browsed folder for a new file).
pub fn resolve(dest: &str, base: &Path) -> Target {
    let dest = dest.trim();
    if let Some(anchor) = dest.strip_prefix('#') {
        return Target::Anchor(decode(anchor));
    }
    // `//host/path` is a URL without a scheme, not an absolute path.
    if dest.starts_with("//") {
        return Target::Refused("Links to other hosts aren't opened".into());
    }
    if let Some(scheme) = scheme(dest) {
        return match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" | "mailto" => Target::External(dest.to_owned()),
            "file" => file_url(&dest[scheme.len() + 1..], base),
            _ => Target::Refused(format!("{scheme}: links aren't opened")),
        };
    }
    local(dest, base)
}

/// The part of a `file:` URL after the colon. `file:///p` and
/// `file://localhost/p` are the local `/p` (RFC 8089); `file:/p` too. A
/// file on another host isn't opened.
fn file_url(rest: &str, base: &Path) -> Target {
    let Some(authority_and_path) = rest.strip_prefix("//") else {
        return local(rest, base);
    };
    let (host, path) = match authority_and_path.find('/') {
        Some(i) => authority_and_path.split_at(i),
        None => (authority_and_path, ""),
    };
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) || path.is_empty() {
        return Target::Refused(format!("file: links to {host} aren't opened"));
    }
    local(path, base)
}

fn local(dest: &str, base: &Path) -> Target {
    let (path, anchor) = match dest.split_once('#') {
        Some((path, anchor)) => (path, Some(decode(anchor))),
        None => (dest, None),
    };
    if path.is_empty() {
        return Target::Refused("Empty link".into());
    }
    let path = PathBuf::from(decode(path));
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    if path.is_dir() {
        return Target::Refused(format!("{} is a folder", path.display()));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if !inkmark_files::is_markdown_name(name) {
        return Target::Refused(format!("{name} isn't a Markdown file"));
    }
    Target::File { path, anchor }
}

/// `https` in `https://…`: letters first, then letters, digits, `+-.`,
/// then a colon. A Windows drive (`C:`) or a plain path has none.
fn scheme(dest: &str) -> Option<&str> {
    let (scheme, _) = dest.split_once(':')?;
    let mut chars = scheme.chars();
    (scheme.len() > 1
        && chars.next()?.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some(scheme)
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations_resolve_to_targets() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir(base.join("sub")).unwrap();
        let file = |p: &str, a: Option<&str>| Target::File {
            path: base.join(p),
            anchor: a.map(str::to_owned),
        };
        assert_eq!(resolve("other.md", base), file("other.md", None));
        assert_eq!(
            resolve("sub/my%20note.md#Set-up", base),
            file("sub/my note.md", Some("Set-up"))
        );
        assert_eq!(
            resolve("../up.markdown", base),
            file("../up.markdown", None)
        );
        assert_eq!(
            resolve("/abs/x.md", base),
            Target::File {
                path: "/abs/x.md".into(),
                anchor: None
            }
        );
        assert_eq!(resolve("#intro", base), Target::Anchor("intro".into()));
        assert_eq!(
            resolve("https://x.org/a?b#c", base),
            Target::External("https://x.org/a?b#c".into())
        );
        assert_eq!(
            resolve("mailto:me@x.org", base),
            Target::External("mailto:me@x.org".into())
        );
        assert_eq!(
            resolve("file:///abs/y.md", base),
            Target::File {
                path: "/abs/y.md".into(),
                anchor: None
            }
        );
        // Review of #24: a host in a file: URL, and protocol-relative links.
        assert_eq!(
            resolve("file://localhost/abs/z.md#h", base),
            Target::File {
                path: "/abs/z.md".into(),
                anchor: Some("h".into())
            }
        );
        assert_eq!(
            resolve("file:/abs/w.md", base),
            Target::File {
                path: "/abs/w.md".into(),
                anchor: None
            }
        );
        assert!(matches!(
            resolve("file://example.com/a.md", base),
            Target::Refused(_)
        ));
        assert!(matches!(
            resolve("//example.com/a.md", base),
            Target::Refused(_)
        ));
        assert!(matches!(
            resolve("javascript:alert(1)", base),
            Target::Refused(_)
        ));
        assert!(matches!(resolve("image.png", base), Target::Refused(_)));
        assert!(matches!(resolve("sub", base), Target::Refused(_)));
    }
}
