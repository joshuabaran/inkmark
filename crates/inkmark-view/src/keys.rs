//! Every configurable shortcut, and the chord it uses unless config says
//! otherwise. One builtin table is the only list of action, chord, group,
//! and description. [`KeyMap`] copies those rows so config can rebind a chord,
//! input handling reads the rows, and the shortcut overlay draws them.
//!
//! Plain editing keys are not actions: arrows, Backspace, Delete, Enter,
//! Tab, Home, End, Page Up/Down, and Shift held to extend a selection.
//! Their Ctrl and Alt chords are actions, so they can be rebound. Mouse
//! gestures (Ctrl+click) and IME are not actions either. The overlay still
//! lists those gestures, beside the group they belong to.
//!
//! Matching requires Shift and Alt to be exactly the ones named. egui's
//! `consume_shortcut` uses `matches_logically`, which treats extra Shift
//! or Alt as a hit, so Alt+Shift+Left would fire Back (Alt+Left) and
//! Ctrl+Shift+E would fire the mode cycle (Ctrl+E). `matches_exact` keeps
//! Shift and Alt strict and still treats Linux Ctrl (the event sets both
//! `ctrl` and `command`) as the Ctrl chord. Comparing modifiers with `==`
//! would miss those real Ctrl events, which tests build as `command` only.

use egui::{Key, Modifiers};

/// Where a shortcut is listed in the overlay. The order is the overlay's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// Opening, saving, and the sidebar's file commands.
    File,
    /// Switching, closing, and reopening tabs.
    Tabs,
    /// Editing text, formatting, and tables.
    Edit,
    /// Find, replace, and folder search.
    Find,
    /// Panes, the sidebar, the outline, and this overlay.
    View,
    /// Moving through the note and through followed links.
    Navigation,
    /// The minimap, and pointer gestures that follow a link.
    Minimap,
}

impl Group {
    pub const ALL: [Group; 7] = [
        Group::File,
        Group::Tabs,
        Group::Edit,
        Group::Find,
        Group::View,
        Group::Navigation,
        Group::Minimap,
    ];

    /// The heading drawn above this group's rows.
    pub fn label(self) -> &'static str {
        match self {
            Group::File => "File",
            Group::Tabs => "Tabs",
            Group::Edit => "Edit",
            Group::Find => "Find",
            Group::View => "View/Layout",
            Group::Navigation => "Navigation",
            Group::Minimap => "Minimap/Mouse",
        }
    }
}

/// A pointer gesture. It is not a chord, so config cannot rebind it and
/// key handling does not read it. The overlay lists it with the chords.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gesture {
    pub group: Group,
    pub keys: &'static str,
    pub description: &'static str,
}

impl Gesture {
    /// Whether the overlay keeps this gesture for `query`.
    ///
    /// `Ctrl+click` also matches `Cmd+click`, the name macOS shows.
    pub fn matches_query(self, query: &str) -> bool {
        self.matches_lower(&query.trim().to_lowercase())
    }

    /// `query_lower` is already trimmed and lowercased. An empty query keeps
    /// every gesture. The haystack is built once, not per frame.
    pub fn matches_lower(self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        gesture_row(self).haystack.contains(query_lower)
    }

    /// How this machine names the gesture. Linux and Windows keep Ctrl.
    /// macOS says Cmd. A gesture with no Ctrl is unchanged.
    pub fn platform_keys(self, os: egui::os::OperatingSystem) -> String {
        let row = gesture_row(self);
        if os.is_mac() {
            row.mac.clone()
        } else {
            row.other.clone()
        }
    }
}

struct CachedGesture {
    gesture: Gesture,
    haystack: String,
    mac: String,
    other: String,
}

fn gesture_row(gesture: Gesture) -> &'static CachedGesture {
    cached_gestures()
        .iter()
        .find(|row| row.gesture == gesture)
        .expect("gesture missing from the overlay list")
}

fn cached_gestures() -> &'static [CachedGesture] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<CachedGesture>> = OnceLock::new();
    CACHE.get_or_init(|| {
        GESTURES
            .iter()
            .copied()
            .map(|gesture| {
                let mac = gesture.keys.replace("Ctrl", "Cmd");
                let other = gesture.keys.to_owned();
                let haystack =
                    format!("{} {mac} {}", gesture.keys, gesture.description).to_lowercase();
                CachedGesture {
                    gesture,
                    haystack,
                    mac,
                    other,
                }
            })
            .collect()
    })
}

/// One action in the shortcut table. `chords` is empty when it is unbound.
///
/// Input handling and the overlay both read these rows from [`KeyMap`].
/// The rows are copied from the builtin table; config replaces `chords` and
/// leaves the group, scope, and description as that table wrote them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub action: Action,
    pub group: Group,
    pub scope: Scope,
    pub description: &'static str,
    pub chords: Vec<Chord>,
}

/// Pointer gestures, in overlay order within their groups.
///
/// A gesture is not a chord, so config cannot rebind it and key handling
/// does not match it. The overlay lists it beside the chords in its group.
/// Folder search's middle click is the pointer equivalent of
/// [`Action::PinSearchResult`].
pub const GESTURES: &[Gesture] = &[
    Gesture {
        group: Group::Tabs,
        keys: "Middle click a tab",
        description: "Close the tab",
    },
    Gesture {
        group: Group::Find,
        keys: "Middle click a search result",
        description: "Open the search result in a pinned tab",
    },
    Gesture {
        group: Group::Minimap,
        keys: "Ctrl+click",
        description: "Follow a link, in a preview tab",
    },
    Gesture {
        group: Group::Minimap,
        keys: "Middle click a link",
        description: "Follow a link, in a pinned tab",
    },
];

/// Where an action is handled.
///
/// A chord belongs to one action among the scopes that can see the key at
/// the same time. [`Scope::Search`] runs only while the folder-search field
/// is focused, so it may share a chord with an editor or table action.
/// An app chord is taken before any of those, and the sidebar can be
/// focused while a pane is not, so those stay exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The window: consumed before a pane sees the key.
    App,
    /// The sidebar, and only while it has focus.
    Browser,
    /// Either editor pane, while it has focus.
    Editor,
    /// Either editor pane, but only inside a table. Outside one, the key
    /// does what it otherwise would (Alt+Shift+Left still extends a
    /// selection).
    Table,
    /// The folder-search field, and only while that field is focused.
    Search,
}

/// One thing a chord can do. The order here is the order of `--list-keys`
/// and the README table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum Action {
    CycleMode = 0,
    FocusCode = 1,
    FocusLive = 2,
    ToggleMinimap = 3,
    OpenFile = 4,
    Save = 5,
    SaveAs = 6,
    OpenFolder = 7,
    ToggleSidebar = 8,
    NewFile = 9,
    RecentFiles = 10,
    Back = 11,
    Forward = 12,
    Rename = 13,
    MoveToTrash = 14,
    Undo = 15,
    Redo = 16,
    Bold = 17,
    Italic = 18,
    Code = 19,
    Strikethrough = 20,
    Link = 21,
    ToggleTask = 22,
    Heading0 = 23,
    Heading1 = 24,
    Heading2 = 25,
    Heading3 = 26,
    Heading4 = 27,
    Heading5 = 28,
    Heading6 = 29,
    SelectAll = 30,
    WordLeft = 31,
    WordRight = 32,
    DeleteWordLeft = 33,
    DeleteWordRight = 34,
    DocumentStart = 35,
    DocumentEnd = 36,
    InsertRowAbove = 37,
    InsertRowBelow = 38,
    InsertColumnLeft = 39,
    InsertColumnRight = 40,
    DeleteRow = 41,
    DeleteColumn = 42,
    MoveRowUp = 43,
    MoveRowDown = 44,
    MoveColumnLeft = 45,
    MoveColumnRight = 46,
    FormatTable = 47,
    InsertTable = 48,
    Find = 49,
    Replace = 50,
    FindNext = 51,
    FindPrevious = 52,
    GoToLine = 53,
    SelectWord = 54,
    SelectParagraph = 55,
    MatchBracket = 56,
    SearchFolder = 57,
    /// Ctrl+Enter in the folder-search field. The same chord toggles a task
    /// in the editor; this scope is the search field, so the two coexist.
    PinSearchResult = 58,
    ToggleOutline = 59,
    NewFolder = 60,
    ShowKeys = 61,
    CloseTab = 62,
    TabNext = 63,
    TabPrevious = 64,
    TabLeft = 65,
    TabRight = 66,
    Tab1 = 67,
    Tab2 = 68,
    Tab3 = 69,
    Tab4 = 70,
    Tab5 = 71,
    Tab6 = 72,
    Tab7 = 73,
    Tab8 = 74,
    Tab9 = 75,
    ReopenTab = 76,
}

const COUNT: usize = 77;

