//! Ctrl+G. A line number, 1-based, and the caret goes there.

use eframe::egui::{self, Key, Modifiers, TextEdit};

const FIELD: &str = "goto_line";

/// What the bar did this frame.
#[derive(Default)]
pub struct GoToStep {
    /// A 0-based line, already clamped to the document.
    pub line: Option<usize>,
    pub closed: bool,
}

#[derive(Default)]
pub struct GoToLine {
    open: bool,
    text: String,
    /// Focus the field on the next draw.
    pending: bool,
}

impl GoToLine {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.open = true;
        self.text.clear();
        self.pending = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Draws the bar. Enter and Go jump when the field holds a line number.
    /// Escape closes the bar. An empty or non-numeric field stays as it is.
    pub fn show(&mut self, ui: &mut egui::Ui, line_count: usize, modal: bool) -> GoToStep {
        let id = egui::Id::new(FIELD);
        let focused = ui.memory(|m| m.has_focus(id));
        let (enter, escape) = ui.input_mut(|input| {
            let escape = if modal {
                false
            } else {
                input.consume_key(Modifiers::NONE, Key::Escape)
            };
            let enter = if focused {
                input.consume_key(Modifiers::NONE, Key::Enter)
            } else {
                false
            };
            (enter, escape)
        });
        if escape {
            self.open = false;
            return GoToStep {
                line: None,
                closed: true,
            };
        }

        let mut go = enter;
        ui.horizontal(|ui| {
            ui.label("Go to line");
            let edit = TextEdit::singleline(&mut self.text)
                .id(id)
                .desired_width(120.0)
                .show(ui);
            if self.pending {
                self.pending = false;
                edit.response.request_focus();
            }
            if ui.button("Go").clicked() {
                go = true;
            }
        });
        if !go {
            return GoToStep::default();
        }
        match parse_line(&self.text, line_count) {
            Some(line) => {
                self.open = false;
                GoToStep {
                    line: Some(line),
                    closed: true,
                }
            }
            None => GoToStep::default(),
        }
    }
}

/// `text` as a 1-based line, clamped to `line_count`. Empty, zero, and
/// anything that is not a whole number are declined.
fn parse_line(text: &str, line_count: usize) -> Option<usize> {
    if line_count == 0 {
        return None;
    }
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let number: usize = text.parse().ok()?;
    if number == 0 {
        return None;
    }
    Some((number - 1).min(line_count - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_number_is_one_based_and_clamped() {
        assert_eq!(parse_line("1", 3), Some(0));
        assert_eq!(parse_line(" 2 ", 3), Some(1));
        assert_eq!(parse_line("3", 3), Some(2));
        assert_eq!(parse_line("99", 3), Some(2));
        assert_eq!(parse_line("0", 3), None);
        assert_eq!(parse_line("", 3), None);
        assert_eq!(parse_line("no", 3), None);
        assert_eq!(parse_line("2a", 3), None);
        assert_eq!(parse_line("1", 0), None);
    }
}
