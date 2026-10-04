//! Which panes are open, each minimap, and the widths of the split and the
//! outline. Stored next to the recent-files list in
//! `$XDG_STATE_HOME/inkmark/layout`.

use std::path::PathBuf;

use crate::outline;

/// Each side of the split keeps this much when the editor is wide enough.
pub(crate) const PANE_MIN: f32 = 160.0;
/// The drawn gap between the code pane and the live pane.
pub(crate) const SPLIT_GAP: f32 = 2.0;
/// The drag target is wider than the drawn gap, so the line can be grabbed.
pub(crate) const SPLIT_HIT: f32 = 6.0;

const DEFAULT_FRACTION: f32 = 0.5;
/// A stored fraction stays inside this range. The window may still draw the
/// split closer to the middle, and that draw does not replace the stored value.
const FRACTION_MIN: f32 = 0.02;
const FRACTION_MAX: f32 = 0.98;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Split,
    Code,
    Live,
}

impl Mode {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "split" => Some(Self::Split),
            "code" => Some(Self::Code),
            "live" => Some(Self::Live),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Code => "code",
            Self::Live => "live",
        }
    }
}

pub(crate) struct Layout {
    store: Option<PathBuf>,
    pub mode: Mode,
    pub code_minimap: bool,
    pub live_minimap: bool,
    /// The code pane's share of the split, before a narrow window pulls it in.
    pub code_fraction: f32,
    /// Outline width the user set. A narrow window may draw it smaller.
    pub outline_width: f32,
}

impl Layout {
    pub(crate) fn load(store: Option<PathBuf>) -> Self {
        let mut mode = Mode::Split;
        let mut code_minimap = true;
        let mut live_minimap = true;
        let mut code_fraction = DEFAULT_FRACTION;
        let mut outline_width = outline::PREFERRED_WIDTH;
        if let Some(path) = &store
            && let Ok(text) = std::fs::read_to_string(path)
        {
            let mut lines = text.lines();
            if let Some(word) = lines.next()
                && let Some(parsed) = Mode::parse(word)
            {
                mode = parsed;
            }
            if let Some(flag) = lines.next() {
                code_minimap = flag != "0";
            }
            if let Some(flag) = lines.next() {
                live_minimap = flag != "0";
            }
            if let Some(parsed) = lines.next().and_then(|line| line.parse::<f32>().ok())
                && parsed.is_finite()
            {
                code_fraction = parsed.clamp(FRACTION_MIN, FRACTION_MAX);
            }
            if let Some(parsed) = lines.next().and_then(|line| line.parse::<f32>().ok())
                && parsed.is_finite()
            {
                outline_width = parsed.clamp(outline::MIN_WIDTH, outline::MAX_WIDTH);
            }
        }
        Self {
            store,
            mode,
            code_minimap,
            live_minimap,
            code_fraction,
            outline_width,
        }
    }

    pub(crate) fn save(&self) {
        let Some(store) = &self.store else {
            return;
        };
        if let Some(dir) = store.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let code = if self.code_minimap { "1" } else { "0" };
        let live = if self.live_minimap { "1" } else { "0" };
        let _ = std::fs::write(
            store,
            format!(
                "{}\n{code}\n{live}\n{}\n{}\n",
                self.mode.as_str(),
                self.code_fraction,
                self.outline_width,
            ),
        );
    }

    /// Where to put the divider in an editor area `width` pixels wide.
    /// A window too narrow for both minimums uses the middle and leaves
    /// [`Self::code_fraction`] alone.
    pub(crate) fn split_fraction(&self, width: f32) -> f32 {
        match split_limits(width) {
            Some((lo, hi)) => self.code_fraction.clamp(lo, hi),
            None => DEFAULT_FRACTION,
        }
    }

    /// Moves the divider from where it was drawn (`mid`) by `delta_x`, the
    /// pointer's travel since the press. The fraction is the code pane's
    /// share of the width beside the gap. A window too narrow for both
    /// minimums leaves the stored fraction alone.
    pub(crate) fn drag_split(&mut self, left: f32, width: f32, mid: f32, delta_x: f32) {
        let Some((lo, hi)) = split_limits(width) else {
            return;
        };
        if !mid.is_finite() || !delta_x.is_finite() {
            return;
        }
        let usable = width - SPLIT_GAP;
        let raw = (mid + delta_x - left - SPLIT_GAP / 2.0) / usable;
        if raw.is_finite() {
            self.code_fraction = raw.clamp(lo, hi);
        }
    }

    /// Sets the outline width from the width on screen when the drag began.
    /// `delta_x` is the pointer's travel since the press; right shrinks the
    /// outline. Past either limit, the edge stays put until the pointer
    /// comes back.
    pub(crate) fn drag_outline(&mut self, drawn: f32, delta_x: f32) {
        if !drawn.is_finite() || !delta_x.is_finite() {
            return;
        }
        self.outline_width = (drawn - delta_x).clamp(outline::MIN_WIDTH, outline::MAX_WIDTH);
    }
}

