//! Reader tests: every test writes its own temp file.

use std::io::Write;
use std::path::{Path, PathBuf};

use dnt_transfer::read::{inspect, open, Cell, ReadError, Record, SourceFormat, SourceOptions};
use dnt_transfer::{file_facts, FileFacts};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn write_file(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    path
}

fn opts(format: SourceFormat) -> SourceOptions {
    SourceOptions {
        format,
        delimiter: None,
        header: true,
        sheet: None,
    }
}

fn csv_opts() -> SourceOptions {
    opts(SourceFormat::Csv)
}

fn read_all(path: &Path, o: &SourceOptions) -> Result<(Vec<String>, Vec<Record>), ReadError> {
    let mut r = open(path, o)?;
    let cols = r.columns().to_vec();
    let mut rows = Vec::new();
    while let Some(rec) = r.next_record()? {
        rows.push(rec);
    }
    Ok((cols, rows))
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn num(s: &str) -> Cell {
    Cell::Number(s.to_string())
}

// ---------------------------------------------------------------- CSV

#[test]
fn csv_quoting_embedded_delimiter_quotes_and_newlines() {
    let dir = TempDir::new().unwrap();
    let body = b"a,b\r\n\"x,y\",\"say \"\"hi\"\"\"\r\n\"line1\r\nline2\",\"l1\nl2\"\n";
    let p = write_file(&dir, "q.csv", body);
    let (cols, rows) = read_all(&p, &csv_opts()).unwrap();
    assert_eq!(cols, vec!["a", "b"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].cells, vec![text("x,y"), text("say \"hi\"")]);
    assert_eq!(rows[1].cells, vec![text("line1\r\nline2"), text("l1\nl2")]);
    // Record numbers are the line where the record starts.
    assert_eq!(rows[0].number, 2);
    assert_eq!(rows[1].number, 3);
}

#[test]
fn csv_record_numbers_skip_blank_lines_and_crlf() {
    let dir = TempDir::new().unwrap();
    let p = write_file(
        &dir,
        "b.csv",
        b"a,b\r\n\r\n\r\n1,2\r\n\n3,\"x\r\ny\"\r\n5,6",
    );
    let (_, rows) = read_all(&p, &csv_opts()).unwrap();
    let numbers: Vec<u64> = rows.iter().map(|r| r.number).collect();
    assert_eq!(numbers, vec![4, 6, 8]);
}

#[test]
fn csv_bom_is_stripped() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "bom.csv", b"\xEF\xBB\xBFid,name\n1,An\n");
    let (cols, rows) = read_all(&p, &csv_opts()).unwrap();
    assert_eq!(cols, vec!["id", "name"]);
    assert_eq!(rows[0].cells, vec![text("1"), text("An")]);
}

#[test]
fn csv_semicolon_and_tab_delimiters() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "s.csv", b"a;b\n1;2\n");
    let mut o = csv_opts();
    o.delimiter = Some(';');
    let (cols, rows) = read_all(&p, &o).unwrap();
    assert_eq!(cols, vec!["a", "b"]);
    assert_eq!(rows[0].cells, vec![text("1"), text("2")]);

    let p = write_file(&dir, "t.tsv", b"a\tb\n1,5\t2\n");
    o.delimiter = Some('\t');
    let (cols, rows) = read_all(&p, &o).unwrap();
    assert_eq!(cols, vec!["a", "b"]);
    assert_eq!(rows[0].cells, vec![text("1,5"), text("2")]);
}

#[test]
fn csv_unsupported_delimiter_is_refused() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "s.csv", b"a:b\n");
    let mut o = csv_opts();
    o.delimiter = Some(':');
    assert!(open(&p, &o).is_err());
}

#[test]
fn csv_delimiter_is_guessed() {
    let dir = TempDir::new().unwrap();
    for (body, want) in [
        (&b"a;b;c\n1;2;3\n"[..], ';'),
        (&b"a\tb\n1\t2\n"[..], '\t'),
        (&b"a|b\n1|2\n"[..], '|'),
        (&b"a,b\n1,2\n"[..], ','),
        // Delimiters inside quotes do not count.
        (&b"\"x;y;z\",b\n1,2\n"[..], ','),
        (&b"single\n1\n"[..], ','),
    ] {
        let p = write_file(&dir, "g.csv", body);
        let ins = inspect(&p, &csv_opts()).unwrap();
        assert_eq!(
            ins.delimiter,
            Some(want),
            "{:?}",
            String::from_utf8_lossy(body)
        );
        let r = open(&p, &csv_opts()).unwrap();
        assert!(!r.columns().is_empty());
    }
}

