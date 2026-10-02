//! Temp-directory coverage for sorting, filtering, lazy expand, symlinks,
//! unreadable folders, and command-line root selection.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use inkmark_files::{Entry, Kind, Launch, Tree, choose_root, list_dir};

fn names(tree: &mut Tree) -> Vec<String> {
    tree.rows()
        .iter()
        .map(|row| format!("{}{}", "  ".repeat(row.depth as usize), row.name))
        .collect()
}

fn row(tree: &mut Tree, name: &str) -> inkmark_files::Row {
    tree.rows()
        .iter()
        .find(|row| row.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no row {name}"))
}

fn touch(path: &Path) {
    fs::write(path, "").unwrap();
}

#[test]
fn folders_sort_first_and_names_sort_naturally_ignoring_case() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["file10.md", "file2.md", "File2.md", "zed.md", "b.md"] {
        touch(&dir.path().join(name));
    }
    fs::create_dir(dir.path().join("dir10")).unwrap();
    fs::create_dir(dir.path().join("dir2")).unwrap();
    touch(&dir.path().join("dir2").join("nested.md"));

    let listed: Vec<_> = list_dir(dir.path(), false, None)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(
        listed,
        vec![
            "dir2".to_string(),
            "dir10".to_string(),
            "b.md".to_string(),
            "File2.md".to_string(),
            "file2.md".to_string(),
            "file10.md".to_string(),
            "zed.md".to_string(),
        ]
    );

    // The nested file is not part of the root listing.
    assert!(!listed.iter().any(|name| name == "nested.md"));

    let mut tree = Tree::new(dir.path());
    assert!(tree.rows().is_empty(), "root children are not read yet");
    assert_eq!(tree.pending().len(), 1);
    tree.load_pending();
    assert_eq!(
        names(&mut tree),
        vec![
            "dir2",
            "dir10",
            "b.md",
            "File2.md",
            "file2.md",
            "file10.md",
            "zed.md"
        ]
    );
    // Still lazy: dir2's child stays unread until dir2 is expanded.
    assert!(!names(&mut tree).iter().any(|name| name.contains("nested")));
    let dir2 = row(&mut tree, "dir2").index;
    tree.expand(dir2);
    assert!(names(&mut tree).iter().any(|name| name == "  nested.md"));
}

#[test]
fn markdown_extensions_dotfiles_and_the_show_all_toggle() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "notes.md",
        "guide.markdown",
        "old.mdown",
        "draft.mkd",
        "README.MD",
        "notes.txt",
        "photo.png",
        ".secret.md",
    ] {
        touch(&dir.path().join(name));
    }
    fs::create_dir(dir.path().join(".hidden")).unwrap();
    touch(&dir.path().join(".hidden").join("x.md"));
    fs::create_dir(dir.path().join("empty")).unwrap();

    let visible: Vec<_> = list_dir(dir.path(), false, None)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(
        visible,
        vec![
            "empty".to_string(),
            "draft.mkd".to_string(),
            "guide.markdown".to_string(),
            "notes.md".to_string(),
            "old.mdown".to_string(),
            "README.MD".to_string(),
        ]
    );

    let all = list_dir(dir.path(), true, None).unwrap();
    let find = |name: &str| -> Entry {
        all.iter()
            .find(|entry| entry.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    assert!(!find("notes.txt").kind.openable());
    assert!(!find("photo.png").kind.openable());
    assert!(find(".secret.md").kind.openable());
    assert!(find(".hidden").kind.is_dir());
    assert!(find("empty").kind.is_dir());
    assert!(find("notes.md").kind.openable());

    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    assert!(!names(&mut tree).iter().any(|name| name.contains("secret")));
    tree.set_show_all(true);
    tree.load_pending();
    let shown = names(&mut tree);
    assert!(shown.iter().any(|name| name == ".secret.md"));
    assert!(shown.iter().any(|name| name == ".hidden"));
    assert!(shown.iter().any(|name| name == "photo.png"));
}

#[test]
fn a_folder_lists_only_when_expanded_and_sees_files_added_before_that() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    assert_eq!(names(&mut tree), vec!["sub".to_string()]);
    touch(&sub.join("late.md"));
    assert_eq!(names(&mut tree), vec!["sub".to_string()]);
    let sub_index = row(&mut tree, "sub").index;
    tree.expand(sub_index);
    assert_eq!(
        names(&mut tree),
        vec!["sub".to_string(), "  late.md".to_string()]
    );
}

