//! Every configurable shortcut, and the chord it uses unless config says
//! otherwise. The app shell, the file browser and both editor panes read
//! this one table.
//!
//! Plain editing keys are not actions: arrows, Backspace, Delete, Enter,
//! Tab, Home, End, Page Up/Down, and Shift held to extend a selection.
//! Their Ctrl and Alt chords are actions, so they can be rebound. Mouse
//! gestures (Ctrl+click) and IME are not actions either.
//!
//! Matching requires Shift and Alt to be exactly the ones named. egui's
//! `consume_shortcut` uses `matches_logically`, which treats extra Shift
//! or Alt as a hit, so Alt+Shift+Left would fire Back (Alt+Left) and
//! Ctrl+Shift+E would fire the mode cycle (Ctrl+E). `matches_exact` keeps
//! Shift and Alt strict and still treats Linux Ctrl (the event sets both
//! `ctrl` and `command`) as the Ctrl chord. Comparing modifiers with `==`
//! would miss those real Ctrl events, which tests build as `command` only.

use egui::{Key, Modifiers};

/// Where an action is handled. A chord belongs to one action everywhere;
/// two actions never share one, even across these.
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
}

const COUNT: usize = 54;

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
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    /// The `[keys]` name.
    pub fn id(self) -> &'static str {
        use Action::*;
        match self {
            CycleMode => "cycle_mode",
            FocusCode => "focus_code",
            FocusLive => "focus_live",
            ToggleMinimap => "toggle_minimap",
            OpenFile => "open_file",
            Save => "save",
            SaveAs => "save_as",
            OpenFolder => "open_folder",
            ToggleSidebar => "toggle_sidebar",
            NewFile => "new_file",
            RecentFiles => "recent_files",
            Back => "back",
            Forward => "forward",
            Rename => "rename",
            MoveToTrash => "move_to_trash",
            Undo => "undo",
            Redo => "redo",
            Bold => "bold",
            Italic => "italic",
            Code => "code",
            Strikethrough => "strikethrough",
            Link => "link",
            ToggleTask => "toggle_task",
            Heading0 => "heading_0",
            Heading1 => "heading_1",
            Heading2 => "heading_2",
            Heading3 => "heading_3",
            Heading4 => "heading_4",
            Heading5 => "heading_5",
            Heading6 => "heading_6",
            SelectAll => "select_all",
            WordLeft => "word_left",
            WordRight => "word_right",
            DeleteWordLeft => "delete_word_left",
            DeleteWordRight => "delete_word_right",
            DocumentStart => "document_start",
            DocumentEnd => "document_end",
            InsertRowAbove => "insert_row_above",
            InsertRowBelow => "insert_row_below",
            InsertColumnLeft => "insert_column_left",
            InsertColumnRight => "insert_column_right",
            DeleteRow => "delete_row",
            DeleteColumn => "delete_column",
            MoveRowUp => "move_row_up",
            MoveRowDown => "move_row_down",
            MoveColumnLeft => "move_column_left",
            MoveColumnRight => "move_column_right",
            FormatTable => "format_table",
            InsertTable => "insert_table",
            Find => "find",
            Replace => "replace",
            FindNext => "find_next",
            FindPrevious => "find_previous",
            GoToLine => "go_to_line",
        }
    }

    /// One line for the README and for `--list-keys`.
    pub fn description(self) -> &'static str {
        use Action::*;
        match self {
            CycleMode => "Cycle split, code, and live",
            FocusCode => "Focus the code pane (switching to it when only live shows)",
            FocusLive => "Focus the live pane (switching to it when only code shows)",
            ToggleMinimap => "Toggle the focused pane's minimap",
            OpenFile => "Open a file",
            Save => "Save",
            SaveAs => "Save as",
            OpenFolder => "Open a folder in the sidebar",
            ToggleSidebar => "Show or hide the sidebar",
            NewFile => "New Markdown file in the selected folder, or the open folder",
            RecentFiles => "Recent files",
            Back => "Back to where you followed the last link from",
            Forward => "Forward again",
            Rename => "Rename the selected file or folder",
            MoveToTrash => "Move the selected file or folder to the trash",
            Undo => "Undo",
            Redo => "Redo",
            Bold => "Toggle bold",
            Italic => "Toggle italic",
            Code => "Toggle code",
            Strikethrough => "Toggle strikethrough",
            Link => "Insert a link",
            ToggleTask => "Toggle the line's task checkbox",
            Heading0 => "Turn the line into a paragraph",
            Heading1 => "Set heading level 1",
            Heading2 => "Set heading level 2",
            Heading3 => "Set heading level 3",
            Heading4 => "Set heading level 4",
            Heading5 => "Set heading level 5",
            Heading6 => "Set heading level 6",
            SelectAll => "Select all",
            WordLeft => "Move to the previous word",
            WordRight => "Move to the next word",
            DeleteWordLeft => "Delete the previous word",
            DeleteWordRight => "Delete the next word",
            DocumentStart => "Move to the start of the document",
            DocumentEnd => "Move to the end of the document",
            InsertRowAbove => "Insert a table row above",
            InsertRowBelow => "Insert a table row below",
            InsertColumnLeft => "Insert a table column to the left",
            InsertColumnRight => "Insert a table column to the right",
            DeleteRow => "Delete the table row",
            DeleteColumn => "Delete the table column",
            MoveRowUp => "Move the table row up",
            MoveRowDown => "Move the table row down",
            MoveColumnLeft => "Move the table column left",
            MoveColumnRight => "Move the table column right",
            FormatTable => "Line up the table's pipes",
            InsertTable => "Insert a 3×3 table",
            Find => "Find in this file",
            Replace => "Replace in this file",
            FindNext => "Find the next match",
            FindPrevious => "Find the previous match",
            GoToLine => "Go to line",
        }
    }

    pub fn scope(self) -> Scope {
        use Action::*;
        match self {
            CycleMode | FocusCode | FocusLive | ToggleMinimap | OpenFile | Save | SaveAs
            | OpenFolder | ToggleSidebar | NewFile | RecentFiles | Back | Forward | Find
            | Replace | FindNext | FindPrevious | GoToLine => Scope::App,
            Rename | MoveToTrash => Scope::Browser,
            InsertRowAbove | InsertRowBelow | InsertColumnLeft | InsertColumnRight | DeleteRow
            | DeleteColumn | MoveRowUp | MoveRowDown | MoveColumnLeft | MoveColumnRight
            | FormatTable | InsertTable => Scope::Table,
            _ => Scope::Editor,
        }
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

    /// `Ctrl+Shift+S`, `Alt+Left`, `F2`. Modifier order is fixed so the
    /// same chord always prints the same way.
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.command || self.modifiers.ctrl {
            parts.push("Ctrl");
        }
        if self.modifiers.alt {
            parts.push("Alt");
        }
        if self.modifiers.shift {
            parts.push("Shift");
        }
        if self.modifiers.mac_cmd {
            parts.push("Super");
        }
        parts.push(self.key.name());
        parts.join("+")
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyMap {
    chords: Vec<Vec<Chord>>,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::builtin()
    }
}