/// X of the divider. `fraction` is the code pane's share of `width`.
pub(crate) fn split_mid(left: f32, width: f32, fraction: f32) -> f32 {
    let usable = (width - SPLIT_GAP).max(0.0);
    let left_w = (usable * fraction).round();
    left + left_w + SPLIT_GAP / 2.0
}

/// Inclusive range for the code pane's share, when `width` can hold both
/// minimums. `None` when the window is too narrow to offer a choice.
fn split_limits(width: f32) -> Option<(f32, f32)> {
    let usable = width - SPLIT_GAP;
    if !width.is_finite() || width <= 0.0 || !usable.is_finite() || usable < PANE_MIN * 2.0 {
        return None;
    }
    let lo = (PANE_MIN / usable).max(FRACTION_MIN);
    let hi = (1.0 - PANE_MIN / usable).min(FRACTION_MAX);
    Some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_garbage_file_uses_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Layout::load(Some(dir.path().join("layout")));
        assert_eq!(missing.mode, Mode::Split);
        assert!(missing.code_minimap && missing.live_minimap);
        assert_eq!(missing.code_fraction, 0.5);
        assert_eq!(missing.outline_width, outline::PREFERRED_WIDTH);

        let path = dir.path().join("layout");
        std::fs::write(&path, "nope\nxyz\nabc\nnope\nwide\n").unwrap();
        let garbage = Layout::load(Some(path));
        assert_eq!(garbage.mode, Mode::Split);
        assert!(garbage.code_minimap && garbage.live_minimap);
        assert_eq!(garbage.code_fraction, 0.5);
        assert_eq!(garbage.outline_width, outline::PREFERRED_WIDTH);
    }

    #[test]
    fn out_of_range_values_clamp_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout");
        std::fs::write(&path, "live\n0\n1\n9\n12\n").unwrap();
        let layout = Layout::load(Some(path.clone()));
        assert_eq!(layout.mode, Mode::Live);
        assert!(!layout.code_minimap);
        assert!(layout.live_minimap);
        assert_eq!(layout.code_fraction, FRACTION_MAX);
        assert_eq!(layout.outline_width, outline::MIN_WIDTH);
        layout.save();
        let again = Layout::load(Some(path));
        assert_eq!(again.mode, layout.mode);
        assert_eq!(again.code_minimap, layout.code_minimap);
        assert_eq!(again.live_minimap, layout.live_minimap);
        assert_eq!(again.code_fraction, layout.code_fraction);
        assert_eq!(again.outline_width, layout.outline_width);
    }

    #[test]
    fn a_narrow_window_draws_the_middle_and_keeps_the_stored_fraction() {
        let mut layout = Layout::load(None);
        layout.code_fraction = 0.85;
        assert_eq!(layout.split_fraction(300.0), 0.5);
        assert_eq!(layout.code_fraction, 0.85);
        // 1200px leaves room for an 0.85 share above the 160px minimum.
        let wide = layout.split_fraction(1200.0);
        assert!((wide - 0.85).abs() < f32::EPSILON, "{wide}");

        layout.drag_split(0.0, 300.0, 150.0, 10.0);
        assert_eq!(layout.code_fraction, 0.85);

        layout.code_fraction = 0.5;
        let mid = split_mid(0.0, 1200.0, layout.split_fraction(1200.0));
        layout.drag_split(0.0, 1200.0, mid, 0.0);
        let stayed = split_mid(0.0, 1200.0, layout.split_fraction(1200.0));
        assert!((stayed - mid).abs() < 0.51, "{stayed} {mid}");

        layout.drag_split(0.0, 1200.0, mid, 80.0);
        let moved = split_mid(0.0, 1200.0, layout.split_fraction(1200.0));
        assert!((moved - (mid + 80.0)).abs() < 0.51, "{moved}");

        layout.drag_split(0.0, 1200.0, mid, -10_000.0);
        let lo = PANE_MIN / (1200.0 - SPLIT_GAP);
        assert!(
            (layout.code_fraction - lo).abs() < 0.001,
            "{}",
            layout.code_fraction
        );
    }

    #[test]
    fn an_outline_drag_starts_from_the_drawn_width() {
        let mut layout = Layout::load(None);
        layout.outline_width = 400.0;
        let (_, drawn) = outline::column_widths(700.0, Some(240.0), layout.outline_width);
        assert!(drawn < layout.outline_width, "{drawn}");
        layout.drag_outline(drawn, 20.0);
        let (_, after) = outline::column_widths(700.0, Some(240.0), layout.outline_width);
        assert!(
            (after - (drawn - 20.0)).abs() < 0.1,
            "drawn {drawn}, after {after}, stored {}",
            layout.outline_width
        );

        // 80px of a 200px outline hits the minimum. Another 20px past it,
        // then 10px back, still sits on the minimum.
        layout.drag_outline(200.0, 100.0);
        assert_eq!(layout.outline_width, outline::MIN_WIDTH);
        layout.drag_outline(200.0, 90.0);
        assert_eq!(layout.outline_width, outline::MIN_WIDTH);
        layout.drag_outline(200.0, 70.0);
        assert_eq!(layout.outline_width, 130.0);
    }
}
