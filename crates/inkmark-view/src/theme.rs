//! Colors shared by the panes, the sidebar and the app chrome.
//!
//! A [`Theme`] lives in the egui context (see [`current`] / [`set`]), so each
//! window, and each test, has its own. The panes read it at the start of
//! every frame; shaped text is cached without colors, so switching themes
//! needs no relayout.

use std::sync::Arc;

use egui::Color32;
use inkmark_parse::{Span, SpanKind, Style, Syntax};

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Dark (light text on a dark background) or light.
    pub dark: bool,

    // Panes.
    pub background: Color32,
    pub text: Color32,
    pub selection: Color32,
    pub caret: Color32,
    pub scroll_track: Color32,
    pub scroll_thumb: Color32,

    // Markdown, in both panes.
    /// Revealed or code-pane syntax: `#`, `**`, list markers, fences.
    pub markup: Color32,
    pub link_markup: Color32,
    pub heading: Color32,
    pub code: Color32,
    pub link: Color32,
    pub html: Color32,
    pub strong: Color32,
    pub emphasis: Color32,
    pub quote: Color32,
    pub struck: Color32,
    pub task_open: Color32,
    pub task_done: Color32,
    pub entity: Color32,

    // Live-pane decorations.
    pub code_background: Color32,
    pub quote_bar: Color32,
    pub rule: Color32,
    pub list_marker: Color32,
    pub checkbox_done: Color32,

    // Minimaps.
    pub mini_background: Color32,
    pub mini_text: Color32,
    pub mini_heading: Color32,
    pub mini_code: Color32,
    pub mini_code_background: Color32,
    pub mini_viewport: Color32,
    pub mini_viewport_hover: Color32,
    pub mini_viewport_edge: Color32,

    // Sidebar and chrome.
    /// The open file's row in the sidebar (the selected row uses `selection`).
    pub current_row: Color32,
    /// Lines between the sidebar and the panes, and between the panes.
    pub divider: Color32,
    pub error: Color32,
    pub hint: Color32,
}

impl Theme {
    /// The original inkmark colors.
    pub fn dark() -> Self {
        Self {
            dark: true,
            background: Color32::from_rgb(22, 22, 26),
            text: Color32::from_gray(212),
            selection: Color32::from_rgba_premultiplied(38, 60, 98, 120),
            caret: Color32::from_rgb(120, 170, 255),
            scroll_track: Color32::from_gray(28),
            scroll_thumb: Color32::from_gray(80),
            markup: Color32::from_rgb(105, 115, 135),
            link_markup: Color32::from_rgb(85, 135, 145),
            heading: Color32::from_rgb(130, 180, 255),
            code: Color32::from_rgb(150, 200, 140),
            link: Color32::from_rgb(110, 170, 230),
            html: Color32::from_rgb(220, 150, 100),
            strong: Color32::from_gray(245),
            emphasis: Color32::from_rgb(220, 200, 150),
            quote: Color32::from_gray(160),
            struck: Color32::from_gray(125),
            task_open: Color32::from_rgb(215, 180, 110),
            task_done: Color32::from_rgb(130, 190, 120),
            entity: Color32::from_rgb(200, 150, 220),
            code_background: Color32::from_rgb(30, 31, 38),
            quote_bar: Color32::from_gray(70),
            rule: Color32::from_gray(70),
            list_marker: Color32::from_gray(140),
            checkbox_done: Color32::from_rgb(110, 165, 105),
            mini_background: Color32::from_rgb(19, 19, 23),
            mini_text: Color32::from_gray(95),
            mini_heading: Color32::from_rgb(90, 130, 190),
            mini_code: Color32::from_rgb(95, 135, 90),
            mini_code_background: Color32::from_rgb(34, 36, 46),
            mini_viewport: Color32::from_rgba_premultiplied(18, 18, 18, 18),
            mini_viewport_hover: Color32::from_rgba_premultiplied(30, 30, 30, 30),
            mini_viewport_edge: Color32::from_rgba_premultiplied(40, 40, 40, 40),
            current_row: Color32::from_rgb(32, 40, 54),
            divider: Color32::from_gray(40),
            error: Color32::from_rgb(255, 140, 120),
            hint: Color32::from_rgb(230, 200, 120),
        }
    }