#[test]
fn symlinks_list_their_target_but_loops_and_dangling_links_do_not_recurse() {
    let dir = tempfile::tempdir().unwrap();
    let docs = dir.path().join("docs");
    fs::create_dir(&docs).unwrap();
    touch(&docs.join("a.md"));
    symlink("docs", dir.path().join("linkdir")).unwrap();
    symlink("loop", dir.path().join("loop")).unwrap();
    symlink("missing.md", dir.path().join("gone.md")).unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    touch(&sub.join("note.md"));
    // Points at the tree root: expanding it must not walk the tree again.
    symlink("..", sub.join("up")).unwrap();

    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    let loop_row = row(&mut tree, "loop");
    assert!(matches!(loop_row.kind, Kind::Dir { looped: true, .. }));
    tree.expand(loop_row.index);
    assert_eq!(
        names(&mut tree)
            .iter()
            .filter(|name| *name == "loop")
            .count(),
        1
    );

    let gone = row(&mut tree, "gone.md");
    assert!(matches!(gone.kind, Kind::File { openable: true }));

    let link = row(&mut tree, "linkdir").index;
    tree.expand(link);
    assert!(names(&mut tree).iter().any(|name| name == "  a.md"));

    let sub_index = row(&mut tree, "sub").index;
    tree.expand(sub_index);
    let up = row(&mut tree, "up");
    tree.expand(up.index);
    let up = row(&mut tree, "up");
    assert!(matches!(up.kind, Kind::Dir { looped: true, .. }));
    // `sub` is not listed again under `up`.
    assert_eq!(
        names(&mut tree)
            .iter()
            .filter(|name| name.ends_with("sub"))
            .count(),
        1
    );
    assert!(names(&mut tree).iter().any(|name| name == "  note.md"));
}

#[test]
fn an_unreadable_folder_is_marked_and_the_rest_of_the_tree_still_lists() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("ok.md"));
    let secret = dir.path().join("secret");
    fs::create_dir(&secret).unwrap();
    touch(&secret.join("hidden.md"));
    let _restore = ModeGuard(secret.clone());
    let mut perms = fs::metadata(&secret).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&secret, perms).unwrap();
    if fs::read_dir(&secret).is_ok() {
        // Running as root ignores the mode bits; the case can't be asserted.
        return;
    }

    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    assert!(names(&mut tree).iter().any(|name| name == "ok.md"));
    let secret_row = row(&mut tree, "secret");
    tree.expand(secret_row.index);
    let secret_row = row(&mut tree, "secret");
    assert!(matches!(
        secret_row.kind,
        Kind::Dir {
            unreadable: true,
            ..
        }
    ));
    assert!(!names(&mut tree).iter().any(|name| name.contains("hidden")));

    let mut perms = fs::metadata(dir.path()).unwrap().permissions();
    perms.set_mode(0o000);
    let _root_restore = ModeGuard(dir.path().to_path_buf());
    fs::set_permissions(dir.path(), perms).unwrap();
    let mut rooted = Tree::new(dir.path());
    rooted.load_pending();
    assert!(matches!(
        rooted.root_kind(),
        Kind::Dir {
            unreadable: true,
            ..
        }
    ));
    assert!(rooted.rows().is_empty());
}

#[test]
fn the_root_follows_a_file_a_folder_a_missing_path_or_no_argument() {
    let cwd = tempfile::tempdir().unwrap();
    let cwd = cwd.path();
    fs::create_dir(cwd.join("notes")).unwrap();
    touch(&cwd.join("notes.md"));
    touch(&cwd.join("notes").join("a.md"));
    symlink("notes.md", cwd.join("link.md")).unwrap();
    symlink("notes", cwd.join("linkdir")).unwrap();

    assert_eq!(
        choose_root(None, cwd),
        Launch::Folder {
            root: cwd.to_path_buf()
        }
    );
    assert_eq!(
        choose_root(Some(Path::new("notes.md")), cwd),
        Launch::File {
            root: cwd.to_path_buf(),
            file: cwd.join("notes.md"),
        }
    );
    assert_eq!(
        choose_root(Some(&cwd.join("notes.md")), cwd),
        Launch::File {
            root: cwd.to_path_buf(),
            file: cwd.join("notes.md"),
        }
    );
    assert_eq!(
        choose_root(Some(&cwd.join("notes")), cwd),
        Launch::Folder {
            root: cwd.join("notes")
        }
    );
    let missing = cwd.join("notes").join("brand-new.md");
    assert_eq!(
        choose_root(Some(&missing), cwd),
        Launch::File {
            root: cwd.join("notes"),
            file: missing,
        }
    );
    // A link to a file browses the link's parent, not the target's.
    assert_eq!(
        choose_root(Some(&cwd.join("link.md")), cwd),
        Launch::File {
            root: cwd.to_path_buf(),
            file: cwd.join("link.md"),
        }
    );
    assert_eq!(
        choose_root(Some(&cwd.join("linkdir")), cwd),
        Launch::Folder {
            root: cwd.join("linkdir")
        }
    );
    // No argument is the current directory even when a relative file would
    // have used that same directory as its parent.
    assert!(matches!(
        choose_root(None, Path::new("/tmp")),
        Launch::Folder { root } if root == Path::new("/tmp")
    ));
}

#[test]
fn expanding_again_reads_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    touch(&sub.join("a.md"));
    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    let index = row(&mut tree, "sub").index;
    tree.expand(index);
    assert!(names(&mut tree).iter().any(|name| name == "  a.md"));

    tree.collapse(index);
    touch(&sub.join("b.md"));
    tree.expand(index);
    let listed = names(&mut tree);
    assert!(listed.iter().any(|name| name == "  a.md"));
    assert!(listed.iter().any(|name| name == "  b.md"));
}

struct ModeGuard(PathBuf);

impl Drop for ModeGuard {
    fn drop(&mut self) {
        let Ok(meta) = fs::metadata(&self.0) else {
            return;
        };
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(&self.0, perms);
    }
}