impl Action {
    pub const ALL: [Action; COUNT] = [
        Action::CycleMode,
        Action::FocusCode,
        Action::FocusLive,
        Action::ToggleMinimap,
        Action::OpenFile,
        Action::Save,
        Action::SaveAs,
        Action::OpenFolder,
        Action::ToggleSidebar,
        Action::NewFile,
        Action::RecentFiles,
        Action::Back,
        Action::Forward,
        Action::Rename,
        Action::MoveToTrash,
        Action::Undo,
        Action::Redo,
        Action::Bold,
        Action::Italic,
        Action::Code,
        Action::Strikethrough,
        Action::Link,
        Action::ToggleTask,
        Action::Heading0,
        Action::Heading1,
        Action::Heading2,
        Action::Heading3,
        Action::Heading4,
        Action::Heading5,
        Action::Heading6,
        Action::SelectAll,
        Action::WordLeft,
        Action::WordRight,
        Action::DeleteWordLeft,
        Action::DeleteWordRight,
        Action::DocumentStart,
        Action::DocumentEnd,
        Action::InsertRowAbove,
        Action::InsertRowBelow,
        Action::InsertColumnLeft,
        Action::InsertColumnRight,
        Action::DeleteRow,
        Action::DeleteColumn,
        Action::MoveRowUp,
        Action::MoveRowDown,
        Action::MoveColumnLeft,
        Action::MoveColumnRight,
        Action::FormatTable,
        Action::InsertTable,
        Action::Find,
        Action::Replace,
        Action::FindNext,
        Action::FindPrevious,
        Action::GoToLine,
        Action::SelectWord,
        Action::SelectParagraph,
        Action::MatchBracket,
        Action::SearchFolder,
        Action::PinSearchResult,
        Action::ToggleOutline,
        Action::NewFolder,
        Action::ShowKeys,
        Action::CloseTab,
        Action::TabNext,
        Action::TabPrevious,
        Action::TabLeft,
        Action::TabRight,
        Action::Tab1,
        Action::Tab2,
        Action::Tab3,
        Action::Tab4,
        Action::Tab5,
        Action::Tab6,
        Action::Tab7,
        Action::Tab8,
        Action::Tab9,
        Action::ReopenTab,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    /// The `[keys]` name. The builtin table is the only place it is written.
    pub fn id(self) -> &'static str {
        self.spec().id
    }

    /// One line for the README and for `--list-keys`.
    pub fn description(self) -> &'static str {
        self.spec().description
    }

    pub fn scope(self) -> Scope {
        self.spec().scope
    }

    /// The overlay section this action is listed under.
    pub fn group(self) -> Group {
        self.spec().group
    }

    fn spec(self) -> &'static Builtin {
        let row = &TABLE[self.index()];
        assert_eq!(row.action, self, "binding table is out of order");
        row
    }

    /// Heading actions carry the level (0 is a paragraph).
    pub fn heading_level(self) -> Option<u8> {
        use Action::*;
        Some(match self {
            Heading0 => 0,
            Heading1 => 1,
            Heading2 => 2,
            Heading3 => 3,
            Heading4 => 4,
            Heading5 => 5,
            Heading6 => 6,
            _ => return None,
        })
    }

    /// Shift added to this chord extends the selection instead of moving.
    pub fn extends_with_shift(self) -> bool {
        matches!(
            self,
            Action::WordLeft | Action::WordRight | Action::DocumentStart | Action::DocumentEnd
        )
    }

    /// Shift doesn't change the chord: Ctrl+Shift+Backspace still deletes
    /// the previous word. An exact chord that includes Shift (delete
    /// column) is tried first, so this only sees a Shift that named nothing.
    pub fn ignores_shift(self) -> bool {
        matches!(self, Action::DeleteWordLeft | Action::DeleteWordRight)
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }
}

/// A key plus the modifiers that have to be held. Nothing else may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Modifiers,
    pub key: Key,
}

impl Chord {
    pub fn matches(self, key: Key, modifiers: Modifiers) -> bool {
        self.key == key && modifiers.matches_exact(self.modifiers)
    }

    /// Removes every press of one of `chords`. Matching is exact, the same
    /// rule as [`KeyMap::find`], so an extra Shift or Alt does not count.
    /// `true` when at least one press was removed.
    pub fn consume(input: &mut egui::InputState, chords: &[Self]) -> bool {
        let mut hit = false;
        input.events.retain(|event| {
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                return true;
            };
            if chords.iter().any(|chord| chord.matches(*key, *modifiers)) {
                hit = true;
                false
            } else {
                true
            }
        });
        hit
    }

    /// `Ctrl+Shift+S`, `Alt+Left`, `F2`. Modifier order is fixed so the
    /// same chord always prints the same way. Config and the README use
    /// this, which always says Ctrl, so a file written on one machine
    /// applies on another. Slash is `/`, the key a person presses.
    pub fn display(&self) -> String {
        self.platform_label(egui::os::OperatingSystem::Nix)
    }

    /// How this machine names the chord. Linux and Windows say Ctrl.
    /// macOS says Cmd for the command bit and Option for Alt. Super stays
    /// Super off macOS; egui's own names omit that bit there.
    pub fn platform_label(self, os: egui::os::OperatingSystem) -> String {
        let mac = os.is_mac();
        let mut mods = egui::ModifierNames::NAMES.format(&self.modifiers, mac);
        if !mac && self.modifiers.mac_cmd {
            if !mods.is_empty() {
                mods.push('+');
            }
            mods.push_str("Super");
        }
        let key = match self.key {
            Key::Slash => "/",
            other => other.name(),
        };
        if mods.is_empty() {
            key.to_owned()
        } else {
            format!("{mods}+{key}")
        }
    }

    /// Why the desktop will take this chord before inkmark sees it, if it will.
    ///
    /// Omarchy grabs these without Super: Ctrl+Alt+Delete closes every
    /// window, Alt+Tab (and Shift) cycles windows, Ctrl+Alt+Tab (and Shift)
    /// cycles monitors. Anything with Super is the compositor's too. The
    /// binding is still stored; the warning is so the banner can say the
    /// desktop will win.
    pub fn desktop_warning(self) -> Option<String> {
        let shown = self.display();
        let m = self.modifiers;
        if m.mac_cmd {
            return Some(format!("{shown} uses Super, which the desktop handles"));
        }
        let ctrl = m.command || m.ctrl;
        if self.key == Key::Delete && ctrl && m.alt && !m.shift {
            return Some(format!("{shown} closes all windows on this desktop"));
        }
        if self.key == Key::Tab && m.alt {
            let note = match (ctrl, m.shift) {
                (false, false) => "switches windows",
                (false, true) => "switches windows the other way",
                (true, false) => "switches monitors",
                (true, true) => "switches monitors the other way",
            };
            return Some(format!("{shown} {note} on this desktop"));
        }
        None
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut modifiers = Modifiers::NONE;
        let mut key = None;
        if text.trim().is_empty() {
            return Err(format!("can't read \"{text}\" as a chord"));
        }
        for part in text.split('+') {
            let part = part.trim();
            if part.is_empty() {
                return Err(format!("can't read \"{text}\" as a chord"));
            }
            if let Some(modifier) = modifier(part) {
                if key.is_some() {
                    return Err(format!("can't read \"{text}\" as a chord"));
                }
                modifiers = modifiers.plus(modifier);
                continue;
            }
            if key.is_some() {
                return Err(format!("can't read \"{text}\" as a chord"));
            }
            key = Some(parse_key(part).ok_or_else(|| format!("can't read \"{text}\" as a chord"))?);
        }
        let Some(key) = key else {
            return Err(format!("can't read \"{text}\" as a chord"));
        };
        // A bare typing key, or Shift plus one, still inserts the character:
        // egui delivers that as a Text event beside the Key event, and only
        // the Key event is matched. Ctrl, Alt, or Super suppresses it.
        let held = modifiers.command || modifiers.ctrl || modifiers.alt || modifiers.mac_cmd;
        if !held && types_a_character(key) {
            return Err(format!(
                "\"{text}\" types a character; it needs Ctrl, Alt, or Super"
            ));
        }
        Ok(Self { modifiers, key })
    }
}

/// Keys that produce a character on their own. Arrows, F-keys, Enter,
/// Backspace, Delete and the rest do not, so they can be bound bare
/// (F2 renames, Delete trashes).
fn types_a_character(key: Key) -> bool {
    matches!(
        key,
        Key::Space
            | Key::Colon
            | Key::Comma
            | Key::Backslash
            | Key::Slash
            | Key::Pipe
            | Key::Questionmark
            | Key::Exclamationmark
            | Key::OpenBracket
            | Key::CloseBracket
            | Key::OpenCurlyBracket
            | Key::CloseCurlyBracket
            | Key::Backtick
            | Key::Minus
            | Key::Period
            | Key::Plus
            | Key::Equals
            | Key::Semicolon
            | Key::Quote
            | Key::IntlBackslash
            | Key::Num0
            | Key::Num1
            | Key::Num2
            | Key::Num3
            | Key::Num4
            | Key::Num5
            | Key::Num6
            | Key::Num7
            | Key::Num8
            | Key::Num9
            | Key::A
            | Key::B
            | Key::C
            | Key::D
            | Key::E
            | Key::F
            | Key::G
            | Key::H
            | Key::I
            | Key::J
            | Key::K
            | Key::L
            | Key::M
            | Key::N
            | Key::O
            | Key::P
            | Key::Q
            | Key::R
            | Key::S
            | Key::T
            | Key::U
            | Key::V
            | Key::W
            | Key::X
            | Key::Y
            | Key::Z
    )
}