    /// Dark text on paper white, for light desktops.
    pub fn light() -> Self {
        Self {
            dark: false,
            background: Color32::from_rgb(250, 250, 248),
            text: Color32::from_rgb(36, 41, 47),
            selection: Color32::from_rgba_unmultiplied(80, 140, 240, 70),
            caret: Color32::from_rgb(30, 100, 220),
            scroll_track: Color32::from_gray(238),
            scroll_thumb: Color32::from_gray(190),
            markup: Color32::from_rgb(110, 118, 130),
            link_markup: Color32::from_rgb(60, 115, 125),
            heading: Color32::from_rgb(25, 90, 180),
            code: Color32::from_rgb(35, 115, 45),
            link: Color32::from_rgb(20, 100, 200),
            html: Color32::from_rgb(170, 80, 20),
            strong: Color32::from_gray(15),
            emphasis: Color32::from_rgb(125, 85, 15),
            quote: Color32::from_gray(85),
            struck: Color32::from_gray(120),
            task_open: Color32::from_rgb(160, 105, 10),
            task_done: Color32::from_rgb(35, 130, 55),
            entity: Color32::from_rgb(135, 65, 165),
            code_background: Color32::from_rgb(238, 238, 233),
            quote_bar: Color32::from_gray(200),
            rule: Color32::from_gray(205),
            list_marker: Color32::from_gray(115),
            checkbox_done: Color32::from_rgb(55, 145, 75),
            mini_background: Color32::from_rgb(243, 243, 240),
            mini_text: Color32::from_gray(170),
            mini_heading: Color32::from_rgb(120, 160, 215),
            mini_code: Color32::from_rgb(130, 175, 130),
            mini_code_background: Color32::from_rgb(230, 230, 225),
            mini_viewport: Color32::from_rgba_unmultiplied(0, 0, 0, 14),
            mini_viewport_hover: Color32::from_rgba_unmultiplied(0, 0, 0, 24),
            mini_viewport_edge: Color32::from_rgba_unmultiplied(0, 0, 0, 40),
            current_row: Color32::from_rgb(226, 233, 245),
            divider: Color32::from_gray(218),
            error: Color32::from_rgb(185, 45, 35),
            hint: Color32::from_rgb(140, 95, 0),
        }
    }

    /// Code-pane color for a span, or `None` for the default text color.
    pub fn code_color(&self, span: &Span) -> Option<Color32> {
        match &span.kind {
            SpanKind::Syntax(Syntax::TaskMarker(true)) => Some(self.task_done),
            SpanKind::Syntax(Syntax::TaskMarker(false)) => Some(self.task_open),
            SpanKind::Syntax(Syntax::LinkMarkup) => Some(self.link_markup),
            SpanKind::Syntax(_) => Some(self.markup),
            SpanKind::Replaced(_) if span.style.contains(Style::FOOTNOTE) => Some(self.link),
            SpanKind::Replaced(_) if !span.style.contains(Style::CODE_BLOCK) => Some(self.entity),
            SpanKind::Text | SpanKind::Replaced(_) => self.text_color(span.style),
            SpanKind::SoftBreak | SpanKind::Whitespace => None,
        }
    }

    fn text_color(&self, style: Style) -> Option<Color32> {
        // Most specific first.
        [
            (Style::STRIKE, self.struck),
            (Style::CODE, self.code),
            (Style::CODE_BLOCK, self.code),
            (Style::HTML, self.html),
            (Style::HEADING, self.heading),
            (Style::LINK, self.link),
            (Style::FOOTNOTE, self.link),
            (Style::IMAGE, self.link),
            (Style::STRONG, self.strong),
            (Style::TABLE_HEAD, self.strong),
            (Style::EMPHASIS, self.emphasis),
            (Style::QUOTE, self.quote),
        ]
        .into_iter()
        .find(|(s, _)| style.contains(*s))
        .map(|(_, c)| c)
    }

    /// Color of revealed syntax in the live view.
    pub fn syntax_color(&self, span: &Span) -> Color32 {
        match span.kind {
            SpanKind::Syntax(Syntax::LinkMarkup) => self.link_markup,
            _ => self.markup,
        }
    }

    /// Live-view color for rendered text, or `None` for the default.
    pub fn live_color(&self, span: &Span) -> Option<Color32> {
        self.text_color(span.style)
    }

    /// egui's own widgets (buttons, dialogs, the sidebar's header) to match.
    pub fn visuals(&self) -> egui::Visuals {
        let mut v = if self.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        v.panel_fill = self.background;
        v.window_fill = self.code_background;
        v.extreme_bg_color = self.code_background;
        v.faint_bg_color = self.mini_background;
        v.hyperlink_color = self.link;
        v.selection.bg_fill = self.selection;
        v.selection.stroke.color = self.text;
        v.widgets.noninteractive.fg_stroke.color = self.text;
        v.widgets.noninteractive.bg_stroke.color = self.divider;
        v.window_stroke.color = self.divider;
        v.text_cursor.stroke.color = self.caret;
        v
    }
}

