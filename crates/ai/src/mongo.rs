//! The fill-in-the-middle prompt of the MongoDB console: as [`crate::prompt::build`] does
//! for SQL, with the console's own language (split, redact) and a header of `//` comments
//! naming the database and its collections.
//!
//! Built in two steps, so the caller samples the fields of only the collections the
//! statement names: [`mongo_draft`] checks the text for secrets and finds the statement at
//! the cursor; [`MongoDraft::referenced`] picks the collections it names out of the
//! database's list; [`MongoDraft::build`] writes the prompt.
//!
//! The console language's redaction says only whether it hid something, not where, so any
//! secret in the document refuses the request ([`Skip::Secret`]): nothing it would hide is
//! ever sent.

use crate::prompt::{
    clean, cut_suffix, member_line, prefix_start, push_other_names, FimPrompt, Skip, HEADER_MAX,
};
use dnt_core::command::CommandLanguage;
use dnt_query::Utf16Map;

/// The first header line: what the text is.
pub const MONGO_HEADER_FIRST: &str = "// MongoDB shell (mongosh subset, no JavaScript)\n";

/// A collection (or view) of the database, with its sampled top-level field names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionBrief {
    pub name: String,
    /// Empty when not known (not sampled, or the sample failed).
    pub fields: Vec<String>,
}

/// The editor text, checked for secrets, and the statement at the cursor.
#[derive(Debug, Clone)]
pub struct MongoDraft<'a> {
    text: &'a str,
    /// Byte offset of the cursor.
    cursor: usize,
    /// Byte offsets where the commands start.
    starts: Vec<usize>,
    /// Byte range of the command the cursor is in (at least up to the cursor).
    current: Option<(usize, usize)>,
}

/// Checks `text` with `language`'s redaction (any secret: [`Skip::Secret`]) and finds the
/// command at the editor's UTF-16 `cursor_utf16`.
pub fn mongo_draft<'a>(
    language: &dyn CommandLanguage,
    text: &'a str,
    cursor_utf16: u32,
) -> Result<MongoDraft<'a>, Skip> {
    if language.redact(text).changed {
        return Err(Skip::Secret);
    }
    let cursor = Utf16Map::new(text).to_byte(cursor_utf16);
    let spans = language.split(text);
    // As for SQL: the last command starting at or before the cursor, when only whitespace
    // lies between its end and the cursor (no `;` yet).
    let current = spans
        .iter()
        .rev()
        .find(|s| s.range.0 <= cursor)
        .filter(|s| text[s.range.1.min(cursor)..cursor].trim().is_empty())
        .map(|s| (s.range.0, s.range.1.max(cursor)));
    Ok(MongoDraft {
        text,
        cursor,
        starts: spans.iter().map(|s| s.range.0).collect(),
        current,
    })
}

impl MongoDraft<'_> {
    /// Indexes into `names` (the database's collections) of those the command at the cursor
    /// names, in order of first reference: `db.<name>` (the longest dotted name the list
    /// has), `db.getCollection("…")` and `db["…"]`. Names match exactly (MongoDB's are
    /// case-sensitive).
    pub fn referenced(&self, names: &[&str]) -> Vec<usize> {
        let Some((from, to)) = self.current else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for chain in references(&self.text[from..to]) {
            let hit = (1..=chain.len()).rev().find_map(|n| {
                let name = chain[..n].join(".");
                names.iter().position(|c| *c == name)
            });
            if let Some(k) = hit {
                if !out.contains(&k) {
                    out.push(k);
                }
            }
        }
        out
    }

    /// The prompt: the header (database, the collections the command names with their
    /// fields, then the other collection names; at most [`HEADER_MAX`] bytes) and the text
    /// before the cursor, and the text after it.
    pub fn build(&self, database: Option<&str>, collections: &[CollectionBrief]) -> FimPrompt {
        let (text, cursor) = (self.text, self.cursor);
        let statement_head = self
            .current
            .map(|(start, _)| text[start..cursor].to_string())
            .unwrap_or_default();
        let prefix_from = prefix_start(text, cursor, self.starts.iter().copied());
        FimPrompt {
            prompt: self.header(database, collections) + &text[prefix_from..cursor],
            suffix: cut_suffix(&text[cursor..]).to_string(),
            statement_head,
        }
    }

    fn header(&self, database: Option<&str>, collections: &[CollectionBrief]) -> String {
        let mut out = String::from(MONGO_HEADER_FIRST);
        if let Some(db) = database {
            out.push_str(&format!("// Database: {}\n", clean(db)));
        }
        let names: Vec<&str> = collections.iter().map(|c| c.name.as_str()).collect();
        let referenced = self.referenced(&names);
        let mut listed = vec![false; collections.len()];
        if !referenced.is_empty() && out.len() + "// Collections:\n".len() <= HEADER_MAX {
            out.push_str("// Collections:\n");
            for k in referenced {
                match collection_line(&collections[k], HEADER_MAX - out.len()) {
                    Some(line) => {
                        out.push_str(&line);
                        listed[k] = true;
                    }
                    None => break,
                }
            }
        }
        let others: Vec<String> = collections
            .iter()
            .zip(&listed)
            .filter(|(_, &l)| !l)
            .map(|(c, _)| clean(&c.name))
            .collect();
        push_other_names(&mut out, "// Other collections: ", "//   ", others);
        out
    }
}

