//! Prints pulldown-cmark's events with their source ranges.
//! `cargo run -p inkmark-parse --example dump < file.md`

use std::io::Read;

fn main() {
    let mut src = String::new();
    std::io::stdin()
        .read_to_string(&mut src)
        .expect("read stdin");
    for (event, range) in pulldown_cmark::Parser::new(&src).into_offset_iter() {
        println!(
            "{:>4}..{:<4} {:?}  {:?}",
            range.start,
            range.end,
            &src[range.clone()],
            event
        );
    }
}
