//! Prints pulldown-cmark's events with their source ranges.
//! `cargo run -p inkmark-parse --example dump [--gfm] < file.md`

use std::io::Read;

use pulldown_cmark::{Options, Parser};

fn main() {
    let mut src = String::new();
    std::io::stdin()
        .read_to_string(&mut src)
        .expect("read stdin");
    let options = if std::env::args().any(|a| a == "--gfm") {
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
    } else {
        Options::empty()
    };
    for (event, range) in Parser::new_ext(&src, options).into_offset_iter() {
        println!(
            "{:>4}..{:<4} {:?}  {:?}",
            range.start,
            range.end,
            &src[range.clone()],
            event
        );
    }
}