#[test]
fn csv_without_header_names_columns() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "n.csv", b"1,2,3\n4,5,6\n");
    let mut o = csv_opts();
    o.header = false;
    let (cols, rows) = read_all(&p, &o).unwrap();
    assert_eq!(cols, vec!["column_1", "column_2", "column_3"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].number, 1);
    assert_eq!(rows[0].cells, vec![text("1"), text("2"), text("3")]);
}

#[test]
fn csv_duplicate_and_empty_header_names() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "d.csv", b"id,name,name,,id,name_2\n1,2,3,4,5,6\n");
    let (cols, _) = read_all(&p, &csv_opts()).unwrap();
    // A suffix never takes a name the header spells itself (`name_2` stays the 6th column's).
    assert_eq!(
        cols,
        vec!["id", "name", "name_3", "column_4", "id_2", "name_2"]
    );
}

#[test]
fn csv_wrong_field_count_names_the_record() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "w.csv", b"a,b\n1,2\n3,secret-value,9\n");
    let mut r = open(&p, &csv_opts()).unwrap();
    assert!(r.next_record().unwrap().is_some());
    let err = r.next_record().unwrap_err();
    assert_eq!(err.record, Some(3));
    assert!(!err.message.contains("secret-value"), "{}", err.message);
}

#[test]
fn csv_invalid_utf8_is_an_error_with_record_number() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "u.csv", b"a,b\n1,2\n3,caf\xE9\n");
    let mut r = open(&p, &csv_opts()).unwrap();
    assert!(r.next_record().unwrap().is_some());
    let err = r.next_record().unwrap_err();
    assert_eq!(err.record, Some(3));
    assert!(err.message.contains("UTF-8"), "{}", err.message);
}

#[test]
fn csv_invalid_utf8_in_header_is_an_error() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "h.csv", b"a,\xFF\n1,2\n");
    let err = open(&p, &csv_opts()).err().unwrap();
    assert_eq!(err.record, Some(1));
}

#[test]
fn csv_large_file_streams_to_the_end() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("big.csv");
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        writeln!(f, "id,name,amount").unwrap();
        for i in 0..200_000u32 {
            writeln!(f, "{i},name {i},{}.25", i % 1000).unwrap();
        }
    }
    let mut r = open(&path, &csv_opts()).unwrap();
    let total = r.total_bytes();
    assert_eq!(total, std::fs::metadata(&path).unwrap().len());
    let mut n = 0u64;
    let mut last_progress = 0;
    while let Some(rec) = r.next_record().unwrap() {
        n += 1;
        assert_eq!(rec.cells.len(), 3);
        assert!(r.bytes_read() >= last_progress);
        last_progress = r.bytes_read();
    }
    assert_eq!(n, 200_000);
    assert_eq!(r.bytes_read(), total);
}

// ---------------------------------------------------------------- JSON

#[test]
fn json_array_of_objects_streams_cells() {
    let dir = TempDir::new().unwrap();
    let body = r#" [
        {"id": 1, "name": "An", "meta": {"a": [1, 2]}, "big": 12345678901234567890, "huge": 1e400, "ok": true, "none": null},
        {"id": 2.50, "name": "Bé", "meta": [1], "big": -0.0, "huge": 1E-7, "ok": false}
    ] "#;
    let p = write_file(&dir, "a.json", body.as_bytes());
    let (cols, rows) = read_all(&p, &opts(SourceFormat::Json)).unwrap();
    assert_eq!(
        cols,
        vec!["id", "name", "meta", "big", "huge", "ok", "none"]
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].number, 1);
    assert_eq!(rows[1].number, 2);
    assert_eq!(
        rows[0].cells,
        vec![
            num("1"),
            text("An"),
            Cell::Json(r#"{"a": [1, 2]}"#.into()),
            num("12345678901234567890"),
            num("1e400"),
            Cell::Bool(true),
            Cell::Null,
        ]
    );
    assert_eq!(
        rows[1].cells,
        vec![
            num("2.50"),
            text("B\u{e9}"),
            Cell::Json("[1]".into()),
            num("-0.0"),
            num("1E-7"),
            Cell::Bool(false),
            Cell::Null, // missing key
        ]
    );
}

#[test]
fn json_non_object_element_is_an_error() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "e.json", br#"[{"a": 1}, "secret-text", {"a": 2}]"#);
    let err = read_all(&p, &opts(SourceFormat::Json)).unwrap_err();
    assert_eq!(err.record, Some(2));
    assert!(!err.message.contains("secret-text"), "{}", err.message);
}

#[test]
fn json_top_level_must_be_an_array() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "o.json", br#"{"a": 1}"#);
    assert!(read_all(&p, &opts(SourceFormat::Json)).is_err());
}

