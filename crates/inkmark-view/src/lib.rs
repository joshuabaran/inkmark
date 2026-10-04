//! CodeView, LiveView, the file browser, reveal rules, smart edit rules, hit-test, scroll sync.

mod browser;
mod code_view;
mod commands;
mod folds;
mod images;
pub mod keys;
mod lines;
mod live_layout;
mod live_view;
pub mod motion;
mod structure;
mod tables;
pub mod theme;

pub use browser::{BrowserOutput, FileBrowser};

/// Ctrl+click on a link in a pane: while Ctrl is held over link text the
/// pointer is a hand, and a press there returns the link's offset instead
/// of moving the caret. `offset_at` maps the pointer to a source offset.
pub(crate) fn link_under_pointer(
    ui: &egui::Ui,
    response: &egui::Response,
    parse: Option<&inkmark_parse::ParseOutput>,
    offset_at: impl FnOnce(egui::Pos2) -> usize,
) -> Option<(usize, bool)> {
    let (ctrl, pressed, pos) = ui.input(|i| {
        (
            i.modifiers.command,
            i.pointer.primary_pressed(),
            i.pointer.interact_pos(),
        )
    });
    let (Some(parse), Some(pos)) = (parse, pos) else {
        return None;
    };
    if !ctrl || !response.hovered() {
        return None;
    }
    let at = offset_at(pos);
    // The caret position nearest the pointer can be just after the link's
    // last character.
    let at = [at, at.saturating_sub(1)]
        .into_iter()
        .find(|&a| inkmark_parse::is_link(parse, a))?;
    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    Some((at, pressed))
}
pub use code_view::CodeView;
pub use lines::ScrollPos;
pub use live_view::LiveView;
