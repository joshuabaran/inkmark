use std::ops::Range;

use pulldown_cmark::{Event, Parser, Tag};

/// An image in a piece of Markdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineImage {
    /// Bytes of `![alt](dest "title")` within the text parsed.
    pub range: Range<usize>,
    pub dest: String,
    pub alt: String,
}

/// Images in `src`, in order. Parses `src` on its own, so reference-style
/// images whose definitions live elsewhere in the document aren't found.
pub fn inline_images(src: &str) -> Vec<InlineImage> {
    let mut images: Vec<InlineImage> = Vec::new();
    let mut depth = 0;
    for (event, range) in Parser::new(src).into_offset_iter() {
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

    #[test]
    fn finds_images_with_destinations_and_alt_text() {
        let src = "See ![a *cat*](img/cat.png \"Cat\") and ![](<my dog.jpg>).\n![\\]](x.gif)";
        let images = inline_images(src);
        assert_eq!(images.len(), 3);
        assert_eq!(
            &src[images[0].range.clone()],
            "![a *cat*](img/cat.png \"Cat\")"
        );
        assert_eq!(images[0].dest, "img/cat.png");
        assert_eq!(images[0].alt, "a cat");
        assert_eq!(images[1].dest, "my dog.jpg");
        assert_eq!(images[2].alt, "]");
        assert!(inline_images("no images, just a [link](x.png)").is_empty());
    }
}
