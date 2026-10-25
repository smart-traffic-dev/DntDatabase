//! Property test (R11): with client detection off, the streaming splitter returns
//! exactly `split`'s statements (text and absolute offsets) for every script and every
//! way of cutting it into chunks, including cuts inside UTF-8 sequences.

use dnt_core::sql::stream::{SplitEvent, SplitOptions, StreamSplitter};
use dnt_core::sql::{split, Dialect, MySqlMode};
use proptest::prelude::*;

const FRAGMENTS: &[&str] = &[
    "'",
    "''",
    "\"",
    "`",
    "$$",
    "$t$",
    "E'",
    "e'",
    "--",
    "-- ",
    "#",
    "/*",
    "*/",
    "/*!",
    "\\",
    "\r",
    "\n",
    "\r\n",
    " ",
    "\t",
    ";",
    ";",
    "\nDELIMITER //\n",
    "\ndelimiter ;\n",
    "  delimiter $$\n",
    "//",
    "đ",
    "ư",
    "😀",
    "\u{a0}",
    "select",
    "a",
    "x1",
    "$",
    "d",
    "(",
];

fn dialects() -> Vec<Dialect> {
    let mut out = vec![Dialect::Postgres];
    for ansi_quotes in [false, true] {
        for no_backslash_escapes in [false, true] {
            out.push(Dialect::MySql(MySqlMode {
                ansi_quotes,
                no_backslash_escapes,
            }));
        }
    }
    out
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512)
}

/// Feeds `input` cut at `cuts` (sorted, deduplicated byte offsets) and collects events.
fn stream(input: &[u8], dialect: Dialect, cuts: &[usize]) -> Vec<SplitEvent> {
    let mut s = StreamSplitter::new(dialect, SplitOptions::default());
    let mut out = Vec::new();
    let mut from = 0;
    for &cut in cuts.iter().chain([input.len()].iter()) {
        s.push(&input[from..cut]);
        from = cut;
        while let Some(e) = s.next_event().expect("valid UTF-8 input") {
            out.push(e);
        }
    }
    s.finish();
    while let Some(e) = s.next_event().expect("valid UTF-8 input") {
        out.push(e);
    }
    assert!(s.is_done());
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn stream_matches_split_for_any_chunking(
        parts in prop::collection::vec(prop::sample::select(FRAGMENTS), 0..60),
        raw_cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..12),
        d in 0usize..5,
    ) {
        let input = parts.concat();
        let dialect = dialects()[d];
        let mut cuts: Vec<usize> = raw_cuts.iter().map(|i| i.index(input.len() + 1)).collect();
        cuts.sort_unstable();
        cuts.dedup();
        let want: Vec<(String, u64, u64)> = split(&input, dialect)
            .into_iter()
            .map(|s| (s.text, s.start as u64, s.end as u64))
            .collect();
        let got: Vec<(String, u64, u64)> = stream(input.as_bytes(), dialect, &cuts)
            .into_iter()
            .map(|e| match e {
                SplitEvent::Statement(s) => (s.text, s.start, s.end),
                other => panic!("detection is off: {other:?}"),
            })
            .collect();
        prop_assert_eq!(got, want, "input {:?} cuts {:?} ({:?})", input, cuts, dialect);
    }

    /// Byte-by-byte feeding is the hardest chunking for the look-backs.
    #[test]
    fn stream_matches_split_byte_by_byte(
        parts in prop::collection::vec(prop::sample::select(FRAGMENTS), 0..30),
        d in 0usize..5,
    ) {
        let input = parts.concat();
        let dialect = dialects()[d];
        let cuts: Vec<usize> = (1..input.len()).collect();
        let want: Vec<(String, u64, u64)> = split(&input, dialect)
            .into_iter()
            .map(|s| (s.text, s.start as u64, s.end as u64))
            .collect();
        let got: Vec<(String, u64, u64)> = stream(input.as_bytes(), dialect, &cuts)
            .into_iter()
            .map(|e| match e {
                SplitEvent::Statement(s) => (s.text, s.start, s.end),
                other => panic!("detection is off: {other:?}"),
            })
            .collect();
        prop_assert_eq!(got, want, "input {:?} ({:?})", input, dialect);
    }
}