fn modifier(part: &str) -> Option<Modifiers> {
    match part.to_ascii_lowercase().as_str() {
        // Ctrl on Linux, Command on Mac: the same bit the built-in
        // shortcuts have always used (`Modifiers::COMMAND`).
        "ctrl" | "control" | "cmd" | "command" => Some(Modifiers::COMMAND),
        "alt" | "opt" | "option" => Some(Modifiers::ALT),
        "shift" => Some(Modifiers::SHIFT),
        "super" | "win" | "windows" | "meta" => Some(Modifiers::MAC_CMD),
        _ => None,
    }
}

fn parse_key(part: &str) -> Option<Key> {
    match part.to_ascii_lowercase().as_str() {
        "pageup" | "page-up" | "pgup" => return Some(Key::PageUp),
        "pagedown" | "page-down" | "pgdn" => return Some(Key::PageDown),
        "arrowleft" | "arrow-left" => return Some(Key::ArrowLeft),
        "arrowright" | "arrow-right" => return Some(Key::ArrowRight),
        "arrowup" | "arrow-up" => return Some(Key::ArrowUp),
        "arrowdown" | "arrow-down" => return Some(Key::ArrowDown),
        "esc" => return Some(Key::Escape),
        "del" => return Some(Key::Delete),
        _ => {}
    }
    if let Some(key) = Key::from_name(part) {
        return Some(key);
    }
    let mut chars = part.chars();
    let first = chars.next()?;
    let titled = first.to_ascii_uppercase().to_string() + chars.as_str();
    Key::from_name(&titled)
}

/// What config.toml's `[keys]` changed the defaults into, and why some
/// entries were left alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub map: KeyMap,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// The chords each action currently uses. Empty means the action is unbound.
///
/// `rows` is the builtin table with config's chords applied. `listed` is the overlay
/// text for those rows, rebuilt when a chord changes so a frame does not
/// format and lowercase the whole list again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyMap {
    rows: Vec<Binding>,
    listed: Vec<Listed>,
}

/// Cached overlay text for one [`Binding`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listed {
    /// Lowercased description, config name, Ctrl-form chords, and key names.
    haystack: String,
    /// Lowercased macOS chord text. Searched only on macOS.
    mac_lower: String,
    /// Chord text with Ctrl, for menus, config, and Linux/Windows.
    other: String,
    /// Chord text the way macOS names modifiers.
    mac: String,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::builtin()
    }
}

impl KeyMap {
    pub fn builtin() -> Self {
        assert_eq!(TABLE.len(), COUNT, "the binding table is missing an action");
        let rows: Vec<Binding> = TABLE
            .iter()
            .enumerate()
            .map(|(i, spec)| {
                assert_eq!(spec.action.index(), i, "binding table is out of order");
                Binding {
                    action: spec.action,
                    group: spec.group,
                    scope: spec.scope,
                    description: spec.description,
                    chords: spec.chords.to_vec(),
                }
            })
            .collect();
        let listed = rows.iter().map(Listed::from_binding).collect();
        Self { rows, listed }
    }

    pub fn chords(&self, action: Action) -> &[Chord] {
        &self.rows[action.index()].chords
    }

    pub fn set(&mut self, action: Action, chords: Vec<Chord>) {
        let i = action.index();
        self.rows[i].chords = chords;
        self.listed[i] = Listed::from_binding(&self.rows[i]);
    }

    /// The label drawn next to a menu item. Every chord, so a rebound
    /// action doesn't keep advertising the old key. Always the Ctrl form
    /// (`display`), so a menu and config.toml agree.
    pub fn shortcut_text(&self, action: Action) -> String {
        self.listed[action.index()].other.clone()
    }

    /// The same chords, named the way `os` names modifiers.
    pub fn platform_text(&self, os: egui::os::OperatingSystem, action: Action) -> String {
        let listed = &self.listed[action.index()];
        if os.is_mac() {
            listed.mac.clone()
        } else {
            listed.other.clone()
        }
    }

    /// The shortcut rows, in [`Action::ALL`] order. Empty `chords` means
    /// the action is unbound. The overlay and `--list-keys` read this, and
    /// [`Self::find`] reads the same rows.
    pub fn bindings(&self) -> &[Binding] {
        &self.rows
    }

    /// Whether the overlay keeps this row for `query`. An empty query keeps
    /// every row. Matching is case-insensitive, against the description, the
    /// config name, and the chord text (Ctrl form and this machine's names).
    pub fn matches_query(
        &self,
        action: Action,
        os: egui::os::OperatingSystem,
        query: &str,
    ) -> bool {
        self.matches_lower(action, os, &query.trim().to_lowercase())
    }