/// A desktop color scheme in Omarchy's `colors.toml` terms: the colors
/// inkmark maps onto its roles. Missing ones fall back to the built-in theme
/// of the same `dark`ness.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub background: Option<Color32>,
    pub foreground: Option<Color32>,
    pub bright_foreground: Option<Color32>,
    pub accent: Option<Color32>,
    pub selection: Option<Color32>,
    pub muted: Option<Color32>,
    pub red: Option<Color32>,
    pub green: Option<Color32>,
    pub yellow: Option<Color32>,
    pub orange: Option<Color32>,
    pub blue: Option<Color32>,
    pub cyan: Option<Color32>,
    pub magenta: Option<Color32>,
}

impl Theme {
    /// inkmark's roles in `palette`'s colors. Shades for structure (code
    /// blocks, quote bars, the minimap) are blended from the background and
    /// foreground; any text color too faint on the background is moved
    /// toward the foreground until it reads (4.5:1 for text, 3:1 for
    /// markup), so every palette stays legible.
    pub fn from_palette(p: &Palette) -> Self {
        let base = if p.dark { Self::dark() } else { Self::light() };
        let bg = p.background.unwrap_or(base.background);
        let fg = p.foreground.unwrap_or(base.text);
        let pick = |c: Option<Color32>, fallback: Color32| c.unwrap_or(fallback);
        let accent = pick(p.accent, pick(p.blue, base.caret));
        let shade = |t: f32| mix(bg, fg, t);
        let tint = |c: Color32, t: f32| mix(bg, c, t);
        let code_background = shade(0.06);
        let readable = |c: Color32, min: f32| readable_on(c, &[bg, code_background], fg, min);
        let green = pick(p.green, base.code);
        let yellow = pick(p.yellow, base.emphasis);
        let selection = pick(p.selection, base.selection);
        Self {
            dark: p.dark,
            background: bg,
            text: readable(fg, 4.5),
            // Painted under the text: keep it see-through.
            selection: Color32::from_rgba_unmultiplied(
                selection.r(),
                selection.g(),
                selection.b(),
                170,
            ),
            caret: readable(accent, 3.0),
            scroll_track: shade(0.04),
            scroll_thumb: shade(0.3),
            markup: readable(pick(p.muted, base.markup), 3.0),
            link_markup: readable(pick(p.cyan, base.link_markup), 3.0),
            heading: readable(accent, 4.5),
            code: readable(green, 4.5),
            link: readable(pick(p.blue, accent), 4.5),
            html: readable(pick(p.orange, pick(p.red, base.html)), 4.5),
            strong: readable(pick(p.bright_foreground, fg), 4.5),
            emphasis: readable(yellow, 4.5),
            quote: readable(shade(0.75), 3.0),
            struck: readable(pick(p.muted, shade(0.5)), 3.0),
            task_open: readable(yellow, 3.0),
            task_done: readable(green, 3.0),
            entity: readable(pick(p.magenta, base.entity), 4.5),
            code_background,
            quote_bar: shade(0.25),
            rule: shade(0.25),
            list_marker: readable(shade(0.55), 3.0),
            checkbox_done: readable(green, 3.0),
            mini_background: shade(0.025),
            mini_text: shade(0.35),
            mini_heading: tint(accent, 0.6),
            mini_code: tint(green, 0.6),
            mini_code_background: shade(0.1),
            mini_viewport: fg.gamma_multiply(0.07),
            mini_viewport_hover: fg.gamma_multiply(0.12),
            mini_viewport_edge: fg.gamma_multiply(0.2),
            current_row: tint(accent, 0.15),
            divider: shade(0.15),
            error: readable(pick(p.red, base.error), 4.5),
            hint: readable(yellow, 3.0),
        }
    }
}

