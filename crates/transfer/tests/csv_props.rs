//! Property: any table written with RFC 4180 quoting reads back cell for cell.

use std::io::Write;

use dnt_transfer::read::{open, Cell, SourceFormat, SourceOptions};
use proptest::prelude::*;

fn field() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just("a".to_string()),
            Just(",".to_string()),
            Just(";".to_string()),
            Just("\"".to_string()),
            Just("\r\n".to_string()),
            Just("\n".to_string()),
            Just(" ".to_string()),
            Just("\u{1ec5}".to_string()),
            Just("|".to_string()),
            Just("\t".to_string()),
        ],
        1..6,
    )
    .prop_map(|v| v.concat())
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn quoted_table_round_trips(
        width in 1usize..4,
        rows in proptest::collection::vec(proptest::collection::vec(field(), 3), 0..8),
        crlf in any::<bool>(),
        delim in prop_oneof![Just(','), Just(';'), Just('\t'), Just('|')],
    ) {
        let eol = if crlf { "\r\n" } else { "\n" };
        let mut body = String::new();
        let header: Vec<String> = (0..width).map(|i| format!("h{i}")).collect();
        body.push_str(&header.join(&delim.to_string()));
        body.push_str(eol);
        for r in &rows {
            let line: Vec<String> = r[..width].iter().map(|f| quote(f)).collect();
            body.push_str(&line.join(&delim.to_string()));
            body.push_str(eol);
        }
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("p.csv");
        std::fs::File::create(&path).unwrap().write_all(body.as_bytes()).unwrap();
        let opts = SourceOptions { format: SourceFormat::Csv, delimiter: Some(delim), header: true, sheet: None };
        let mut reader = open(&path, &opts).unwrap();
        prop_assert_eq!(reader.columns(), &header[..]);
        for r in &rows {
            let rec = reader.next_record().unwrap().expect("a record");
            let want: Vec<Cell> = r[..width].iter().map(|f| Cell::Text(f.clone())).collect();
            prop_assert_eq!(rec.cells, want);
        }
        prop_assert!(reader.next_record().unwrap().is_none());
        prop_assert_eq!(reader.bytes_read(), reader.total_bytes());
    }
}