    /// `query_lower` is already trimmed and lowercased, so a frame can
    /// prepare it once and test every row.
    pub fn matches_lower(
        &self,
        action: Action,
        os: egui::os::OperatingSystem,
        query_lower: &str,
    ) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        let listed = &self.listed[action.index()];
        listed.haystack.contains(query_lower)
            || (os.is_mac() && listed.mac_lower.contains(query_lower))
    }

    /// The action whose row in `scope` names this chord. Reads [`Self::bindings`].
    pub fn find(&self, key: Key, modifiers: Modifiers, scope: Scope) -> Option<Action> {
        self.rows.iter().find_map(|row| {
            (row.scope == scope && row.chords.iter().any(|chord| chord.matches(key, modifiers)))
                .then_some(row.action)
        })
    }

    /// Editor chords and table chords, which the focused pane both sees.
    pub fn find_editing(&self, key: Key, modifiers: Modifiers) -> Option<Action> {
        self.rows.iter().find_map(|row| {
            (matches!(row.scope, Scope::Editor | Scope::Table)
                && row.chords.iter().any(|chord| chord.matches(key, modifiers)))
            .then_some(row.action)
        })
    }

    /// The editing action for this press.
    ///
    /// An exact chord wins. Failing that, Shift on a motion chord extends
    /// the selection, and Shift on delete-word still deletes: those
    /// commands have always treated Shift that way, and delete-column
    /// (which is Shift on purpose) was already claimed by the exact match.
    /// Anything else with an extra Shift is not a match, so Ctrl+Shift+B
    /// is not bold.
    ///
    /// Shift on a table chord that doesn't name Shift is not that command
    /// either. Ctrl+Alt+Left inserts a column, so Ctrl+Alt+Shift+Left is
    /// the ordinary Ctrl+Shift+Left and selects a word. The Alt is what
    /// made it a table chord; without the table command claiming it, the
    /// word motion underneath is what's left.
    pub fn editor_gesture(&self, key: Key, modifiers: Modifiers) -> Option<(Action, bool)> {
        if let Some(action) = self.find_editing(key, modifiers) {
            return Some((action, false));
        }
        if !modifiers.shift {
            return None;
        }
        let bare = Modifiers {
            shift: false,
            ..modifiers
        };
        if let Some(action) = self.shifted(key, bare) {
            return Some(action);
        }
        if self.find(key, bare, Scope::Table).is_some() {
            let without_alt = Modifiers { alt: false, ..bare };
            return self.shifted(key, without_alt);
        }
        None
    }

    /// Shift added to an editor chord: extend a motion, or ignore it on
    /// delete-word. `None` when nothing is bound there, or when the action
    /// doesn't treat Shift that way.
    fn shifted(&self, key: Key, bare: Modifiers) -> Option<(Action, bool)> {
        let action = self.find(key, bare, Scope::Editor)?;
        if action.extends_with_shift() {
            Some((action, true))
        } else if action.ignores_shift() {
            Some((action, false))
        } else {
            None
        }
    }

    /// Removes presses of this one action and leaves every other key.
    /// Folder search uses it so a rebound pin chord is the key the field
    /// actually reads. Matching is exact, the same rule as [`Self::find`].
    pub fn consume_action(&self, input: &mut egui::InputState, action: Action) -> bool {
        Chord::consume(input, self.chords(action))
    }

    /// Removes every press of an action in `scope` and returns them, in
    /// the order the keys were pressed. The panes never see those events.
    pub fn consume(&self, input: &mut egui::InputState, scope: Scope) -> Vec<Action> {
        let mut found = Vec::new();
        input.events.retain(|event| {
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                return true;
            };
            if let Some(action) = self.find(*key, *modifiers, scope) {
                found.push(action);
                false
            } else {
                true
            }
        });
        found
    }

    /// Applies `[keys]` entries `(action id, chord texts)`. An empty list
    /// unbinds. An unknown action, a chord that doesn't parse, or an entry
    /// that would share a chord with another action is reported and that
    /// action keeps its default. A swap (each action taking the other's
    /// chord) is not a conflict. If rejecting one entry makes another
    /// entry collide with the default that snapped back, that one is
    /// rejected too: otherwise the snapped-back default and the entry
    /// would share the chord.
    pub fn apply(entries: &[(String, Vec<String>)]) -> Applied {
        let mut errors = Vec::new();
        let mut accepted: Vec<(Action, Vec<Chord>)> = Vec::new();
        for (name, texts) in entries {
            let Some(action) = Action::from_id(name) else {
                errors.push(format!("Unknown key action \"{name}\""));
                continue;
            };
            let mut chords = Vec::new();
            let mut bad = false;
            for text in texts {
                match Chord::parse(text) {
                    Ok(chord) => {
                        if !chords.contains(&chord) {
                            chords.push(chord);
                        }
                    }
                    Err(why) => {
                        errors.push(format!("keys.{name}: {why} (keeping the default)"));
                        bad = true;
                        break;
                    }
                }
            }
            if bad {
                continue;
            }
            if let Some(slot) = accepted.iter_mut().find(|(have, _)| *have == action) {
                slot.1 = chords;
            } else {
                accepted.push((action, chords));
            }
        }

        loop {
            let mut trial = Self::builtin();
            for (action, chords) in &accepted {
                trial.set(*action, chords.clone());
            }
            let clashes = trial.clashes();
            if clashes.is_empty() {
                let warnings = trial.desktop_warnings();
                return Applied {
                    map: trial,
                    errors,
                    warnings,
                };
            }
            let before = accepted.len();
            let mut kept = Vec::new();
            for (action, chords) in accepted.drain(..) {
                let clash = chords
                    .iter()
                    .find_map(|chord| clashes.iter().find(|(clashing, _)| clashing == chord));
                let Some((chord, owners)) = clash else {
                    kept.push((action, chords));
                    continue;
                };
                let others: Vec<&str> = owners
                    .iter()
                    .filter(|owner| **owner != action)
                    .map(|owner| owner.id())
                    .collect();
                errors.push(format!(
                    "keys.{} shares {} with {} (keeping the default)",
                    action.id(),
                    chord.display(),
                    others.join(" and ")
                ));
            }
            accepted = kept;
            if accepted.len() == before {
                // Two defaults share a chord. That is a bug in this table,
                // not in the user's file: don't loop, and don't apply a
                // map that would fire one action for the other's key.
                errors.push(
                    "The default key bindings conflict; ignoring [keys] and using them anyway"
                        .into(),
                );
                return Applied {
                    map: Self::builtin(),
                    errors,
                    warnings: Vec::new(),
                };
            }
        }
    }

    fn clashes(&self) -> Vec<(Chord, Vec<Action>)> {
        let mut found: Vec<(Chord, Vec<Action>)> = Vec::new();
        for row in &self.rows {
            for chord in &row.chords {
                if let Some((_, owners)) = found.iter_mut().find(|(have, _)| have == chord) {
                    owners.push(row.action);
                } else {
                    found.push((*chord, vec![row.action]));
                }
            }
        }
        found.retain(|(_, owners)| {
            owners.iter().any(|left| {
                owners
                    .iter()
                    .any(|right| *left != *right && scopes_conflict(left.scope(), right.scope()))
            })
        });
        found
    }

    fn desktop_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for action in Action::ALL {
            for chord in self.chords(action) {
                if let Some(note) = chord.desktop_warning() {
                    warnings.push(format!("keys.{}: {note}", action.id()));
                }
            }
        }
        warnings
    }

    /// The `[keys]` table, ready to paste into config.toml. One chord is a
    /// string, more than one (or none) is a list.
    pub fn to_toml(&self) -> String {
        let mut out = String::from(
            "# Key bindings. A chord (\"Ctrl+B\") or a list; [] unbinds.\n\
             # Anything left out keeps its default. inkmark --list-keys prints this.\n\
             \n[keys]\n",
        );
        for action in Action::ALL {
            out.push_str(&format!("# {}\n", action.description()));
            out.push_str(action.id());
            out.push_str(" = ");
            match self.chords(action) {
                [] => out.push_str("[]\n"),
                [one] => out.push_str(&format!("{}\n", toml_string(&one.display()))),
                many => {
                    out.push('[');
                    for (i, chord) in many.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(&toml_string(&chord.display()));
                    }
                    out.push_str("]\n");
                }
            }
        }
        out
    }

    /// The README shortcut table. A test checks the README still contains it.
    pub fn readme_table() -> String {
        let map = Self::builtin();
        let mut out = String::from("| Keys | Action |\n|---|---|\n");
        for action in Action::ALL {
            let keys = map.shortcut_text(action);
            if keys.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "| {keys} | {} (`{}`) |\n",
                action.description(),
                action.id()
            ));
        }
        out
    }
}

fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

const fn chord(modifiers: Modifiers, key: Key) -> Chord {
    Chord { modifiers, key }
}

const CTRL: Modifiers = Modifiers::COMMAND;
const ALT: Modifiers = Modifiers::ALT;
const SHIFT: Modifiers = Modifiers::SHIFT;
const CTRL_SHIFT: Modifiers = CTRL.plus(SHIFT);
const CTRL_ALT: Modifiers = CTRL.plus(ALT);
const ALT_SHIFT: Modifiers = ALT.plus(SHIFT);
const CTRL_ALT_SHIFT: Modifiers = CTRL_ALT.plus(SHIFT);

/// One builtin shortcut. [`TABLE`] is the only place a default chord, group,
/// scope, or description is written. [`Action::id`] and the other accessors
/// read it, and [`KeyMap::builtin`] copies it.
struct Builtin {
    action: Action,
    id: &'static str,
    group: Group,
    scope: Scope,
    description: &'static str,
    chords: &'static [Chord],
}

