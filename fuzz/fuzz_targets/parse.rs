//! Fuzzes the tree-sitter grammars compiled into tokensave (#958).
//!
//! Input layout: byte 0 selects the grammar (modulo the number of compiled
//! grammars), bytes 1..3 place an edit, and the rest is the source text. The
//! target parses the text, walks the tree, deletes the selected range and
//! re-parses incrementally, which round-trips external scanner state.
//!
//! Set `TOKENSAVE_FUZZ_GRAMMAR=<key>` to fuzz one grammar; byte 0 is then
//! ignored. See scripts/sanitizers/README.md.

#![no_main]

use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use tokensave::extraction::ts_provider;
use tree_sitter::InputEdit;
use tree_sitter::Parser;
use tree_sitter::Point;

static KEYS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    let all = ts_provider::language_keys();
    match std::env::var("TOKENSAVE_FUZZ_GRAMMAR") {
        Ok(wanted) => {
            let key = all
                .iter()
                .copied()
                .find(|k| *k == wanted)
                .unwrap_or_else(|| panic!("unknown grammar '{wanted}'; compiled: {all:?}"));
            vec![key]
        }
        Err(_) => all,
    }
});

fuzz_target!(|data: &[u8]| {
    let [selector, a, b, text @ ..] = data else {
        return;
    };
    let keys = &*KEYS;
    let key = keys[usize::from(*selector) % keys.len()];
    let mut parser = Parser::new();
    parser
        .set_language(&ts_provider::language(key))
        .expect("compiled grammars match the linked tree-sitter ABI");
    let Some(mut tree) = parser.parse(text, None) else {
        return;
    };
    let _ = tree.root_node().to_sexp();

    let len = text.len();
    let start = usize::from(*a) * len / 256;
    let end = (start + usize::from(*b)).min(len);
    let mut edited = text[..start].to_vec();
    edited.extend_from_slice(&text[end..]);
    tree.edit(&InputEdit {
        start_byte: start,
        old_end_byte: end,
        new_end_byte: start,
        start_position: point_at(text, start),
        old_end_position: point_at(text, end),
        new_end_position: point_at(&edited, start),
    });
    if let Some(new_tree) = parser.parse(&edited, Some(&tree)) {
        let _ = new_tree.root_node().to_sexp();
        let _ = tree.changed_ranges(&new_tree).count();
    }
});

fn point_at(text: &[u8], offset: usize) -> Point {
    let before = &text[..offset];
    let row = before.iter().filter(|&&b| b == b'\n').count();
    let column = before
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(offset, |nl| offset - nl - 1);
    Point { row, column }
}