#[test]
fn json_syntax_error_never_echoes_values() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "s.json", br#"[{"a": "secret-text" "b": 1}]"#);
    let err = read_all(&p, &opts(SourceFormat::Json)).unwrap_err();
    assert_eq!(err.record, Some(1));
    assert!(!err.message.contains("secret"), "{}", err.message);
}

#[test]
fn json_empty_array_has_no_columns() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "z.json", b"[ ]");
    let (cols, rows) = read_all(&p, &opts(SourceFormat::Json)).unwrap();
    assert!(cols.is_empty());
    assert!(rows.is_empty());
}

#[test]
fn json_key_order_and_late_key_warning() {
    let dir = TempDir::new().unwrap();
    let mut body = String::from("[");
    for i in 0..1_000 {
        if i > 0 {
            body.push(',');
        }
        if i == 3 {
            body.push_str(&format!(r#"{{"b": {i}, "a": 1, "c": "x"}}"#));
        } else {
            body.push_str(&format!(r#"{{"b": {i}, "a": 1}}"#));
        }
    }
    body.push_str(r#", {"b": 1000, "late": 5, "a": 2}, {"late": 6}]"#);
    let p = write_file(&dir, "k.json", body.as_bytes());
    let mut r = open(&p, &opts(SourceFormat::Json)).unwrap();
    assert_eq!(r.columns(), &["b", "a", "c"]);
    let mut rows = Vec::new();
    while let Some(rec) = r.next_record().unwrap() {
        rows.push(rec);
    }
    assert_eq!(rows.len(), 1_002);
    assert_eq!(rows[1_000].cells, vec![num("1000"), num("2"), Cell::Null]);
    let w = r.warnings();
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("late"), "{}", w[0]);
    assert!(w[0].contains("1001"), "{}", w[0]);
}

// ---------------------------------------------------------------- JSONL

#[test]
fn jsonl_skips_blank_lines() {
    let dir = TempDir::new().unwrap();
    let body = b"{\"a\": 1, \"b\": \"x\"}\r\n\n   \n{\"b\": \"y\", \"a\": null}\n\n";
    let p = write_file(&dir, "l.jsonl", body);
    let (cols, rows) = read_all(&p, &opts(SourceFormat::Jsonl)).unwrap();
    assert_eq!(cols, vec!["a", "b"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].number, 1);
    assert_eq!(rows[1].number, 4);
    assert_eq!(rows[1].cells, vec![Cell::Null, text("y")]);
}

#[test]
fn jsonl_bad_line_names_the_line() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "b.jsonl", b"{\"a\": 1}\n[1, 2]\n");
    let err = read_all(&p, &opts(SourceFormat::Jsonl)).unwrap_err();
    assert_eq!(err.record, Some(2));
    let p = write_file(&dir, "c.jsonl", b"{\"a\": 1}\n{\"a\": \"secret\n");
    let err = read_all(&p, &opts(SourceFormat::Jsonl)).unwrap_err();
    assert_eq!(err.record, Some(2));
    assert!(!err.message.contains("secret"));
}

// ---------------------------------------------------------------- XLSX

fn write_xlsx(path: &Path) {
    use rust_xlsxwriter::{ExcelDateTime, Format, Workbook};
    let mut wb = Workbook::new();
    let date_fmt = Format::new().set_num_format("yyyy-mm-dd");
    let dt_fmt = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    let ws = wb.add_worksheet().set_name("People").unwrap();
    ws.write_string(0, 0, "name").unwrap();
    ws.write_string(0, 1, "age").unwrap();
    ws.write_string(0, 2, "score").unwrap();
    ws.write_string(0, 3, "active").unwrap();
    ws.write_string(0, 4, "born").unwrap();
    ws.write_string(0, 5, "seen").unwrap();
    ws.write_string(1, 0, "An").unwrap();
    ws.write_number(1, 1, 42).unwrap();
    ws.write_number(1, 2, 1.5).unwrap();
    ws.write_boolean(1, 3, true).unwrap();
    let d = ExcelDateTime::from_ymd(2024, 1, 2).unwrap();
    ws.write_datetime_with_format(1, 4, &d, &date_fmt).unwrap();
    let dt = ExcelDateTime::from_ymd(2024, 1, 2)
        .unwrap()
        .and_hms(3, 4, 5)
        .unwrap();
    ws.write_datetime_with_format(1, 5, &dt, &dt_fmt).unwrap();
    // Row 3: empty cells in the middle.
    ws.write_string(2, 0, "Binh").unwrap();
    ws.write_number(2, 3 + 2, 0.25).unwrap();
    let ws2 = wb.add_worksheet().set_name("Other").unwrap();
    ws2.write_number(0, 0, 1).unwrap();
    ws2.write_number(0, 1, 2).unwrap();
    ws2.write_number(1, 0, -3).unwrap();
    ws2.write_string(1, 1, "z").unwrap();
    wb.save(path).unwrap();
}

#[test]
fn xlsx_reads_types_dates_and_empty_cells() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("x.xlsx");
    write_xlsx(&p);
    let o = opts(SourceFormat::Xlsx);
    let (cols, rows) = read_all(&p, &o).unwrap();
    assert_eq!(cols, vec!["name", "age", "score", "active", "born", "seen"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].number, 2);
    assert_eq!(
        rows[0].cells,
        vec![
            text("An"),
            num("42"),
            num("1.5"),
            Cell::Bool(true),
            Cell::DateTime("2024-01-02".into()),
            Cell::DateTime("2024-01-02T03:04:05".into()),
        ]
    );
    assert_eq!(rows[1].number, 3);
    assert_eq!(
        rows[1].cells,
        vec![
            text("Binh"),
            Cell::Null,
            Cell::Null,
            Cell::Null,
            Cell::Null,
            num("0.25"),
        ]
    );
}

#[test]
fn xlsx_second_sheet_and_no_header() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("x.xlsx");
    write_xlsx(&p);
    let mut o = opts(SourceFormat::Xlsx);
    o.sheet = Some("Other".into());
    o.header = false;
    let ins = inspect(&p, &o).unwrap();
    assert_eq!(ins.sheets, vec!["People", "Other"]);
    assert_eq!(ins.columns, vec!["column_1", "column_2"]);
    assert_eq!(ins.rows.len(), 2);
    assert_eq!(ins.rows[0].cells, vec![num("1"), num("2")]);
    assert_eq!(ins.rows[1].cells, vec![num("-3"), text("z")]);

    o.header = true;
    let (cols, rows) = read_all(&p, &o).unwrap();
    assert_eq!(cols, vec!["1", "2"]);
    assert_eq!(rows.len(), 1);

    o.sheet = Some("Missing".into());
    assert!(open(&p, &o).is_err());
}