const TABLE: &[Builtin] = &[
    Builtin {
        action: Action::CycleMode,
        id: "cycle_mode",
        group: Group::View,
        scope: Scope::App,
        description: "Cycle split, code, and live",
        chords: &[chord(CTRL, Key::E)],
    },
    Builtin {
        action: Action::FocusCode,
        id: "focus_code",
        group: Group::View,
        scope: Scope::App,
        description: "Focus the code pane (switching to it when only live shows)",
        chords: &[chord(CTRL, Key::Num1)],
    },
    Builtin {
        action: Action::FocusLive,
        id: "focus_live",
        group: Group::View,
        scope: Scope::App,
        description: "Focus the live pane (switching to it when only code shows)",
        chords: &[chord(CTRL, Key::Num2)],
    },
    Builtin {
        action: Action::ToggleMinimap,
        id: "toggle_minimap",
        group: Group::Minimap,
        scope: Scope::App,
        description: "Toggle the focused pane's minimap",
        chords: &[chord(CTRL, Key::M)],
    },
    Builtin {
        action: Action::OpenFile,
        id: "open_file",
        group: Group::File,
        scope: Scope::App,
        description: "Open a file",
        chords: &[chord(CTRL, Key::O)],
    },
    Builtin {
        action: Action::Save,
        id: "save",
        group: Group::File,
        scope: Scope::App,
        description: "Save",
        chords: &[chord(CTRL, Key::S)],
    },
    Builtin {
        action: Action::SaveAs,
        id: "save_as",
        group: Group::File,
        scope: Scope::App,
        description: "Save as",
        chords: &[chord(CTRL_SHIFT, Key::S)],
    },
    Builtin {
        action: Action::OpenFolder,
        id: "open_folder",
        group: Group::File,
        scope: Scope::App,
        description: "Open a folder in the sidebar",
        chords: &[chord(CTRL_SHIFT, Key::O)],
    },
    Builtin {
        action: Action::ToggleSidebar,
        id: "toggle_sidebar",
        group: Group::View,
        scope: Scope::App,
        description: "Show or hide the sidebar",
        chords: &[chord(CTRL_SHIFT, Key::E)],
    },
    Builtin {
        action: Action::NewFile,
        id: "new_file",
        group: Group::File,
        scope: Scope::App,
        description: "New Markdown file in the selected folder, or the open folder",
        chords: &[chord(CTRL, Key::N)],
    },
    Builtin {
        action: Action::RecentFiles,
        id: "recent_files",
        group: Group::File,
        scope: Scope::App,
        description: "Recent files",
        chords: &[chord(CTRL, Key::R)],
    },
    Builtin {
        action: Action::Back,
        id: "back",
        group: Group::Navigation,
        scope: Scope::App,
        description: "Back to where you followed the last link from",
        chords: &[chord(ALT, Key::ArrowLeft)],
    },
    Builtin {
        action: Action::Forward,
        id: "forward",
        group: Group::Navigation,
        scope: Scope::App,
        description: "Forward again",
        chords: &[chord(ALT, Key::ArrowRight)],
    },
    Builtin {
        action: Action::Rename,
        id: "rename",
        group: Group::File,
        scope: Scope::Browser,
        description: "Rename the selected file or folder",
        chords: &[chord(Modifiers::NONE, Key::F2)],
    },
    Builtin {
        action: Action::MoveToTrash,
        id: "move_to_trash",
        group: Group::File,
        scope: Scope::Browser,
        description: "Move the selected file or folder to the trash",
        chords: &[chord(Modifiers::NONE, Key::Delete)],
    },
    Builtin {
        action: Action::Undo,
        id: "undo",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Undo",
        chords: &[chord(CTRL, Key::Z)],
    },
    Builtin {
        action: Action::Redo,
        id: "redo",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Redo",
        chords: &[chord(CTRL_SHIFT, Key::Z), chord(CTRL, Key::Y)],
    },
    Builtin {
        action: Action::Bold,
        id: "bold",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Toggle bold",
        chords: &[chord(CTRL, Key::B)],
    },
    Builtin {
        action: Action::Italic,
        id: "italic",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Toggle italic",
        chords: &[chord(CTRL, Key::I)],
    },
    Builtin {
        action: Action::Code,
        id: "code",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Toggle code",
        chords: &[chord(CTRL, Key::Backtick)],
    },
    Builtin {
        action: Action::Strikethrough,
        id: "strikethrough",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Toggle strikethrough",
        chords: &[chord(CTRL_SHIFT, Key::X)],
    },
    Builtin {
        action: Action::Link,
        id: "link",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Insert a link",
        chords: &[chord(CTRL, Key::K)],
    },
    Builtin {
        action: Action::ToggleTask,
        id: "toggle_task",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Toggle the line's task checkbox",
        chords: &[chord(CTRL, Key::Enter)],
    },
    Builtin {
        action: Action::Heading0,
        id: "heading_0",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Turn the line into a paragraph",
        chords: &[chord(CTRL_ALT, Key::Num0)],
    },
    Builtin {
        action: Action::Heading1,
        id: "heading_1",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Set heading level 1",
        chords: &[chord(CTRL_ALT, Key::Num1)],
    },
    Builtin {
        action: Action::Heading2,
        id: "heading_2",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Set heading level 2",
        chords: &[chord(CTRL_ALT, Key::Num2)],
    },
    Builtin {
        action: Action::Heading3,
        id: "heading_3",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Set heading level 3",
        chords: &[chord(CTRL_ALT, Key::Num3)],
    },
    Builtin {
        action: Action::Heading4,
        id: "heading_4",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Set heading level 4",
        chords: &[chord(CTRL_ALT, Key::Num4)],
    },
    Builtin {
        action: Action::Heading5,
        id: "heading_5",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Set heading level 5",
        chords: &[chord(CTRL_ALT, Key::Num5)],
    },
    Builtin {
        action: Action::Heading6,
        id: "heading_6",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Set heading level 6",
        chords: &[chord(CTRL_ALT, Key::Num6)],
    },
    Builtin {
        action: Action::SelectAll,
        id: "select_all",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Select all",
        chords: &[chord(CTRL, Key::A)],
    },
    Builtin {
        action: Action::WordLeft,
        id: "word_left",
        group: Group::Navigation,
        scope: Scope::Editor,
        description: "Move to the previous word",
        chords: &[chord(CTRL, Key::ArrowLeft)],
    },
    Builtin {
        action: Action::WordRight,
        id: "word_right",
        group: Group::Navigation,
        scope: Scope::Editor,
        description: "Move to the next word",
        chords: &[chord(CTRL, Key::ArrowRight)],
    },
    Builtin {
        action: Action::DeleteWordLeft,
        id: "delete_word_left",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Delete the previous word",
        chords: &[chord(CTRL, Key::Backspace)],
    },
    Builtin {
        action: Action::DeleteWordRight,
        id: "delete_word_right",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Delete the next word",
        chords: &[chord(CTRL, Key::Delete)],
    },
    Builtin {
        action: Action::DocumentStart,
        id: "document_start",
        group: Group::Navigation,
        scope: Scope::Editor,
        description: "Move to the start of the document",
        chords: &[chord(CTRL, Key::Home)],
    },
    Builtin {
        action: Action::DocumentEnd,
        id: "document_end",
        group: Group::Navigation,
        scope: Scope::Editor,
        description: "Move to the end of the document",
        chords: &[chord(CTRL, Key::End)],
    },
    Builtin {
        action: Action::InsertRowAbove,
        id: "insert_row_above",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Insert a table row above",
        chords: &[chord(CTRL_ALT, Key::ArrowUp)],
    },
    Builtin {
        action: Action::InsertRowBelow,
        id: "insert_row_below",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Insert a table row below",
        chords: &[chord(CTRL_ALT, Key::ArrowDown)],
    },
    Builtin {
        action: Action::InsertColumnLeft,
        id: "insert_column_left",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Insert a table column to the left",
        chords: &[chord(CTRL_ALT, Key::ArrowLeft)],
    },
    Builtin {
        action: Action::InsertColumnRight,
        id: "insert_column_right",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Insert a table column to the right",
        chords: &[chord(CTRL_ALT, Key::ArrowRight)],
    },
    Builtin {
        action: Action::DeleteRow,
        id: "delete_row",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Delete the table row",
        chords: &[chord(CTRL_ALT, Key::Backspace)],
    },
    Builtin {
        action: Action::DeleteColumn,
        id: "delete_column",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Delete the table column",
        chords: &[chord(CTRL_ALT_SHIFT, Key::Backspace)],
    },
    Builtin {
        action: Action::MoveRowUp,
        id: "move_row_up",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Move the table row up",
        chords: &[chord(ALT_SHIFT, Key::ArrowUp)],
    },
    Builtin {
        action: Action::MoveRowDown,
        id: "move_row_down",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Move the table row down",
        chords: &[chord(ALT_SHIFT, Key::ArrowDown)],
    },
    Builtin {
        action: Action::MoveColumnLeft,
        id: "move_column_left",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Move the table column left",
        chords: &[chord(ALT_SHIFT, Key::ArrowLeft)],
    },
    Builtin {
        action: Action::MoveColumnRight,
        id: "move_column_right",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Move the table column right",
        chords: &[chord(ALT_SHIFT, Key::ArrowRight)],
    },
    Builtin {
        action: Action::FormatTable,
        id: "format_table",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Line up the table's pipes",
        chords: &[chord(CTRL_ALT, Key::F)],
    },
    Builtin {
        action: Action::InsertTable,
        id: "insert_table",
        group: Group::Edit,
        scope: Scope::Table,
        description: "Insert a 3×3 table",
        chords: &[chord(CTRL_ALT, Key::T)],
    },
    Builtin {
        action: Action::Find,
        id: "find",
        group: Group::Find,
        scope: Scope::App,
        description: "Find in this file",
        chords: &[chord(CTRL, Key::F)],
    },
    Builtin {
        action: Action::Replace,
        id: "replace",
        group: Group::Find,
        scope: Scope::App,
        description: "Replace in this file",
        chords: &[chord(CTRL, Key::H)],
    },
    Builtin {
        action: Action::FindNext,
        id: "find_next",
        group: Group::Find,
        scope: Scope::App,
        description: "Find the next match",
        chords: &[chord(Modifiers::NONE, Key::F3)],
    },
    Builtin {
        action: Action::FindPrevious,
        id: "find_previous",
        group: Group::Find,
        scope: Scope::App,
        description: "Find the previous match",
        chords: &[chord(SHIFT, Key::F3)],
    },
    Builtin {
        action: Action::GoToLine,
        id: "go_to_line",
        group: Group::Navigation,
        scope: Scope::App,
        description: "Go to line",
        chords: &[chord(CTRL, Key::G)],
    },
    Builtin {
        action: Action::SelectWord,
        id: "select_word",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Select word",
        chords: &[chord(CTRL, Key::D)],
    },
    Builtin {
        action: Action::SelectParagraph,
        id: "select_paragraph",
        group: Group::Edit,
        scope: Scope::Editor,
        description: "Select paragraph",
        chords: &[chord(CTRL_SHIFT, Key::P)],
    },
    Builtin {
        action: Action::MatchBracket,
        id: "match_bracket",
        group: Group::Navigation,
        scope: Scope::Editor,
        description: "Jump to the matching fence or brackets",
        chords: &[chord(CTRL_SHIFT, Key::Backslash)],
    },
    Builtin {
        action: Action::SearchFolder,
        id: "search_folder",
        group: Group::Find,
        scope: Scope::App,
        description: "Search the open folder",
        chords: &[chord(CTRL_SHIFT, Key::F)],
    },
    Builtin {
        action: Action::PinSearchResult,
        id: "pin_search_result",
        group: Group::Find,
        scope: Scope::Search,
        description: "Open the selected folder-search result in a pinned tab",
        chords: &[chord(CTRL, Key::Enter)],
    },
    Builtin {
        action: Action::ToggleOutline,
        id: "toggle_outline",
        group: Group::View,
        scope: Scope::App,
        description: "Show or hide the outline",
        chords: &[chord(CTRL_SHIFT, Key::B)],
    },
    Builtin {
        action: Action::NewFolder,
        id: "new_folder",
        group: Group::File,
        scope: Scope::App,
        description: "New folder in the selected folder, or the open folder",
        chords: &[chord(CTRL_SHIFT, Key::N)],
    },
    Builtin {
        action: Action::ShowKeys,
        id: "show_keys",
        group: Group::View,
        scope: Scope::App,
        description: "Show or hide the key bindings",
        chords: &[chord(CTRL, Key::Slash), chord(Modifiers::NONE, Key::F1)],
    },
    Builtin {
        action: Action::CloseTab,
        id: "close_tab",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Close the tab",
        chords: &[chord(CTRL, Key::W)],
    },
    Builtin {
        action: Action::TabNext,
        id: "tab_next",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to the next most recently used tab",
        chords: &[chord(CTRL, Key::Tab)],
    },
    Builtin {
        action: Action::TabPrevious,
        id: "tab_previous",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to the previous most recently used tab",
        chords: &[chord(CTRL_SHIFT, Key::Tab)],
    },
    Builtin {
        action: Action::TabLeft,
        id: "tab_left",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to the tab on the left",
        chords: &[chord(CTRL, Key::PageUp)],
    },
    Builtin {
        action: Action::TabRight,
        id: "tab_right",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to the tab on the right",
        chords: &[chord(CTRL, Key::PageDown)],
    },
    Builtin {
        action: Action::Tab1,
        id: "tab_1",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to tab 1",
        chords: &[chord(ALT, Key::Num1)],
    },
    Builtin {
        action: Action::Tab2,
        id: "tab_2",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to tab 2",
        chords: &[chord(ALT, Key::Num2)],
    },
    Builtin {
        action: Action::Tab3,
        id: "tab_3",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to tab 3",
        chords: &[chord(ALT, Key::Num3)],
    },
    Builtin {
        action: Action::Tab4,
        id: "tab_4",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to tab 4",
        chords: &[chord(ALT, Key::Num4)],
    },
    Builtin {
        action: Action::Tab5,
        id: "tab_5",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to tab 5",
        chords: &[chord(ALT, Key::Num5)],
    },
    Builtin {
        action: Action::Tab6,
        id: "tab_6",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to tab 6",
        chords: &[chord(ALT, Key::Num6)],
    },
    Builtin {
        action: Action::Tab7,
        id: "tab_7",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to tab 7",
        chords: &[chord(ALT, Key::Num7)],
    },
    Builtin {
        action: Action::Tab8,
        id: "tab_8",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to tab 8",
        chords: &[chord(ALT, Key::Num8)],
    },
    Builtin {
        action: Action::Tab9,
        id: "tab_9",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Switch to tab 9",
        chords: &[chord(ALT, Key::Num9)],
    },
    Builtin {
        action: Action::ReopenTab,
        id: "reopen_tab",
        group: Group::Tabs,
        scope: Scope::App,
        description: "Reopen the closed tab",
        chords: &[chord(CTRL_SHIFT, Key::T)],
    },
];