impl KeyMap {
    pub fn builtin() -> Self {
        let chords = Action::ALL.into_iter().map(default_chords).collect();
        Self { chords }
    }

    pub fn chords(&self, action: Action) -> &[Chord] {
        &self.chords[action.index()]
    }

    pub fn set(&mut self, action: Action, chords: Vec<Chord>) {
        self.chords[action.index()] = chords;
    }

    /// The label drawn next to a menu item. Every chord, so a rebound
    /// action doesn't keep advertising the old key.
    pub fn shortcut_text(&self, action: Action) -> String {
        self.chords(action)
            .iter()
            .map(Chord::display)
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn find(&self, key: Key, modifiers: Modifiers, scope: Scope) -> Option<Action> {
        self.find_where(key, modifiers, |action| action.scope() == scope)
    }

    /// Editor chords and table chords, which the focused pane both sees.
    pub fn find_editing(&self, key: Key, modifiers: Modifiers) -> Option<Action> {
        self.find_where(key, modifiers, |action| {
            matches!(action.scope(), Scope::Editor | Scope::Table)
        })
    }

    fn find_where(
        &self,
        key: Key,
        modifiers: Modifiers,
        want: impl Fn(Action) -> bool,
    ) -> Option<Action> {
        Action::ALL.into_iter().find(|action| {
            want(*action)
                && self
                    .chords(*action)
                    .iter()
                    .any(|chord| chord.matches(key, modifiers))
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
        for action in Action::ALL {
            for chord in self.chords(action) {
                if let Some((_, owners)) = found.iter_mut().find(|(have, _)| have == chord) {
                    owners.push(action);
                } else {
                    found.push((*chord, vec![action]));
                }
            }
        }
        found.retain(|(_, owners)| owners.len() > 1);
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

fn chord(modifiers: Modifiers, key: Key) -> Chord {
    Chord { modifiers, key }
}

fn default_chords(action: Action) -> Vec<Chord> {
    use Action::*;
    let ctrl = Modifiers::COMMAND;
    let alt = Modifiers::ALT;
    let shift = Modifiers::SHIFT;
    let ctrl_shift = ctrl.plus(shift);
    let ctrl_alt = ctrl.plus(alt);
    let alt_shift = alt.plus(shift);
    let ctrl_alt_shift = ctrl_alt.plus(shift);
    match action {
        CycleMode => vec![chord(ctrl, Key::E)],
        FocusCode => vec![chord(ctrl, Key::Num1)],
        FocusLive => vec![chord(ctrl, Key::Num2)],
        ToggleMinimap => vec![chord(ctrl, Key::M)],
        OpenFile => vec![chord(ctrl, Key::O)],
        Save => vec![chord(ctrl, Key::S)],
        SaveAs => vec![chord(ctrl_shift, Key::S)],
        OpenFolder => vec![chord(ctrl_shift, Key::O)],
        ToggleSidebar => vec![chord(ctrl_shift, Key::E)],
        NewFile => vec![chord(ctrl, Key::N)],
        RecentFiles => vec![chord(ctrl, Key::R)],
        Back => vec![chord(alt, Key::ArrowLeft)],
        Forward => vec![chord(alt, Key::ArrowRight)],
        Rename => vec![chord(Modifiers::NONE, Key::F2)],
        MoveToTrash => vec![chord(Modifiers::NONE, Key::Delete)],
        Undo => vec![chord(ctrl, Key::Z)],
        Redo => vec![chord(ctrl_shift, Key::Z), chord(ctrl, Key::Y)],
        Bold => vec![chord(ctrl, Key::B)],
        Italic => vec![chord(ctrl, Key::I)],
        Code => vec![chord(ctrl, Key::Backtick)],
        Strikethrough => vec![chord(ctrl_shift, Key::X)],
        Link => vec![chord(ctrl, Key::K)],
        ToggleTask => vec![chord(ctrl, Key::Enter)],
        Heading0 => vec![chord(ctrl_alt, Key::Num0)],
        Heading1 => vec![chord(ctrl_alt, Key::Num1)],
        Heading2 => vec![chord(ctrl_alt, Key::Num2)],
        Heading3 => vec![chord(ctrl_alt, Key::Num3)],
        Heading4 => vec![chord(ctrl_alt, Key::Num4)],
        Heading5 => vec![chord(ctrl_alt, Key::Num5)],
        Heading6 => vec![chord(ctrl_alt, Key::Num6)],
        SelectAll => vec![chord(ctrl, Key::A)],
        WordLeft => vec![chord(ctrl, Key::ArrowLeft)],
        WordRight => vec![chord(ctrl, Key::ArrowRight)],
        DeleteWordLeft => vec![chord(ctrl, Key::Backspace)],
        DeleteWordRight => vec![chord(ctrl, Key::Delete)],
        DocumentStart => vec![chord(ctrl, Key::Home)],
        DocumentEnd => vec![chord(ctrl, Key::End)],
        InsertRowAbove => vec![chord(ctrl_alt, Key::ArrowUp)],
        InsertRowBelow => vec![chord(ctrl_alt, Key::ArrowDown)],
        InsertColumnLeft => vec![chord(ctrl_alt, Key::ArrowLeft)],
        InsertColumnRight => vec![chord(ctrl_alt, Key::ArrowRight)],
        DeleteRow => vec![chord(ctrl_alt, Key::Backspace)],
        DeleteColumn => vec![chord(ctrl_alt_shift, Key::Backspace)],
        MoveRowUp => vec![chord(alt_shift, Key::ArrowUp)],
        MoveRowDown => vec![chord(alt_shift, Key::ArrowDown)],
        MoveColumnLeft => vec![chord(alt_shift, Key::ArrowLeft)],
        MoveColumnRight => vec![chord(alt_shift, Key::ArrowRight)],
        FormatTable => vec![chord(ctrl_alt, Key::F)],
        InsertTable => vec![chord(ctrl_alt, Key::T)],
        Find => vec![chord(ctrl, Key::F)],
        Replace => vec![chord(ctrl, Key::H)],
        FindNext => vec![chord(Modifiers::NONE, Key::F3)],
        FindPrevious => vec![chord(shift, Key::F3)],
        GoToLine => vec![chord(ctrl, Key::G)],
    }
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
        let applied = KeyMap::apply(&[entry("bold", &["Ctrl+Shift+B"]), entry("italic", &[])]);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        assert_eq!(applied.map.shortcut_text(Action::Bold), "Ctrl+Shift+B");
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
}
