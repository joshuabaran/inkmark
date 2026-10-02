//! Lazy folder tree. A directory's children are read the first time it is
//! expanded; nothing walks the tree ahead of that.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::list::{self, Entry, Kind, list_dir};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Index into the tree. Stable until that node is dropped by a reload.
    pub index: usize,
    pub depth: u32,
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub expanded: bool,
}

/// A directory the UI should list off the UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub index: usize,
    pub generation: u64,
    pub path: PathBuf,
    pub show_all: bool,
}

struct Node {
    parent: Option<usize>,
    name: String,
    path: PathBuf,
    kind: Kind,
    /// Canonical path, remembered so a later symlink can see the cycle.
    canon: Option<PathBuf>,
    expanded: bool,
    loaded: bool,
    /// Bumped when the listing is invalidated, so a late result is dropped.
    generation: u64,
    alive: bool,
    children: Vec<usize>,
}

pub struct Tree {
    root: PathBuf,
    show_all: bool,
    nodes: Vec<Node>,
    /// Dead slots, reused so a folder that keeps changing does not grow the
    /// table by one entry per reload.
    free: Vec<usize>,
    rows: Vec<Row>,
    rows_dirty: bool,
    /// Open file whose parents should expand as listings arrive.
    reveal: Option<PathBuf>,
}

