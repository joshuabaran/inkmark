Markdown syntax cheatsheet. Each row is a directive, then the source
inkmark shows: the left column in the code font, the right column through
the live renderer.

Edit a row by changing the Markdown under its directive. A directive is a
line `<!-- name -->`. The parser ignores every line before the first one.

Included, because inkmark's parser turns them on (GFM_OPTIONS in
crates/inkmark-parse/src/pulldown.rs: tables, strikethrough, task lists,
footnotes) or they are CommonMark:

headings, emphasis, strong, strikethrough, inline code, fenced code,
links, reference definitions, images, block quotes, unordered lists,
ordered lists, task lists, tables, horizontal rules, footnotes, hard breaks.

Left out because the pulldown-cmark option is off: smart punctuation,
heading attributes, YAML and +++ metadata, old footnotes, definition
lists, superscript, subscript, wikilinks, pulldown math, and GitHub alerts
(ENABLE_GFM). Autolinks and $...$ formulas are inkmark's own and are not
part of this sheet.

<!-- heading 1 -->
# Heading

<!-- heading 2 -->
## Heading

<!-- heading 3 -->
### Heading

<!-- heading 4 -->
#### Heading

<!-- heading 5 -->
##### Heading

<!-- heading 6 -->
###### Heading

<!-- emphasis -->
*emphasized* and _also_

<!-- strong -->
**strong** and __also__

<!-- strikethrough -->
~~struck~~

<!-- inline-code -->
Inline `code` span.

<!-- fenced-code -->
```
fn demo() {}
```

<!-- link -->
[inkmark](https://example.com)

<!-- reference -->
See the [guide][home].

[home]: https://example.com/guide

<!-- image -->
![A hillside](https://example.com/hill.png)

<!-- quote -->
> A quoted line.

<!-- unordered -->
- one
- two

<!-- ordered -->
1. first
2. second

<!-- task -->
- [ ] open
- [x] done

<!-- table -->
| Item | Count |
| --- | --- |
| pens | 2 |

<!-- rule -->
---

<!-- footnote -->
A note[^1].

[^1]: The note.

<!-- hard-break -->
first\
second