/// `//   name(field, …)\n` within `room` bytes (`//   name\n` when no field is known).
fn collection_line(c: &CollectionBrief, room: usize) -> Option<String> {
    let name = clean(&c.name);
    if c.fields.is_empty() {
        let line = format!("//   {name}\n");
        return (line.len() <= room).then_some(line);
    }
    let fields: Vec<String> = c.fields.iter().map(|f| clean(f)).collect();
    member_line("//   ", &name, &fields, room)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(char),
    /// A number or anything else that names nothing.
    Other,
}

/// The collections `stmt` names, each as its dotted chain (`db.a.b.find` → `[a, b, find]`;
/// `db["x"]` → `[x]`).
fn references(stmt: &str) -> Vec<Vec<String>> {
    let toks = scan(stmt);
    let ident = |i: usize| match toks.get(i) {
        Some(Tok::Ident(s)) => Some(s.as_str()),
        _ => None,
    };
    let punct = |i: usize, c: char| toks.get(i) == Some(&Tok::Punct(c));
    let string = |i: usize| match toks.get(i) {
        Some(Tok::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let mut out = Vec::new();
    for i in 0..toks.len() {
        // `db`, not a member named `db` (`x.db`).
        if ident(i) != Some("db") || (i > 0 && punct(i - 1, '.')) {
            continue;
        }
        if punct(i + 1, '[') && punct(i + 3, ']') {
            if let Some(name) = string(i + 2) {
                out.push(vec![name]);
            }
            continue;
        }
        if !punct(i + 1, '.') {
            continue;
        }
        match ident(i + 2) {
            Some("getCollection") => {
                if punct(i + 3, '(') {
                    if let Some(name) = string(i + 4) {
                        out.push(vec![name]);
                    }
                }
            }
            // Another database's collection.
            Some("getSiblingDB") => {}
            Some(first) => {
                let mut chain = vec![first.to_string()];
                let mut j = i + 3;
                while punct(j, '.') {
                    match ident(j + 1) {
                        Some(next) => chain.push(next.to_string()),
                        None => break,
                    }
                    j += 2;
                }
                out.push(chain);
            }
            None => {}
        }
    }
    out
}

/// The tokens of `s` as far as references need them: identifiers, strings (escapes
/// decoded), punctuation; comments and whitespace skipped. Never panics.
fn scan(s: &str) -> Vec<Tok> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if matches!(c, '\'' | '"' | '`') {
            let mut value = String::new();
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' {
                    i += 1;
                    match chars.get(i) {
                        Some('n') => value.push('\n'),
                        Some('r') => value.push('\r'),
                        Some('t') => value.push('\t'),
                        Some(&other) => value.push(other),
                        None => {}
                    }
                } else {
                    value.push(chars[i]);
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(value));
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let from = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            out.push(Tok::Ident(chars[from..i].iter().collect()));
        } else if c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(Tok::Other);
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::{PREFIX_MAX, SUFFIX_MAX};
    use crate::test_lang::TestLang;

    fn coll(name: &str, fields: &[&str]) -> CollectionBrief {
        CollectionBrief {
            name: name.into(),
            fields: fields.iter().map(|f| f.to_string()).collect(),
        }
    }

    /// `marked` with the cursor at `|` (removed).
    fn split_marked(marked: &str) -> (String, u32) {
        let at = marked.find('|').expect("cursor marker");
        let text = marked.replacen('|', "", 1);
        let cursor = text[..at].encode_utf16().count() as u32;
        (text, cursor)
    }

    fn build_at(
        marked: &str,
        db: Option<&str>,
        collections: &[CollectionBrief],
    ) -> Result<FimPrompt, Skip> {
        let (text, cursor) = split_marked(marked);
        let draft = mongo_draft(&TestLang, &text, cursor)?;
        Ok(draft.build(db, collections))
    }

    /// The prompt without its header.
    fn body(p: &FimPrompt) -> &str {
        let header_end = p
            .prompt
            .lines()
            .take_while(|l| l.starts_with("// "))
            .map(|l| l.len() + 1)
            .sum::<usize>();
        &p.prompt[header_end..]
    }

    fn header(p: &FimPrompt) -> &str {
        &p.prompt[..p.prompt.len() - body(p).len()]
    }

    fn shop() -> Vec<CollectionBrief> {
        vec![
            coll("audit", &["_id", "at"]),
            coll("orders", &["_id", "status", "total"]),
            coll("customers", &["_id", "email"]),
        ]
    }

    #[test]
    fn header_lists_referenced_collections_first() {
        let p = build_at("db.orders.find({ |})", Some("shop"), &shop()).unwrap();
        assert_eq!(
            header(&p),
            "// MongoDB shell (mongosh subset, no JavaScript)\n\
             // Database: shop\n\
             // Collections:\n\
             //   orders(_id, status, total)\n\
             // Other collections: audit, customers\n"
        );
        assert_eq!(body(&p), "db.orders.find({ ");
        assert_eq!(p.suffix, "})");

        // In order of first reference.
        let p = build_at(
            "db.orders.find({ c: db.customers.findOne(), |})",
            Some("shop"),
            &shop(),
        )
        .unwrap();
        assert!(
            header(&p).ends_with(
                "// Collections:\n\
                 //   orders(_id, status, total)\n\
                 //   customers(_id, email)\n\
                 // Other collections: audit\n"
            ),
            "{}",
            p.prompt
        );
    }

    #[test]
    fn header_without_references_lists_names_only() {
        let p = build_at("|", Some("shop"), &shop()).unwrap();
        assert_eq!(
            p.prompt,
            "// MongoDB shell (mongosh subset, no JavaScript)\n\
             // Database: shop\n\
             // Other collections: audit, customers, orders\n"
        );
    }

    #[test]
    fn references_by_get_collection_brackets_and_dotted_names() {
        let colls = [
            coll("order items", &["sku"]),
            coll("app", &["a"]),
            coll("app.logs", &["level"]),
            coll("orders", &["_id"]),
        ];
        let names: Vec<&str> = colls.iter().map(|c| c.name.as_str()).collect();
        let refs = |marked: &str| {
            let (text, cursor) = split_marked(marked);
            mongo_draft(&TestLang, &text, cursor)
                .unwrap()
                .referenced(&names)
        };
        assert_eq!(refs("db.getCollection(\"order items\").find(|)"), [0]);
        assert_eq!(refs("db['order items'].find(|)"), [0]);
        // The longest dotted name the database has.
        assert_eq!(refs("db.app.logs.find(|)"), [2]);
        assert_eq!(refs("db.app.find(|)"), [1]);
        assert_eq!(refs("db . orders . find(|)"), [3]);
        // Another database's collection, a database method, a member named `db`, strings and
        // comments name nothing here.
        assert!(refs("db.getSiblingDB('x').orders.find(|)").is_empty());
        assert!(refs("db.getName(|)").is_empty());
        assert!(refs("db.orders2.find({ a: x.db.orders }|)").is_empty());
        assert!(refs("db.x.find({ a: 'db.orders' }) // db.orders|").is_empty());
        // Only the command at the cursor.
        assert_eq!(refs("db.orders.find()\ndb.app.find(|)"), [1]);
        assert!(refs("db.orders.find();\n|").is_empty());

        let p = build_at("db.app.logs.find(|)", None, &colls).unwrap();
        assert!(
            header(&p).contains("//   app.logs(level)\n"),
            "{}",
            p.prompt
        );
    }

    #[test]
    fn unknown_fields_list_the_name_alone() {
        let p = build_at("db.orders.find(|)", Some("shop"), &[coll("orders", &[])]).unwrap();
        assert!(
            header(&p).ends_with("// Collections:\n//   orders\n"),
            "{}",
            p.prompt
        );
    }

    #[test]
    fn no_database_header_has_the_first_line_only() {
        let p = build_at("db.orders.find(|)", None, &[]).unwrap();
        assert_eq!(
            p.prompt,
            "// MongoDB shell (mongosh subset, no JavaScript)\ndb.orders.find("
        );
        assert_eq!(p.suffix, ")");
    }

    #[test]
    fn header_respects_the_caps() {
        let fields: Vec<String> = (0..50)
            .map(|i| format!("field_with_a_rather_long_name_{i}"))
            .collect();
        let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
        let colls: Vec<CollectionBrief> = (0..500)
            .map(|i| coll(&format!("collection_{i}"), &fields))
            .collect();
        let p = build_at(
            "db.collection_1.find({ a: db.collection_2.findOne(), b: db.collection_3.findOne() |})",
            Some("shop"),
            &colls,
        )
        .unwrap();
        let h = header(&p);
        assert!(
            h.len() <= HEADER_MAX,
            "{}
{h}",
            h.len()
        );
        assert!(h.contains("//   collection_1("), "{h}");
        assert!(h.contains("…)"), "a long collection is cut: {h}");
        for line in h.lines() {
            assert!(line.starts_with("// "), "{line:?}");
        }

        // At most 200 other names.
        let colls: Vec<CollectionBrief> = (0..500).map(|i| coll(&format!("c{i}"), &[])).collect();
        let p = build_at("|", Some("shop"), &colls).unwrap();
        let h = header(&p);
        assert!(h.len() <= HEADER_MAX, "{}", h.len());
        let listed: usize = h
            .lines()
            .skip(2)
            .map(|l| {
                l.trim_start_matches("// Other collections: ")
                    .trim_start_matches("//   ")
                    .split(", ")
                    .count()
            })
            .sum();
        assert_eq!(listed, 200, "{h}");
        for line in h.lines() {
            assert!(line.starts_with("// "), "{line:?}");
            assert!(line.len() <= 100, "{line:?}");
        }
    }

    #[test]
    fn header_sanitizes_names() {
        let colls = [coll("a\nb", &["x\r\ny"])];
        let p = build_at("db.getCollection('a\\nb').find(|)", Some("s\nhop"), &colls).unwrap();
        for line in header(&p).lines() {
            assert!(line.starts_with("// "), "{line:?}");
        }
        assert!(header(&p).contains("// Database: s hop\n"), "{}", p.prompt);
    }

    #[test]
    fn a_secret_anywhere_refuses() {
        for marked in [
            "db.runCommand({createUser:'u', pwd:'p'})\ndb.orders.find(|)",
            "db.orders.find(|)\ndb.runCommand({createUser:'u', pwd:'p'})",
            "db.x.insertOne({ uri: 'mongodb://u:p@h' })|",
        ] {
            assert_eq!(
                build_at(marked, Some("shop"), &[]).unwrap_err(),
                Skip::Secret,
                "{marked}"
            );
        }
    }

    #[test]
    fn prefix_is_cut_at_a_command_start() {
        let filler: String = (0..1000)
            .map(|i| format!("db.c.find({{ n: {i} }})\n"))
            .collect();
        let p = build_at(&format!("{filler}db.orders.find({{ |"), None, &[]).unwrap();
        let b = body(&p);
        assert!(b.len() <= PREFIX_MAX, "{}", b.len());
        assert!(b.starts_with("db.c.find({ n: "), "{:?}", &b[..20]);
        assert!(b.ends_with("db.orders.find({ "));
    }

    #[test]
    fn suffix_is_cut_at_a_line() {
        let tail: String = (0..500)
            .map(|i| format!("db.c.find({{ n: {i} }})\n"))
            .collect();
        let p = build_at(&format!("db.c.find(|)\n{tail}"), None, &[]).unwrap();
        assert!(p.suffix.len() <= SUFFIX_MAX);
        assert!(
            p.suffix.ends_with("})"),
            "{:?}",
            &p.suffix[p.suffix.len() - 10..]
        );
    }

    #[test]
    fn utf16_cursor_maps_to_bytes() {
        let p = build_at("db.c.find({ name: \"Tiếng 😀\", |x: 1 })", None, &[]).unwrap();
        assert!(body(&p).ends_with("\"Tiếng 😀\", "), "{}", p.prompt);
        assert_eq!(p.suffix, "x: 1 })");
    }

    #[test]
    fn statement_head_is_the_command_before_the_cursor() {
        let head = |marked: &str| build_at(marked, None, &[]).unwrap().statement_head;
        assert_eq!(
            head("db.a.find()\ndb.orders.find({ |})"),
            "db.orders.find({ "
        );
        assert_eq!(
            head("db.orders.find({\n  status: 1,\n  |"),
            "db.orders.find({\n  status: 1,\n  "
        );
        assert_eq!(head("db.a.find();\n|"), "");
        assert_eq!(head("|"), "");
    }
}