impl Tree {
    /// Root starts expanded, but its children are not read yet.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let node = Node {
            parent: None,
            name: root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string()),
            path: root.clone(),
            kind: Kind::Dir {
                unreadable: false,
                looped: false,
            },
            canon: None,
            expanded: true,
            loaded: false,
            generation: 1,
            alive: true,
            children: Vec::new(),
        };
        Self {
            root,
            show_all: false,
            nodes: vec![node],
            free: Vec::new(),
            rows: Vec::new(),
            rows_dirty: true,
            reveal: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Slots in the node table, including ones waiting to be reused.
    /// Reloading a folder does not grow this past the largest listing seen.
    pub fn stored_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn root_name(&self) -> &str {
        &self.nodes[0].name
    }

    pub fn root_kind(&self) -> Kind {
        self.nodes[0].kind
    }

    pub fn show_all(&self) -> bool {
        self.show_all
    }

    /// Changing the filter drops every listing. Expanded folders are read again.
    pub fn set_show_all(&mut self, show_all: bool) {
        if self.show_all == show_all {
            return;
        }
        self.show_all = show_all;
        let loaded: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.alive && node.loaded)
            .map(|(index, _)| index)
            .collect();
        for index in loaded {
            self.invalidate(index);
        }
    }

    pub fn set_root(&mut self, root: PathBuf) {
        *self = Self::new(root);
    }

    /// Parent of the root, if it has one. `/` does not.
    pub fn parent_root(&self) -> Option<PathBuf> {
        self.root.parent().map(Path::to_path_buf)
    }

    pub fn rows(&mut self) -> &[Row] {
        if self.rows_dirty {
            self.rebuild_rows();
        }
        &self.rows
    }

    pub fn pending(&self) -> Vec<Pending> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                node.alive && node.expanded && !node.loaded && node.kind.is_dir() && !node.looped()
            })
            .map(|(index, node)| Pending {
                index,
                generation: node.generation,
                path: node.path.clone(),
                show_all: self.show_all,
            })
            .collect()
    }

    /// Expanded directories, the set a watcher should follow.
    pub fn expanded_dirs(&self) -> Vec<PathBuf> {
        self.nodes
            .iter()
            .filter(|node| node.alive && node.expanded && node.kind.is_dir() && !node.looped())
            .map(|node| node.path.clone())
            .collect()
    }

    /// `true` when this directory's canonical path is already an ancestor,
    /// or the symlink chain loops on itself.
    pub fn is_loop(&mut self, index: usize) -> bool {
        let Some(node) = self.nodes.get(index) else {
            return false;
        };
        if !node.alive || node.looped() {
            return node.looped();
        }
        let path = node.path.clone();
        match path.canonicalize() {
            Ok(canon) => {
                let looped = self.ancestor_canons(index).iter().any(|a| a == &canon);
                if !looped {
                    self.nodes[index].canon = Some(canon);
                }
                looped
            }
            Err(error) if list::is_symlink_loop(&error) => true,
            Err(_) => false,
        }
    }

    pub fn mark_loop(&mut self, index: usize, generation: u64) {
        self.finish(index, generation, Listing::Loop);
    }

    /// Applies a listing started for `(index, generation)`. A stale generation
    /// (the folder was collapsed and re-listed, or the root changed) is ignored.
    /// `Interrupted` is a cancelled listing, not an unreadable folder.
    pub fn apply(&mut self, index: usize, generation: u64, result: io::Result<Vec<Entry>>) {
        let listing = match result {
            Ok(entries) => Listing::Entries(entries),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return,
            Err(_) => Listing::Unreadable,
        };
        self.finish(index, generation, listing);
    }

    /// Marks one directory stale so the next load reads the disk again.
    /// Children stay until that listing is merged, so an expanded subfolder
    /// stays expanded when a sibling is created, renamed or deleted.
    pub fn invalidate(&mut self, index: usize) {
        if !self.nodes.get(index).is_some_and(|node| node.alive) {
            return;
        }
        self.nodes[index].loaded = false;
        self.nodes[index].generation += 1;
        self.rows_dirty = true;
        self.nudge_reveal();
    }

    /// Every alive directory node with this path (a symlink can show it twice).
    pub fn dirs_at(&self, path: &Path) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.alive && node.kind.is_dir() && node.path == path)
            .map(|(index, _)| index)
            .collect()
    }

    pub fn set_expanded(&mut self, index: usize, expanded: bool) {
        let Some(node) = self.nodes.get(index) else {
            return;
        };
        if !node.alive || !node.kind.is_dir() {
            return;
        }
        // A collapsed folder is not watched, so a cached listing may be stale.
        let reload = expanded && !node.expanded && node.loaded;
        self.nodes[index].expanded = expanded;
        self.rows_dirty = true;
        if expanded {
            self.nudge_reveal();
        }
        if reload {
            self.invalidate(index);
        }
    }

    /// Expands and lists on this thread. The sidebar lists off-thread instead.
    pub fn expand(&mut self, index: usize) {
        self.set_expanded(index, true);
        if self
            .nodes
            .get(index)
            .is_some_and(|node| node.alive && !node.loaded)
        {
            self.load_sync(index);
        }
    }

    pub fn collapse(&mut self, index: usize) {
        self.set_expanded(index, false);
    }

    /// Reads every expanded-but-unloaded directory on this thread.
    pub fn load_pending(&mut self) {
        loop {
            let pending = self.pending();
            if pending.is_empty() {
                break;
            }
            for job in pending {
                self.load_sync(job.index);
            }
        }
    }

    /// Expands ancestors of `file` as soon as their listings are in, so the
    /// open file can be shown without a walk ahead of time.
    pub fn reveal(&mut self, file: &Path) {
        self.reveal = Some(file.to_path_buf());
        self.nodes[0].expanded = true;
        self.nudge_reveal();
        self.rows_dirty = true;
    }

    pub fn reveal_target(&self) -> Option<&Path> {
        self.reveal.as_deref()
    }

    fn load_sync(&mut self, index: usize) {
        let Some(node) = self.nodes.get(index) else {
            return;
        };
        if !node.alive || node.loaded || !node.kind.is_dir() {
            return;
        }
        let generation = node.generation;
        if node.looped() || self.is_loop(index) {
            self.finish(index, generation, Listing::Loop);
            return;
        }
        let path = self.nodes[index].path.clone();
        let show_all = self.show_all;
        let listed = list_dir(&path, show_all, None);
        self.apply(index, generation, listed);
    }

    fn finish(&mut self, index: usize, generation: u64, listing: Listing) {
        let Some(node) = self.nodes.get(index) else {
            return;
        };
        if !node.alive || node.generation != generation {
            return;
        }
        match listing {
            Listing::Entries(entries) => {
                self.nodes[index].kind = Kind::Dir {
                    unreadable: false,
                    looped: false,
                };
                self.adopt(index, entries);
            }
            Listing::Unreadable => {
                self.drop_children(index);
                self.nodes[index].kind = Kind::Dir {
                    unreadable: true,
                    looped: false,
                };
            }
            Listing::Loop => {
                self.drop_children(index);
                self.nodes[index].kind = Kind::Dir {
                    unreadable: false,
                    looped: true,
                };
            }
        }
        self.nodes[index].loaded = true;
        self.rows_dirty = true;
        self.nudge_reveal();
    }

    /// Keeps a child whose path is still in the folder, in the new order.
    /// A file that became a directory (or the reverse) is a different node.
    fn adopt(&mut self, index: usize, entries: Vec<Entry>) {
        let old = std::mem::take(&mut self.nodes[index].children);
        let mut by_path = HashMap::with_capacity(old.len());
        for child in old {
            if self.nodes[child].alive {
                by_path.insert(self.nodes[child].path.clone(), child);
            }
        }
        // Drop names that left before allocating the new ones, so those slots
        // are reused in this same listing instead of one reload later.
        let mut plan = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(child) = by_path.remove(&entry.path) {
                if self.nodes[child].kind.is_dir() == entry.kind.is_dir() {
                    self.reuse(child, entry);
                    plan.push(Ok(child));
                    continue;
                }
                self.kill(child);
            }
            plan.push(Err(entry));
        }
        for child in by_path.into_values() {
            self.kill(child);
        }
        self.nodes[index].children = plan
            .into_iter()
            .map(|item| match item {
                Ok(child) => child,
                Err(entry) => self.alloc(index, entry),
            })
            .collect();
    }

    fn reuse(&mut self, index: usize, entry: Entry) {
        self.nodes[index].name = entry.name;
        match (self.nodes[index].kind, entry.kind) {
            (Kind::File { .. }, kind @ Kind::File { .. }) => {
                self.nodes[index].kind = kind;
            }
            (
                Kind::Dir { looped, unreadable },
                Kind::Dir {
                    looped: now_looped,
                    unreadable: now_unreadable,
                },
            ) => {
                // The parent listing cannot see an ancestor cycle, so a loop
                // found while expanding has to stay. A folder that is now a
                // self-loop or unreadable loses the children it had.
                if now_looped || now_unreadable {
                    self.nodes[index].kind = entry.kind;
                    self.drop_children(index);
                    self.nodes[index].loaded = true;
                } else if unreadable && !looped {
                    self.nodes[index].kind = entry.kind;
                    self.nodes[index].loaded = false;
                    self.nodes[index].generation += 1;
                }
            }
            _ => {}
        }
    }

    fn alloc(&mut self, parent: usize, entry: Entry) -> usize {
        let mut node = Node {
            parent: Some(parent),
            name: entry.name,
            path: entry.path,
            kind: entry.kind,
            canon: None,
            expanded: false,
            loaded: false,
            generation: 1,
            alive: true,
            children: Vec::new(),
        };
        if let Some(index) = self.free.pop() {
            // The slot's generation was bumped when its last occupant died.
            // Keep that, so a listing still in flight for the old file cannot
            // apply to this one.
            node.generation = self.nodes[index].generation;
            self.nodes[index] = node;
            index
        } else {
            let index = self.nodes.len();
            self.nodes.push(node);
            index
        }
    }

    fn drop_children(&mut self, index: usize) {
        let kids = std::mem::take(&mut self.nodes[index].children);
        for child in kids {
            self.kill(child);
        }
    }

    fn kill(&mut self, index: usize) {
        if !self.nodes[index].alive {
            return;
        }
        self.nodes[index].alive = false;
        self.nodes[index].generation += 1;
        self.drop_children(index);
        self.free.push(index);
    }

    fn ancestor_canons(&mut self, index: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut current = self.nodes[index].parent;
        while let Some(ancestor) = current {
            if self.nodes[ancestor].canon.is_none()
                && let Ok(canon) = self.nodes[ancestor].path.canonicalize()
            {
                self.nodes[ancestor].canon = Some(canon);
            }
            if let Some(canon) = &self.nodes[ancestor].canon {
                out.push(canon.clone());
            }
            current = self.nodes[ancestor].parent;
        }
        out
    }

    fn nudge_reveal(&mut self) {
        let Some(target) = self.reveal.clone() else {
            return;
        };
        let Ok(relative) = target.strip_prefix(&self.root) else {
            return;
        };
        let mut index = 0usize;
        for component in relative.components() {
            if !self.nodes[index].loaded {
                return;
            }
            let Some(name) = component.as_os_str().to_str() else {
                return;
            };
            let children = self.nodes[index].children.clone();
            let Some(child) = children
                .into_iter()
                .find(|&child| self.nodes[child].alive && self.nodes[child].name == name)
            else {
                return;
            };
            if self.nodes[child].path == target {
                return;
            }
            if !self.nodes[child].kind.is_dir() || self.nodes[child].looped() {
                return;
            }
            self.nodes[child].expanded = true;
            self.rows_dirty = true;
            index = child;
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let children = self.nodes[0].children.clone();
        if self.nodes[0].alive && self.nodes[0].expanded {
            for child in children {
                self.walk(child, 0);
            }
        }
        self.rows_dirty = false;
    }

    fn walk(&mut self, index: usize, depth: u32) {
        if !self.nodes[index].alive {
            return;
        }
        self.rows.push(Row {
            index,
            depth,
            name: self.nodes[index].name.clone(),
            path: self.nodes[index].path.clone(),
            kind: self.nodes[index].kind,
            expanded: self.nodes[index].expanded,
        });
        if self.nodes[index].expanded {
            let children = self.nodes[index].children.clone();
            for child in children {
                self.walk(child, depth + 1);
            }
        }
    }
}

enum Listing {
    Entries(Vec<Entry>),
    Unreadable,
    Loop,
}

impl Node {
    fn looped(&self) -> bool {
        self.kind.looped()
    }
}