impl Listed {
    fn from_binding(row: &Binding) -> Self {
        let other = row
            .chords
            .iter()
            .map(|c| c.platform_label(egui::os::OperatingSystem::Nix))
            .collect::<Vec<_>>()
            .join(", ");
        let mac = row
            .chords
            .iter()
            .map(|c| c.platform_label(egui::os::OperatingSystem::Mac))
            .collect::<Vec<_>>()
            .join(", ");
        let mut hay = String::new();
        hay.push_str(row.description);
        hay.push(' ');
        hay.push_str(row.action.id());
        hay.push(' ');
        hay.push_str(&other);
        for chord in &row.chords {
            hay.push(' ');
            hay.push_str(chord.key.name());
        }
        Self {
            haystack: hay.to_lowercase(),
            mac_lower: mac.to_lowercase(),
            other,
            mac,
        }
    }
}

/// Search may share a chord with the editor, because the field has focus
/// instead of the pane. Every other pair can see the key together, or the
/// app takes it first, so those conflict.
fn scopes_conflict(a: Scope, b: Scope) -> bool {
    if a == b {
        return true;
    }
    !matches!(
        (a, b),
        (Scope::Search, Scope::Editor | Scope::Table)
            | (Scope::Editor | Scope::Table, Scope::Search)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, chords: &[&str]) -> (String, Vec<String>) {
        (
            name.to_owned(),
            chords.iter().map(|c| (*c).to_owned()).collect(),
        )
    }

    #[test]
    fn actions_are_indexed_and_named_once() {
        let mut names = Vec::new();
        for (i, action) in Action::ALL.iter().enumerate() {
            assert_eq!(action.index(), i);
            assert!(!action.id().is_empty());
            assert!(!action.description().is_empty());
            names.push(action.id());
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Action::ALL.len());
    }

    #[test]
    fn defaults_give_every_action_a_chord_and_never_share_one() {
        let map = KeyMap::builtin();
        assert!(map.clashes().is_empty(), "{:?}", map.clashes());
        // Ctrl+Enter toggles a task in the editor and pins a search hit
        // while the search field is focused. Different scopes, so it is
        // not a clash.
        assert_eq!(
            map.chords(Action::ToggleTask),
            map.chords(Action::PinSearchResult)
        );
        for action in Action::ALL {
            assert!(
                !map.chords(action).is_empty(),
                "{} has no default",
                action.id()
            );
        }
        assert_eq!(map.shortcut_text(Action::Bold), "Ctrl+B");
        assert_eq!(map.shortcut_text(Action::Redo), "Ctrl+Shift+Z, Ctrl+Y");
        assert_eq!(map.shortcut_text(Action::Back), "Alt+Left");
        assert_eq!(map.shortcut_text(Action::Code), "Ctrl+Backtick");
        assert_eq!(
            map.shortcut_text(Action::DeleteColumn),
            "Ctrl+Alt+Shift+Backspace"
        );
        assert_eq!(map.shortcut_text(Action::SelectWord), "Ctrl+D");
        assert_eq!(map.shortcut_text(Action::SelectParagraph), "Ctrl+Shift+P");
        assert_eq!(
            map.shortcut_text(Action::MatchBracket),
            "Ctrl+Shift+Backslash"
        );
        assert_eq!(map.shortcut_text(Action::ToggleOutline), "Ctrl+Shift+B");
        assert_eq!(map.shortcut_text(Action::NewFolder), "Ctrl+Shift+N");
        assert_eq!(map.shortcut_text(Action::ShowKeys), "Ctrl+/, F1");
        assert_eq!(map.shortcut_text(Action::PinSearchResult), "Ctrl+Enter");
        assert_eq!(map.shortcut_text(Action::CloseTab), "Ctrl+W");
        assert_eq!(map.shortcut_text(Action::ReopenTab), "Ctrl+Shift+T");
        assert_eq!(map.shortcut_text(Action::TabNext), "Ctrl+Tab");
        assert_eq!(map.shortcut_text(Action::TabPrevious), "Ctrl+Shift+Tab");
        assert_eq!(map.shortcut_text(Action::TabLeft), "Ctrl+PageUp");
        assert_eq!(map.shortcut_text(Action::TabRight), "Ctrl+PageDown");
        assert_eq!(map.shortcut_text(Action::Tab1), "Alt+1");
        assert_eq!(map.shortcut_text(Action::Tab9), "Alt+9");
    }

    #[test]
    fn chords_match_ctrl_either_way_and_reject_extra_shift_or_alt() {
        // Linux reports Ctrl as both `ctrl` and `command`; the tests press
        // `Modifiers::COMMAND` alone. Extra Shift or Alt must not match:
        // that is the bug in `matches_logically`.
        let bold = Chord::parse("Ctrl+B").unwrap();
        assert!(bold.matches(Key::B, Modifiers::COMMAND));
        assert!(bold.matches(Key::B, Modifiers::COMMAND.plus(Modifiers::CTRL)));
        assert!(!bold.matches(Key::B, Modifiers::COMMAND.plus(Modifiers::SHIFT)));
        assert!(!bold.matches(Key::B, Modifiers::COMMAND.plus(Modifiers::ALT)));
        let back = Chord::parse("Alt+Left").unwrap();
        assert!(back.matches(Key::ArrowLeft, Modifiers::ALT));
        assert!(!back.matches(Key::ArrowLeft, Modifiers::ALT.plus(Modifiers::SHIFT)));
        let column = Chord::parse("Alt+Shift+Left").unwrap();
        assert!(column.matches(Key::ArrowLeft, Modifiers::ALT.plus(Modifiers::SHIFT)));
        assert!(!column.matches(Key::ArrowLeft, Modifiers::ALT));
    }

    #[test]
    fn chord_text_parses_the_forms_config_will_see() {
        assert_eq!(
            Chord::parse(" ctrl + shift + s ").unwrap().display(),
            "Ctrl+Shift+S"
        );
        assert_eq!(Chord::parse("F2").unwrap().key, Key::F2);
        assert_eq!(Chord::parse("delete").unwrap().key, Key::Delete);
        assert_eq!(Chord::parse("Ctrl+`").unwrap().key, Key::Backtick);
        assert_eq!(Chord::parse("Ctrl+Alt+0").unwrap().display(), "Ctrl+Alt+0");
        assert!(Chord::parse("Super+B").unwrap().modifiers.mac_cmd);
        assert!(Chord::parse("Alt+B").is_ok());
        assert!(Chord::parse("Shift+F2").is_ok());
        for bad in ["", "Ctrl+", "Ctrl++B", "NoSuch", "Ctrl+B+Shift", "Shift"] {
            assert!(Chord::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_typing_key_needs_ctrl_alt_or_super() {
        // Review of #34: the Key event would run the action and the Text
        // event would still insert the character. Shift doesn't stop that.
        for bare in ["B", "Shift+B", "Space", "2", "Backtick", "Shift+Equals"] {
            let err = Chord::parse(bare).expect_err(bare);
            assert!(err.contains("types a character"), "{bare}: {err}");
        }
        let applied = KeyMap::apply(&[entry("bold", &["B"]), entry("save", &["S"])]);
        let text = applied.errors.join("\n");
        assert!(text.contains("keys.bold"), "{text}");
        assert!(text.contains("keys.save"), "{text}");
        assert!(text.contains("types a character"), "{text}");
        assert_eq!(applied.map.shortcut_text(Action::Bold), "Ctrl+B");
        assert_eq!(applied.map.shortcut_text(Action::Save), "Ctrl+S");
    }

    #[test]
    fn shift_extends_a_motion_and_does_not_widen_any_other_chord() {
        let map = KeyMap::builtin();
        let ctrl_shift = Modifiers::COMMAND.plus(Modifiers::SHIFT);
        assert_eq!(
            map.editor_gesture(Key::ArrowLeft, ctrl_shift),
            Some((Action::WordLeft, true))
        );
        assert_eq!(
            map.editor_gesture(Key::Backspace, ctrl_shift),
            Some((Action::DeleteWordLeft, false))
        );
        assert_eq!(map.editor_gesture(Key::B, ctrl_shift), None);
        assert_eq!(
            map.editor_gesture(Key::B, Modifiers::COMMAND),
            Some((Action::Bold, false))
        );
        // Exact, not "Back plus Shift".
        assert_eq!(
            map.editor_gesture(Key::ArrowLeft, Modifiers::ALT.plus(Modifiers::SHIFT)),
            Some((Action::MoveColumnLeft, false))
        );
        assert_eq!(
            map.find(Key::ArrowLeft, Modifiers::ALT, Scope::App),
            Some(Action::Back)
        );
        assert_eq!(
            map.find(
                Key::ArrowLeft,
                Modifiers::ALT.plus(Modifiers::SHIFT),
                Scope::App
            ),
            None
        );
    }

    #[test]
    fn shift_on_a_column_chord_selects_a_word() {
        // Review of #34. Ctrl+Alt+Left is insert-column, so its Shift
        // variant is not that command: it is Ctrl+Shift+Left. Backspace's
        // shifted chord is delete-column itself. Delete was never a table
        // chord, and extra Alt does not fire delete-word.
        let map = KeyMap::builtin();
        let ctrl_alt = Modifiers::COMMAND.plus(Modifiers::ALT);
        let ctrl_alt_shift = ctrl_alt.plus(Modifiers::SHIFT);
        assert_eq!(
            map.editor_gesture(Key::ArrowLeft, ctrl_alt),
            Some((Action::InsertColumnLeft, false))
        );
        assert_eq!(
            map.editor_gesture(Key::ArrowLeft, ctrl_alt_shift),
            Some((Action::WordLeft, true))
        );
        assert_eq!(
            map.editor_gesture(Key::ArrowRight, ctrl_alt_shift),
            Some((Action::WordRight, true))
        );
        assert_eq!(
            map.editor_gesture(Key::Backspace, ctrl_alt_shift),
            Some((Action::DeleteColumn, false))
        );
        // Not a table chord, so extra Alt does not fire delete-word.
        assert_eq!(map.editor_gesture(Key::Delete, ctrl_alt_shift), None);
        // Shift on insert-row or format isn't a motion, so nothing claims it.
        assert_eq!(map.editor_gesture(Key::ArrowUp, ctrl_alt_shift), None);
        assert_eq!(map.editor_gesture(Key::F, ctrl_alt_shift), None);
    }

    #[test]
    fn overrides_keep_the_rest_and_an_empty_list_unbinds() {
        let applied = KeyMap::apply(&[entry("bold", &["Ctrl+Shift+L"]), entry("italic", &[])]);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        assert_eq!(applied.map.shortcut_text(Action::Bold), "Ctrl+Shift+L");
        assert!(applied.map.chords(Action::Italic).is_empty());
        assert_eq!(applied.map.shortcut_text(Action::Save), "Ctrl+S");
    }

    #[test]
    fn a_swap_is_not_a_conflict() {
        let applied = KeyMap::apply(&[entry("bold", &["Ctrl+I"]), entry("italic", &["Ctrl+B"])]);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        assert_eq!(applied.map.shortcut_text(Action::Bold), "Ctrl+I");
        assert_eq!(applied.map.shortcut_text(Action::Italic), "Ctrl+B");
    }

    #[test]
    fn unknown_unparseable_and_shared_chords_keep_that_entry_default() {
        let applied = KeyMap::apply(&[
            entry("not_an_action", &["Ctrl+Q"]),
            entry("bold", &["Ctrl+", "Ctrl+J"]),
            entry("italic", &["Ctrl+K"]),
            entry("link", &["Ctrl+J"]),
            entry("code", &["Ctrl+E"]),
        ]);
        let text = applied.errors.join("\n");
        assert!(
            text.contains("Unknown key action \"not_an_action\""),
            "{text}"
        );
        assert!(text.contains("keys.bold"), "{text}");
        assert!(text.contains("keys.code"), "{text}");
        // The broken bold entry didn't take Ctrl+J, so link could, which
        // frees Ctrl+K for italic. code still loses to cycle_mode.
        assert_eq!(applied.map.shortcut_text(Action::Bold), "Ctrl+B");
        assert_eq!(applied.map.shortcut_text(Action::Italic), "Ctrl+K");
        assert_eq!(applied.map.shortcut_text(Action::Link), "Ctrl+J");
        // cycle_mode still owns Ctrl+E, so code stays on its default.
        assert_eq!(applied.map.shortcut_text(Action::Code), "Ctrl+Backtick");
        assert_eq!(applied.map.shortcut_text(Action::CycleMode), "Ctrl+E");
    }

    #[test]
    fn a_rebind_that_only_fits_because_a_rejected_entry_moved_is_rejected_too() {
        // italic takes link's chord and is rejected, so it snaps back to
        // Ctrl+I. bold can sit on Ctrl+I only while italic is away.
        let applied = KeyMap::apply(&[entry("bold", &["Ctrl+I"]), entry("italic", &["Ctrl+K"])]);
        assert!(
            applied.errors.iter().any(|e| e.contains("keys.bold")),
            "{:?}",
            applied.errors
        );
        assert!(
            applied.errors.iter().any(|e| e.contains("keys.italic")),
            "{:?}",
            applied.errors
        );
        assert_eq!(applied.map.shortcut_text(Action::Bold), "Ctrl+B");
        assert_eq!(applied.map.shortcut_text(Action::Italic), "Ctrl+I");
        assert_eq!(applied.map.shortcut_text(Action::Link), "Ctrl+K");
    }

    #[test]
    fn one_bad_chord_in_a_list_rejects_the_whole_entry() {
        let applied = KeyMap::apply(&[entry("insert_row_below", &["Ctrl+Alt+Down", "nope"])]);
        assert!(
            applied
                .errors
                .iter()
                .any(|e| e.contains("insert_row_below"))
        );
        assert_eq!(
            applied.map.shortcut_text(Action::InsertRowBelow),
            "Ctrl+Alt+Down"
        );
    }

    #[test]
    fn a_desktop_chord_is_kept_and_warned_about() {
        let applied = KeyMap::apply(&[
            entry("bold", &["Ctrl+Alt+Delete"]),
            entry("italic", &["Super+I"]),
            entry("link", &["Alt+Tab"]),
        ]);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        assert_eq!(applied.map.shortcut_text(Action::Bold), "Ctrl+Alt+Delete");
        assert_eq!(applied.map.shortcut_text(Action::Italic), "Super+I");
        let warnings = applied.warnings.join("\n");
        assert!(warnings.contains("closes all windows"), "{warnings}");
        assert!(warnings.contains("Super"), "{warnings}");
        assert!(warnings.contains("switches windows"), "{warnings}");
        assert!(
            KeyMap::builtin().desktop_warnings().is_empty(),
            "defaults must not use a desktop chord"
        );
    }

    #[test]
    fn listed_keys_round_trip_through_the_parser() {
        let map = KeyMap::builtin();
        let text = map.to_toml();
        assert!(text.starts_with("# Key bindings"));
        assert!(text.contains("redo = [\"Ctrl+Shift+Z\", \"Ctrl+Y\"]"));
        assert!(text.contains("bold = \"Ctrl+B\""));
        // The same strings parse back. Comments and the header aren't chords.
        let mut entries = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                continue;
            }
            let (name, value) = line.split_once(" = ").unwrap();
            if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
                let chords = if inner.is_empty() {
                    Vec::new()
                } else {
                    inner
                        .split(", ")
                        .map(|part| part.trim_matches('"').to_owned())
                        .collect()
                };
                entries.push((name.to_owned(), chords));
            } else {
                entries.push((name.to_owned(), vec![value.trim_matches('"').to_owned()]));
            }
        }
        let applied = KeyMap::apply(&entries);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        assert_eq!(applied.map, map);
    }

    #[test]
    fn the_readme_lists_these_shortcuts() {
        let readme = include_str!("../../../README.md");
        let table = KeyMap::readme_table();
        assert!(
            readme.contains(table.trim_end()),
            "README shortcut table drifted from KeyMap::readme_table"
        );
    }

    #[test]
    fn the_readme_table_names_every_default() {
        let table = KeyMap::readme_table();
        assert!(table.starts_with("| Keys | Action |\n|---|---|\n"));
        assert!(table.contains("| Ctrl+E | Cycle split, code, and live (`cycle_mode`) |"));
        assert!(table.contains("`insert_row_below`"));
        assert!(table.contains("Ctrl+Alt+Shift+Backspace"));
        assert!(!table.contains("Ctrl+click"));
        for action in Action::ALL {
            assert!(
                table.contains(&format!("`{}`", action.id())),
                "missing {}",
                action.id()
            );
        }
    }

    #[test]
    fn every_table_chord_is_what_dispatch_fires() {
        // The table is a literal, not a walk of `Action::ALL`. This fails
        // when a row is missing, when a second chord such as F1 is missing,
        // or when `find` stops reading the rows.
        let map = KeyMap::builtin();
        assert_eq!(TABLE.len(), Action::ALL.len());
        assert_eq!(map.bindings().len(), TABLE.len());
        let mut seen = [false; COUNT];
        for (i, spec) in TABLE.iter().enumerate() {
            assert_eq!(spec.action.index(), i, "{}", spec.id);
            assert_eq!(Action::ALL[i], spec.action, "{}", spec.id);
            assert_eq!(spec.action.id(), spec.id);
            assert!(!spec.description.is_empty(), "{}", spec.id);
            assert!(!spec.chords.is_empty(), "{} has no chord", spec.id);
            let row = &map.bindings()[i];
            assert!(!seen[spec.action.index()], "twice: {}", spec.id);
            seen[spec.action.index()] = true;
            assert_eq!(row.action, spec.action);
            assert_eq!(row.group, spec.group);
            assert_eq!(row.scope, spec.scope);
            assert_eq!(row.description, spec.description);
            assert_eq!(row.chords, spec.chords);
            for chord in spec.chords {
                let found = match spec.scope {
                    Scope::Editor | Scope::Table => map.find_editing(chord.key, chord.modifiers),
                    scope => map.find(chord.key, chord.modifiers, scope),
                };
                assert_eq!(
                    found,
                    Some(spec.action),
                    "{} via {}",
                    spec.id,
                    chord.display()
                );
            }
        }
        assert!(seen.iter().all(|hit| *hit));
        // Both chords, not only the first. F1 is the alternate for Ctrl+/.
        let show = TABLE
            .iter()
            .find(|spec| spec.action == Action::ShowKeys)
            .unwrap();
        assert_eq!(show.chords.len(), 2);
        assert_eq!(
            map.find(Key::F1, Modifiers::NONE, Scope::App),
            Some(Action::ShowKeys)
        );
        assert_eq!(
            map.find(Key::Slash, Modifiers::COMMAND, Scope::App),
            Some(Action::ShowKeys)
        );
        // The same chord, two scopes: the editor toggles a task, search pins.
        assert_eq!(
            map.find_editing(Key::Enter, Modifiers::COMMAND),
            Some(Action::ToggleTask)
        );
        assert_eq!(
            map.find(Key::Enter, Modifiers::COMMAND, Scope::Search),
            Some(Action::PinSearchResult)
        );
        assert_eq!(
            map.find(Key::Enter, Modifiers::COMMAND, Scope::Editor),
            Some(Action::ToggleTask)
        );
        let reopen = map
            .bindings()
            .iter()
            .find(|row| row.action == Action::ReopenTab)
            .unwrap();
        assert_eq!(reopen.group, Group::Tabs);
        assert_eq!(reopen.chords, &[chord(CTRL_SHIFT, Key::T)]);
        assert!(map.bindings().iter().any(|row| row.action == Action::Tab1
            && row.group == Group::Tabs
            && row.chords == [chord(ALT, Key::Num1)]));
        for group in Group::ALL {
            assert!(
                map.bindings().iter().any(|row| row.group == group),
                "no row in {}",
                group.label()
            );
        }
        assert!(GESTURES.iter().any(|gesture| {
            gesture.group == Group::Find && gesture.keys == "Middle click a search result"
        }));
    }

    #[test]
    fn search_may_share_an_editor_chord_but_not_an_app_chord() {
        let shared = KeyMap::apply(&[entry("pin_search_result", &["Ctrl+B"])]);
        assert!(shared.errors.is_empty(), "{:?}", shared.errors);
        assert_eq!(shared.map.shortcut_text(Action::PinSearchResult), "Ctrl+B");
        assert_eq!(shared.map.shortcut_text(Action::Bold), "Ctrl+B");
        assert_eq!(
            shared.map.find(Key::B, Modifiers::COMMAND, Scope::Search),
            Some(Action::PinSearchResult)
        );
        assert_eq!(
            shared.map.find_editing(Key::B, Modifiers::COMMAND),
            Some(Action::Bold)
        );

        let app = KeyMap::apply(&[entry("pin_search_result", &["Ctrl+S"])]);
        let text = app.errors.join("\n");
        assert!(text.contains("pin_search_result"), "{text}");
        assert!(text.contains("save"), "{text}");
        assert_eq!(app.map.shortcut_text(Action::PinSearchResult), "Ctrl+Enter");
        assert_eq!(app.map.shortcut_text(Action::Save), "Ctrl+S");
    }

    #[test]
    fn the_overlay_query_matches_the_action_name_or_the_key() {
        let map = KeyMap::builtin();
        let os = egui::os::OperatingSystem::Nix;
        let hit = |query: &str, action: Action| map.matches_query(action, os, query);
        assert!(hit("reopen", Action::ReopenTab));
        assert!(hit("REOPEN", Action::ReopenTab));
        assert!(hit("ctrl+shift+t", Action::ReopenTab));
        assert!(!hit("reopen", Action::Bold));
        assert!(hit("show_keys", Action::ShowKeys));
        assert!(hit("ctrl+/", Action::ShowKeys));
        assert!(hit("f1", Action::ShowKeys));
        assert!(hit("", Action::Bold));
        assert!(hit("   ", Action::Save));
        assert!(
            GESTURES
                .iter()
                .any(|gesture| gesture.matches_query("ctrl+click"))
        );
        assert!(!GESTURES.iter().any(|gesture| gesture.matches_query("bold")));
    }

    #[test]
    fn chords_say_ctrl_on_linux_and_cmd_on_mac() {
        let map = KeyMap::builtin();
        let linux = egui::os::OperatingSystem::Nix;
        let mac = egui::os::OperatingSystem::Mac;
        assert!(!linux.is_mac());
        assert_eq!(map.platform_text(linux, Action::Bold), "Ctrl+B");
        assert_eq!(map.platform_text(linux, Action::ReopenTab), "Ctrl+Shift+T");
        assert_eq!(map.platform_text(linux, Action::ShowKeys), "Ctrl+/, F1");
        assert_eq!(map.platform_text(linux, Action::Back), "Alt+Left");
        assert_eq!(map.shortcut_text(Action::Bold), "Ctrl+B");
        assert_eq!(map.platform_text(mac, Action::Bold), "Cmd+B");
        assert_eq!(map.platform_text(mac, Action::Redo), "Shift+Cmd+Z, Cmd+Y");
        assert_eq!(map.platform_text(mac, Action::Back), "Option+Left");
        let super_i = Chord::parse("Super+I").unwrap();
        assert_eq!(super_i.platform_label(linux), "Super+I");
        assert_eq!(super_i.platform_label(mac), "Cmd+I");
        assert_eq!(Chord::parse("Ctrl+/").unwrap().display(), "Ctrl+/");
    }
}
