use std::collections::HashMap;
use std::ops::Range;

use pulldown_cmark::{BrokenLink, CowStr, Event, Options, Parser, Tag};

use crate::normalize_label;

/// An image in a piece of Markdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineImage {
    /// Bytes of `![alt](dest "title")` within the text parsed.
    pub range: Range<usize>,
    pub dest: String,
    pub alt: String,
}

/// Images in `src`, in order. `src` is parsed on its own; reference-style
/// images (`![alt][label]`) resolve through `link_defs` (the document's
/// [`ParseOutput::link_defs`](crate::ParseOutput::link_defs)).
pub fn inline_images(src: &str, link_defs: &HashMap<String, String>) -> Vec<InlineImage> {
    let mut images: Vec<InlineImage> = Vec::new();
    let mut depth = 0;
    let resolve = |link: BrokenLink| {
        link_defs
            .get(&normalize_label(&link.reference))
            .map(|dest| (CowStr::from(dest.clone()), CowStr::from("")))
    };
    let parser = Parser::new_with_broken_link_callback(src, Options::empty(), Some(resolve));
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                if depth == 0 {
                    images.push(InlineImage {
                        range,
                        dest: dest_url.into_string(),
                        alt: String::new(),
                    });
                }
                depth += 1;
            }
            Event::End(pulldown_cmark::TagEnd::Image) => depth -= 1,
            Event::Text(text) | Event::Code(text) if depth > 0 => {
                if let Some(image) = images.last_mut() {
                    image.alt.push_str(&text);
                }
            }
            _ => {}
        }
    }
    images
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MarkdownParser;

    #[test]
    fn finds_images_with_destinations_and_alt_text() {
        let src = "See ![a *cat*](img/cat.png \"Cat\") and ![](<my dog.jpg>).\n![\\]](x.gif)";
        let images = inline_images(src, &HashMap::new());
        assert_eq!(images.len(), 3);
        assert_eq!(
            &src[images[0].range.clone()],
            "![a *cat*](img/cat.png \"Cat\")"
        );
        assert_eq!(images[0].dest, "img/cat.png");
        assert_eq!(images[0].alt, "a cat");
        assert_eq!(images[1].dest, "my dog.jpg");
        assert_eq!(images[2].alt, "]");
        assert!(inline_images("no images, just a [link](x.png)", &HashMap::new()).is_empty());
    }

    #[test]
    fn reference_images_resolve_through_document_definitions() {
        let defs = crate::PulldownParser
            .parse("Body.\n\n[Logo  Image]: img/logo.png \"Logo\"\n")
            .link_defs;
        assert_eq!(
            defs.get("logo image").map(String::as_str),
            Some("img/logo.png")
        );
        let images = inline_images("A ![logo][logo image] and ![missing][nope].", &defs);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].dest, "img/logo.png");
        assert_eq!(images[0].alt, "logo");
    }
}