/// `t` of the way from `a` to `b`.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// `color`, moved toward `toward` (then toward black or white) just far
/// enough to reach `min` contrast on every one of `backgrounds`.
fn readable_on(color: Color32, backgrounds: &[Color32], toward: Color32, min: f32) -> Color32 {
    let ok = |c: Color32| backgrounds.iter().all(|&bg| contrast(c, bg) >= min);
    if ok(color) {
        return color;
    }
    let extreme =
        if contrast(Color32::WHITE, backgrounds[0]) > contrast(Color32::BLACK, backgrounds[0]) {
            Color32::WHITE
        } else {
            Color32::BLACK
        };
    for target in [toward, extreme] {
        for step in 1..=20 {
            let c = mix(color, target, step as f32 / 20.0);
            if ok(c) {
                return c;
            }
        }
    }
    extreme
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// The theme for `ctx` (dark until one is set).
pub fn current(ctx: &egui::Context) -> Arc<Theme> {
    ctx.data(|d| d.get_temp::<Arc<Theme>>(egui::Id::NULL))
        .unwrap_or_else(|| Arc::new(Theme::dark()))
}

/// Uses `theme` for `ctx` from the next frame, egui's widgets included.
pub fn set(ctx: &egui::Context, theme: Theme) {
    // Pin egui's own light/dark choice to ours, so it doesn't switch its
    // widgets back with the system setting.
    ctx.set_theme(if theme.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    ctx.set_visuals(theme.visuals());
    ctx.data_mut(|d| d.insert_temp(egui::Id::NULL, Arc::new(theme)));
}

/// WCAG contrast ratio between two opaque colors, 1 to 21.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    fn luminance(c: Color32) -> f32 {
        let lin = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every color text is drawn in, with the contrast it needs on the
    /// background: body text 4.5 (WCAG AA), markup and secondary text 3.
    pub(crate) fn check_readable(t: &Theme) -> Vec<String> {
        let roles = [
            ("text", t.text, 4.5),
            ("heading", t.heading, 4.5),
            ("code", t.code, 4.5),
            ("link", t.link, 4.5),
            ("html", t.html, 4.5),
            ("strong", t.strong, 4.5),
            ("emphasis", t.emphasis, 4.5),
            ("entity", t.entity, 4.5),
            ("quote", t.quote, 3.0),
            ("markup", t.markup, 3.0),
            ("link_markup", t.link_markup, 3.0),
            ("struck", t.struck, 3.0),
            ("task_open", t.task_open, 3.0),
            ("task_done", t.task_done, 3.0),
            ("list_marker", t.list_marker, 3.0),
            ("error", t.error, 4.5),
            ("hint", t.hint, 3.0),
        ];
        let mut failures = Vec::new();
        for (name, color, min) in roles {
            for (bg_name, bg) in [
                ("background", t.background),
                ("code_background", t.code_background),
            ] {
                let c = contrast(color, bg);
                if c < min {
                    failures.push(format!("{name} on {bg_name}: {c:.2} < {min}"));
                }
            }
        }
        failures
    }

    #[test]
    fn the_built_in_themes_are_readable() {
        for (name, theme) in [("dark", Theme::dark()), ("light", Theme::light())] {
            let failures = check_readable(&theme);
            assert!(failures.is_empty(), "{name}: {failures:#?}");
        }
    }

    #[test]
    fn contrast_matches_the_wcag_formula() {
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(Color32::WHITE, Color32::WHITE) - 1.0).abs() < 0.01);
    }

    fn hex(s: &str) -> Option<Color32> {
        Color32::from_hex(s).ok()
    }

    #[test]
    fn a_palette_maps_onto_every_role_readably() {
        // A dark palette with a too-dim muted color and a too-dark blue: the
        // mapping lifts them to readable shades rather than using them as is.
        let p = Palette {
            dark: true,
            background: hex("#101418"),
            foreground: hex("#d8dee9"),
            accent: hex("#2a3a8a"),
            muted: hex("#2c333b"),
            green: hex("#a3be8c"),
            yellow: hex("#ebcb8b"),
            red: hex("#bf616a"),
            ..Default::default()
        };
        let t = Theme::from_palette(&p);
        assert!(check_readable(&t).is_empty(), "{:#?}", check_readable(&t));
        assert_eq!(t.background, hex("#101418").unwrap());
        assert_eq!(
            t.code,
            hex("#a3be8c").unwrap(),
            "readable colors are used as given"
        );
        assert_ne!(
            t.markup,
            hex("#2c333b").unwrap(),
            "a faint muted color is lifted"
        );
        assert!(t.dark);

        // A light palette with nothing but its two main colors.
        let p = Palette {
            dark: false,
            background: hex("#fffcf0"),
            foreground: hex("#100f0f"),
            ..Default::default()
        };
        let t = Theme::from_palette(&p);
        assert!(check_readable(&t).is_empty(), "{:#?}", check_readable(&t));
        assert!(!t.dark);
        assert_eq!(t.visuals().panel_fill, hex("#fffcf0").unwrap());
    }

    #[test]
    fn each_context_has_its_own_theme() {
        let (a, b) = (egui::Context::default(), egui::Context::default());
        assert_eq!(*current(&a), Theme::dark());
        set(&a, Theme::light());
        assert_eq!(*current(&a), Theme::light());
        assert_eq!(*current(&b), Theme::dark());
    }
}