#[test]
fn xlsx_progress_reaches_total() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("x.xlsx");
    write_xlsx(&p);
    let mut r = open(&p, &opts(SourceFormat::Xlsx)).unwrap();
    while r.next_record().unwrap().is_some() {}
    assert_eq!(r.bytes_read(), r.total_bytes());
    assert_eq!(r.total_bytes(), std::fs::metadata(&p).unwrap().len());
}

#[test]
fn xlsx_garbage_file_is_an_error() {
    let dir = TempDir::new().unwrap();
    let p = write_file(&dir, "bad.xlsx", b"not a zip at all");
    assert!(open(&p, &opts(SourceFormat::Xlsx)).is_err());
}

// ---------------------------------------------------------------- inspect / facts

#[test]
fn inspect_returns_at_most_100_rows() {
    let dir = TempDir::new().unwrap();
    let mut body = String::from("a,b\n");
    for i in 0..250 {
        body.push_str(&format!("{i},{i}\n"));
    }
    let p = write_file(&dir, "i.csv", body.as_bytes());
    let ins = inspect(&p, &csv_opts()).unwrap();
    assert_eq!(ins.rows.len(), 100);
    assert_eq!(ins.columns, vec!["a", "b"]);
    assert_eq!(ins.delimiter, Some(','));
    assert!(ins.sheets.is_empty());
    assert_eq!(ins.total_bytes, body.len() as u64);

    let p = write_file(&dir, "i.jsonl", b"{\"a\":1}\n");
    let ins = inspect(&p, &opts(SourceFormat::Jsonl)).unwrap();
    assert_eq!(ins.delimiter, None);
    assert_eq!(ins.rows.len(), 1);
}

#[test]
fn file_facts_hash_matches_sha2() {
    let dir = TempDir::new().unwrap();
    let mut bytes = Vec::new();
    for i in 0..(3 * 1024 * 1024 + 17) {
        bytes.push((i % 251) as u8);
    }
    let p = write_file(&dir, "facts.bin", &bytes);
    let facts: FileFacts = file_facts(&p).unwrap();
    assert_eq!(facts.name, "facts.bin");
    assert_eq!(facts.size, bytes.len() as u64);
    assert_eq!(facts.sha256, hex::encode(Sha256::digest(&bytes)));

    let p = write_file(&dir, "empty.bin", b"");
    let facts = file_facts(&p).unwrap();
    assert_eq!(facts.size, 0);
    assert_eq!(facts.sha256, hex::encode(Sha256::digest(b"")));
}

#[test]
fn missing_file_is_an_error() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("nope.csv");
    let err = open(&p, &csv_opts()).err().unwrap();
    assert_eq!(err.record, None);
    assert!(file_facts(&p).is_err());
}
