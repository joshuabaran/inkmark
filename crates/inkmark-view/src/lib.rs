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
mod long_line;
mod math;
pub mod motion;
mod structure;
mod tables;
pub mod theme;

pub use browser::{BrowserOutput, FileBrowser, OpenFile};
pub use images::ImageCache;

/// A click that follows a link. Ctrl+click opens the preview tab.
/// A middle click opens a pinned tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkClick {
    pub at: usize,
    pub pin: bool,
}

fn link_at(
    response: &egui::Response,
    parse: Option<&inkmark_parse::ParseOutput>,
    pos: Option<egui::Pos2>,
    offset_at: &mut impl FnMut(egui::Pos2) -> usize,
) -> Option<usize> {
    let (Some(parse), Some(pos)) = (parse, pos) else {
        return None;
    };
    if !response.hovered() {
        return None;
    }
    let at = offset_at(pos);
    // The caret position nearest the pointer can be just after the link's
    // last character.
    [at, at.saturating_sub(1)]
        .into_iter()
        .find(|&offset| inkmark_parse::is_link(parse, offset))
}

/// Ctrl+click on a link in a pane: while Ctrl is held over link text the
/// pointer is a hand, and a press there returns the link's offset instead
/// of moving the caret. `offset_at` maps the pointer to a source offset.
pub(crate) fn link_under_pointer(
    ui: &egui::Ui,
    response: &egui::Response,
    parse: Option<&inkmark_parse::ParseOutput>,
    mut offset_at: impl FnMut(egui::Pos2) -> usize,
) -> Option<(usize, bool)> {
    let (ctrl, pressed, pos) = ui.input(|i| {
        (
            i.modifiers.command,
            i.pointer.primary_pressed(),
            i.pointer.interact_pos(),
        )
    });
    if !ctrl {
        return None;
    }
    let at = link_at(response, parse, pos, &mut offset_at)?;
    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    Some((at, pressed))
}

/// Middle click on link text. The caller opens that link in a pinned tab.
pub(crate) fn middle_clicked_link(
    response: &egui::Response,
    parse: Option<&inkmark_parse::ParseOutput>,
    mut offset_at: impl FnMut(egui::Pos2) -> usize,
) -> Option<usize> {
    if !response.middle_clicked() {
        return None;
    }
    link_at(
        response,
        parse,
        response.interact_pointer_pos(),
        &mut offset_at,
    )
}
pub use code_view::CodeView;
pub use lines::ScrollPos;
pub use live_view::LiveView;
