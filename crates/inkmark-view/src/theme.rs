//! Dark theme colors shared by the panes.

use egui::Color32;
use inkmark_parse::{Span, SpanKind, Style, Syntax};

pub const BACKGROUND: Color32 = Color32::from_rgb(22, 22, 26);
pub const TEXT: Color32 = Color32::from_gray(212);
pub const SELECTION: Color32 = Color32::from_rgba_premultiplied(38, 60, 98, 120);
pub const CARET: Color32 = Color32::from_rgb(120, 170, 255);
pub const SCROLL_TRACK: Color32 = Color32::from_gray(28);
pub const SCROLL_THUMB: Color32 = Color32::from_gray(80);

const MARKUP: Color32 = Color32::from_rgb(105, 115, 135);
const LINK_MARKUP: Color32 = Color32::from_rgb(85, 135, 145);
const HEADING: Color32 = Color32::from_rgb(130, 180, 255);
const CODE: Color32 = Color32::from_rgb(150, 200, 140);
const LINK: Color32 = Color32::from_rgb(110, 170, 230);
const HTML: Color32 = Color32::from_rgb(220, 150, 100);
const STRONG: Color32 = Color32::from_gray(245);
const EMPHASIS: Color32 = Color32::from_rgb(220, 200, 150);
const QUOTE: Color32 = Color32::from_gray(160);
const ENTITY: Color32 = Color32::from_rgb(200, 150, 220);

/// Code-pane color for a span, or `None` for the default text color.
pub fn code_color(span: &Span) -> Option<Color32> {
    match &span.kind {
        SpanKind::Syntax(Syntax::LinkMarkup) => Some(LINK_MARKUP),
        SpanKind::Syntax(_) => Some(MARKUP),
        SpanKind::Replaced(_) if !span.style.contains(Style::CODE_BLOCK) => Some(ENTITY),
        SpanKind::Text | SpanKind::Replaced(_) => text_color(span.style),
        SpanKind::SoftBreak | SpanKind::Whitespace => None,
    }
}

fn text_color(style: Style) -> Option<Color32> {
    // Most specific first.
    [
        (Style::CODE, CODE),
        (Style::CODE_BLOCK, CODE),
        (Style::HTML, HTML),
        (Style::HEADING, HEADING),
        (Style::LINK, LINK),
        (Style::IMAGE, LINK),
        (Style::STRONG, STRONG),
        (Style::EMPHASIS, EMPHASIS),
        (Style::QUOTE, QUOTE),
    ]
    .into_iter()
    .find(|(s, _)| style.contains(*s))
    .map(|(_, c)| c)
}
