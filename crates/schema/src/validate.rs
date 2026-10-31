//! Draft checks shown at the offending cell before anything is rendered.

use crate::model::*;
use crate::types::type_info;
use dnt_core::error::DbError;
use dnt_core::server::{DbKind, MySqlCapabilities, ServerInfo, ServerVariant};
use dnt_core::sql::build::{check_expression, BuildError};
use dnt_core::sql::ddl::validate_identifier;
use dnt_core::sql::Dialect;
use std::collections::HashSet;

/// Problems in `def` for `server`, in element order.
///
/// **Precondition (ruling T5-7):** every element that is locked in the stored base
/// has been replaced by the base copy (matched by `original`) before this call, so a
/// locked element with `original: Some` is server-derived. Such an element skips the
/// text-safety checks (`check_expression`, raw words, enum members) as well as the
/// shape and capability rules; it is never rendered. A locked element with
/// `original: None` is an error, and every unlocked element gets every check.
///
/// Names (ruling T5-8): `validate_identifier` applies only to names the user creates
/// or changes (`original` None or different from `name`; the table name against
/// `original_name`). Unchanged names and referenced names (FK target table, schema
/// and columns, key/index columns) are only checked for being empty or holding a NUL:
/// they are always quoted on render.
pub fn validate(def: &TableDef, server: &ServerInfo) -> Vec<DraftError> {
    let mut v = Validator {
        server,
        kind: server.kind,
        dialect: Dialect::for_kind(server.kind.as_sql()),
        trusted: false,
        errs: Vec::new(),
    };
    v.name(
        ElementRef::Table,
        "name",
        &def.name,
        def.original_name.as_deref(),
    );
    if def.columns.is_empty() {
        v.err(
            ElementRef::Table,
            None,
            "A table needs at least one column.",
        );
    }
    let mut seen = HashSet::new();
    for c in &def.columns {
        let el = ElementRef::Column(c.name.clone());
        if v.name(el.clone(), "name", &c.name, c.original.as_deref())
            && !seen.insert(v.key(&c.name))
        {
            v.err(el.clone(), Some("name"), "Another column has this name.");
        }
        v.enter(&el, &c.original, c.locked);
        v.column(c, !c.locked);
    }
    let columns: HashSet<String> = def.columns.iter().map(|c| v.key(&c.name)).collect();
    if let Some(pk) = &def.primary_key {
        v.enter(&ElementRef::PrimaryKey, &pk.original, pk.locked);
        v.primary_key(pk, &columns, !pk.locked);
    }
    let mut seen = HashSet::new();
    for i in &def.indexes {
        let el = ElementRef::Index(i.name.clone());
        if v.name(el.clone(), "name", &i.name, i.original.as_deref())
            && !seen.insert(v.key(&i.name))
        {
            v.err(el.clone(), Some("name"), "Another index has this name.");
        }
        v.enter(&el, &i.original, i.locked);
        v.index(i, &columns, !i.locked);
    }
    let mut seen = HashSet::new();
    for f in &def.foreign_keys {
        let el = ElementRef::ForeignKey(f.name.clone());
        if v.name(el.clone(), "name", &f.name, f.original.as_deref())
            && !seen.insert(v.key(&f.name))
        {
            v.err(
                el.clone(),
                Some("name"),
                "Another foreign key has this name.",
            );
        }
        v.enter(&el, &f.original, f.locked);
        v.foreign_key(f, &columns, !f.locked);
    }
    let mut seen = HashSet::new();
    for c in &def.checks {
        let el = ElementRef::Check(c.name.clone());
        if v.name(el.clone(), "name", &c.name, c.original.as_deref())
            && !seen.insert(v.key(&c.name))
        {
            v.err(el.clone(), Some("name"), "Another check has this name.");
        }
        v.enter(&el, &c.original, c.locked);
        v.check(c, !c.locked);
    }
    if def.options.locked && def.original_name.is_none() {
        v.err(
            ElementRef::Options,
            Some("locked"),
            "A new table's options cannot be locked.",
        );
    }
    v.trusted = def.options.locked && def.original_name.is_some();
    v.options(&def.options);
    v.errs
}

const FK_ACTIONS: &[&str] = &[
    "RESTRICT",
    "CASCADE",
    "SET NULL",
    "NO ACTION",
    "SET DEFAULT",
];
const MATCH_TYPES: &[&str] = &["SIMPLE", "FULL", "PARTIAL"];

struct Validator<'a> {
    server: &'a ServerInfo,
    kind: DbKind,
    dialect: Dialect,
    /// The current element is locked and server-derived: skip text-safety checks.
    trusted: bool,
    errs: Vec<DraftError>,
}

impl Validator<'_> {
    fn err(&mut self, element: ElementRef, field: Option<&str>, message: impl Into<String>) {
        self.errs.push(DraftError {
            element,
            field: field.map(str::to_string),
            message: message.into(),
        });
    }

    fn mysql(&self) -> bool {
        self.kind == DbKind::MySql
    }

    /// Starts the checks of one element. Only an element that exists on the server
    /// can be locked; a locked one is then server-derived (see [`validate`]).
    fn enter(&mut self, el: &ElementRef, original: &Option<String>, locked: bool) {
        if locked && original.is_none() {
            self.err(
                el.clone(),
                Some("locked"),
                "A new element cannot be locked.",
            );
        }
        self.trusted = locked && original.is_some();
    }

    /// Name comparison key: MySQL names compare case-insensitively.
    fn key(&self, name: &str) -> String {
        match self.kind {
            DbKind::MySql => name.to_lowercase(),
            DbKind::Postgres => name.to_string(),
        }
    }

    /// Checks an element's own name; `true` when it is valid. `validate_identifier`
    /// applies only when the name is new or changed (`original` None or different).
    fn name(&mut self, el: ElementRef, field: &str, name: &str, original: Option<&str>) -> bool {
        if !self.reference(el.clone(), field, name) {
            return false;
        }
        if original == Some(name) {
            return true;
        }
        if let Err(e) = validate_identifier(self.kind.as_sql(), name) {
            let message = match e {
                DbError::Config { message, .. } => message,
                other => other.to_string(),
            };
            self.err(el, Some(field), message);
            return false;
        }
        true
    }

    /// Checks an unchanged or referenced name (always quoted on render): not empty,
    /// no NUL. `true` when it is valid.
    fn reference(&mut self, el: ElementRef, field: &str, name: &str) -> bool {
        if name.trim().is_empty() {
            self.err(el, Some(field), "A name is required.");
            return false;
        }
        if name.contains('\0') {
            self.err(el, Some(field), "A name cannot contain a NUL character.");
            return false;
        }
        true
    }

    /// Checks a user-written SQL expression.
    fn expression(&mut self, el: &ElementRef, field: &str, text: &str) {
        if self.trusted {
            return;
        }
        if let Err(e) = check_expression(text, self.dialect) {
            let reason = match e {
                BuildError::Invalid { reason, .. } => reason,
                other => other.to_string(),
            };
            self.err(
                el.clone(),
                Some(field),
                format!("Invalid expression: {reason}."),
            );
        }
    }

    /// A name the server would take unquoted (engine, charset, collation, row format).
    fn word(&mut self, el: &ElementRef, field: &str, value: &Option<String>) {
        if self.trusted {
            return;
        }
        if let Some(v) = value {
            if v.is_empty() || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                self.err(
                    el.clone(),
                    Some(field),
                    "Use letters, digits and underscores only.",
                );
            }
        }
    }

    /// Every name in `names` is one of the table's columns.
    fn columns_exist(
        &mut self,
        el: &ElementRef,
        field: &str,
        names: &[String],
        columns: &HashSet<String>,
    ) {
        for n in names {
            if !columns.contains(&self.key(n)) {
                self.err(
                    el.clone(),
                    Some(field),
                    format!("There is no column \"{n}\"."),
                );
            }
        }
    }

    /// Enum / set members: safe to render (always) and distinct (`shape`).
    fn values(&mut self, el: &ElementRef, values: &[String], shape: bool) {
        let mut seen = HashSet::new();
        for v in values {
            if !self.trusted && (v.contains('\0') || (self.mysql() && v.contains('\\'))) {
                self.err(
                    el.clone(),
                    Some("values"),
                    "Values cannot contain a backslash or a NUL character.",
                );
            } else if shape && !seen.insert(self.key(v)) {
                self.err(
                    el.clone(),
                    Some("values"),
                    format!("'{v}' is listed twice."),
                );
            }
        }
    }

    /// `shape`: also check the shape and capability rules (the element is not locked).
    fn column(&mut self, c: &ColumnDef, shape: bool) {
        let el = ElementRef::Column(c.name.clone());
        let mut collation_ok = true;
        match &c.data_type {
            TypeSpec::Custom { text } => {
                if text.trim().is_empty() {
                    self.err(el.clone(), Some("data_type"), "A type is required.");
                } else {
                    self.expression(&el, "data_type", text);
                }
            }
            TypeSpec::Known {
                base,
                length,
                precision,
                scale,
                unsigned,
                values,
            } => match type_info(base, self.server) {
                // The base is rendered as is: it must be a catalog name, locked or not.
                None => self.err(
                    el.clone(),
                    Some("data_type"),
                    format!("{base} is not a type of this server."),
                ),
                Some(_) if !shape => self.values(&el, values, false),
                Some(ti) => {
                    collation_ok = ti.collation;
                    self.values(&el, values, true);
                    let params = [
                        ("length", "Length", ti.length, length),
                        ("precision", "Precision", ti.precision, precision),
                        ("scale", "Scale", ti.scale, scale),
                    ];
                    for (field, label, usage, value) in params {
                        match (usage, value) {
                            (ParamUse::Required, None) => self.err(
                                el.clone(),
                                Some(field),
                                format!("{label} is required for {base}."),
                            ),
                            (ParamUse::None, Some(_)) => self.err(
                                el.clone(),
                                Some(field),
                                format!("{base} takes no {}.", field),
                            ),
                            _ => {}
                        }
                    }
                    match (precision, scale) {
                        (None, Some(_)) if ti.scale != ParamUse::None => {
                            self.err(el.clone(), Some("scale"), "Scale needs a precision.")
                        }
                        (Some(p), Some(s)) if s > p => self.err(
                            el.clone(),
                            Some("scale"),
                            "Scale cannot be larger than precision.",
                        ),
                        _ => {}
                    }
                    if *unsigned && !ti.unsigned {
                        self.err(
                            el.clone(),
                            Some("unsigned"),
                            format!("{base} cannot be unsigned."),
                        );
                    }
                    if ti.values && values.is_empty() {
                        self.err(el.clone(), Some("values"), format!("{base} needs values."));
                    } else if !ti.values && !values.is_empty() {
                        self.err(
                            el.clone(),
                            Some("values"),
                            format!("{base} takes no values."),
                        );
                    }
                }
            },
        }
        if let Some(DefaultSpec::Expression { text }) = &c.default {
            self.expression(&el, "default", text);
        }
        if let Some(g) = &c.generated {
            self.expression(&el, "generated", g);
        }
        if let Some(u) = &c.using {
            self.expression(&el, "using", u);
            if self.mysql() {
                self.err(el.clone(), Some("using"), "USING is PostgreSQL only.");
            }
        }
        if let Some(on_update) = &c.on_update {
            self.expression(&el, "on_update", on_update);
        }
        if c.charset.is_some() && self.mysql() {
            self.word(&el, "charset", &c.charset);
        }
        if shape {
            self.column_shape(&el, c, collation_ok);
        }
        if c.collation.is_some() {
            if self.mysql() {
                self.word(&el, "collation", &c.collation);
            } else if c
                .collation
                .as_deref()
                .is_some_and(|s| s.contains('\0') || s.is_empty())
            {
                self.err(el.clone(), Some("collation"), "Invalid collation name.");
            }
        }
    }

    /// The column rules that only concern an editable column.
    fn column_shape(&mut self, el: &ElementRef, c: &ColumnDef, collation_ok: bool) {
        if c.auto_increment && !self.mysql() {
            self.err(
                el.clone(),
                Some("auto_increment"),
                "Use an identity column on PostgreSQL.",
            );
        }
        if c.identity.is_some() && self.mysql() {
            self.err(
                el.clone(),
                Some("identity"),
                "Identity columns are PostgreSQL only; use AUTO_INCREMENT.",
            );
        }
        if c.on_update.is_some() && !self.mysql() {
            self.err(el.clone(), Some("on_update"), "ON UPDATE is MySQL only.");
        }
        if c.charset.is_some() && !self.mysql() {
            self.err(
                el.clone(),
                Some("charset"),
                "PostgreSQL columns have no character set.",
            );
        }
        if c.collation.is_some() && !collation_ok {
            self.err(el.clone(), Some("collation"), "This type has no collation.");
        }
    }

    /// Referenced column names: not empty, no NUL (they are quoted into DDL).
    fn refs(&mut self, el: &ElementRef, field: &str, names: &[String]) {
        for n in names {
            self.reference(el.clone(), field, n);
        }
    }

    fn primary_key(&mut self, pk: &KeyDef, columns: &HashSet<String>, shape: bool) {
        let el = ElementRef::PrimaryKey;
        if let Some(name) = &pk.name {
            self.name(el.clone(), "name", name, pk.original.as_deref());
        }
        self.refs(&el, "columns", &pk.columns);
        if !shape {
            return;
        }
        if pk.columns.is_empty() {
            self.err(el.clone(), Some("columns"), "Pick at least one column.");
        }
        self.columns_exist(&el, "columns", &pk.columns, columns);
        self.no_repeats(&el, "columns", &pk.columns);
    }

    fn no_repeats(&mut self, el: &ElementRef, field: &str, names: &[String]) {
        let mut seen = HashSet::new();
        for n in names {
            if !seen.insert(self.key(n)) {
                self.err(el.clone(), Some(field), format!("\"{n}\" is listed twice."));
            }
        }
    }

    fn index(&mut self, i: &IndexDef, columns: &HashSet<String>, shape: bool) {
        let el = ElementRef::Index(i.name.clone());
        // Safety, locked or not: part expressions, referenced names, the raw method.
        let mut part_columns = Vec::new();
        for p in &i.parts {
            if let Some(c) = &p.column {
                part_columns.push(c.clone());
            }
            if let Some(e) = &p.expression {
                self.expression(&el, "parts", e);
            }
        }
        self.refs(&el, "parts", &part_columns);
        self.refs(&el, "include", &i.include);
        if let Some(m) = &i.method {
            let bad = m.is_empty() || !m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if bad && !self.trusted {
                self.err(el.clone(), Some("method"), "Invalid index method.");
            }
        }
        if !shape {
            return;
        }
        if i.parts.is_empty() {
            self.err(el.clone(), Some("parts"), "Pick at least one column.");
        }
        let caps = MySqlCapabilities::for_server(self.server);
        for p in &i.parts {
            match (&p.column, &p.expression) {
                (Some(_), None) => {}
                (None, Some(_)) => {
                    if self.mysql() && !caps.functional_index {
                        self.err(
                            el.clone(),
                            Some("parts"),
                            "Expression indexes need MySQL 8.0.13 or later.",
                        );
                    }
                }
                _ => self.err(
                    el.clone(),
                    Some("parts"),
                    "Each part is either a column or an expression.",
                ),
            }
            if p.prefix.is_some() && !self.mysql() {
                self.err(el.clone(), Some("parts"), "Prefix lengths are MySQL only.");
            }
            if p.prefix == Some(0) {
                self.err(
                    el.clone(),
                    Some("parts"),
                    "A prefix length must be positive.",
                );
            }
        }
        self.columns_exist(&el, "parts", &part_columns, columns);
        self.no_repeats(&el, "parts", &part_columns);
        if !i.include.is_empty() {
            if self.mysql() {
                self.err(el.clone(), Some("include"), "INCLUDE is PostgreSQL only.");
            } else {
                self.columns_exist(&el, "include", &i.include, columns);
            }
        }
        if !i.visible && self.server.variant != ServerVariant::MySql8 {
            self.err(
                el.clone(),
                Some("visible"),
                "Invisible indexes need MySQL 8.0.",
            );
        }
    }

    fn foreign_key(&mut self, f: &FkDef, columns: &HashSet<String>, shape: bool) {
        let el = ElementRef::ForeignKey(f.name.clone());
        // Safety, locked or not: names, the raw actions and match type.
        self.refs(&el, "columns", &f.columns);
        self.reference(el.clone(), "ref_table", &f.ref_table);
        if let Some(s) = &f.ref_schema {
            self.reference(el.clone(), "ref_schema", s);
        }
        self.refs(&el, "ref_columns", &f.ref_columns);
        for (field, action) in [("on_update", &f.on_update), ("on_delete", &f.on_delete)] {
            if !FK_ACTIONS.iter().any(|a| a.eq_ignore_ascii_case(action)) {
                self.err(el.clone(), Some(field), "Unknown referential action.");
            }
        }
        if let Some(m) = &f.match_type {
            if !MATCH_TYPES.iter().any(|t| t.eq_ignore_ascii_case(m)) {
                self.err(el.clone(), Some("match_type"), "Unknown match type.");
            } else if self.mysql() && shape {
                self.err(el.clone(), Some("match_type"), "MATCH is PostgreSQL only.");
            }
        }
        if !shape {
            return;
        }
        if f.columns.is_empty() {
            self.err(el.clone(), Some("columns"), "Pick at least one column.");
        }
        self.columns_exist(&el, "columns", &f.columns, columns);
        if f.ref_columns.len() != f.columns.len() {
            self.err(
                el.clone(),
                Some("ref_columns"),
                "Pick as many referenced columns as columns.",
            );
        }
        if (f.deferrable || f.initially_deferred) && self.mysql() {
            self.err(
                el.clone(),
                Some("deferrable"),
                "Deferrable constraints are PostgreSQL only.",
            );
        } else if f.initially_deferred && !f.deferrable {
            self.err(
                el.clone(),
                Some("initially_deferred"),
                "Only a deferrable constraint can be initially deferred.",
            );
        }
    }

    fn check(&mut self, c: &CheckDef, shape: bool) {
        let el = ElementRef::Check(c.name.clone());
        if shape && self.mysql() && !MySqlCapabilities::for_server(self.server).check_constraints {
            self.err(
                el.clone(),
                None,
                "CHECK constraints need MySQL 8.0.16 or later.",
            );
        }
        self.expression(&el, "expression", &c.expression);
    }

    fn options(&mut self, o: &TableOptionsDef) {
        let el = ElementRef::Options;
        let fields = [
            ("engine", &o.engine),
            ("charset", &o.charset),
            ("collation", &o.collation),
            ("row_format", &o.row_format),
        ];
        for (field, value) in fields {
            if self.mysql() {
                self.word(&el, field, value);
            } else if value.is_some() {
                self.err(el.clone(), Some(field), "Not available on PostgreSQL.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_table;
    use crate::test_util::*;

    fn col(name: &str, data_type: TypeSpec) -> ColumnDef {
        ColumnDef {
            original: None,
            name: name.into(),
            data_type,
            nullable: true,
            default: None,
            auto_increment: false,
            identity: None,
            on_update: None,
            charset: None,
            collation: None,
            comment: None,
            using: None,
            generated: None,
            locked: false,
        }
    }

    fn int() -> TypeSpec {
        TypeSpec::Known {
            base: "int".into(),
            length: None,
            precision: None,
            scale: None,
            unsigned: false,
            values: vec![],
        }
    }

    fn decimal(p: Option<u32>, s: Option<u32>) -> TypeSpec {
        TypeSpec::Known {
            base: "decimal".into(),
            length: None,
            precision: p,
            scale: s,
            unsigned: false,
            values: vec![],
        }
    }

    fn base() -> TableDef {
        let mut t = new_table("t", &mysql8());
        t.columns.push(col("a", int()));
        t
    }

    fn has(errs: &[DraftError], element: ElementRef, field: Option<&str>) -> bool {
        errs.iter()
            .any(|e| e.element == element && e.field.as_deref() == field)
    }

    #[test]
    fn validate_reports() {
        let my = mysql8();
        assert_eq!(validate(&base(), &my), vec![]);

        // Duplicate column name.
        let mut t = base();
        t.columns.push(col("a", int()));
        let errs = validate(&t, &my);
        assert!(
            has(&errs, ElementRef::Column("a".into()), Some("name")),
            "{errs:?}"
        );
        // MySQL column names are case-insensitive.
        let mut t = base();
        t.columns.push(col("A", int()));
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("A".into()),
            Some("name")
        ));

        // Empty names.
        let mut t = base();
        t.columns.push(col("", int()));
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("".into()),
            Some("name")
        ));
        let mut t = base();
        t.name = String::new();
        assert!(has(&validate(&t, &my), ElementRef::Table, Some("name")));

        // No columns.
        let mut t = base();
        t.columns.clear();
        t.primary_key = None;
        assert!(has(&validate(&t, &my), ElementRef::Table, None));

        // decimal scale > precision; scale without precision.
        let mut t = base();
        t.columns.push(col("d", decimal(Some(4), Some(6))));
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("d".into()),
            Some("scale")
        ));
        let mut t = base();
        t.columns.push(col("d", decimal(None, Some(2))));
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("d".into()),
            Some("scale")
        ));

        // Missing required length.
        let mut t = base();
        t.columns.push(col(
            "v",
            TypeSpec::Known {
                base: "varchar".into(),
                length: None,
                precision: None,
                scale: None,
                unsigned: false,
                values: vec![],
            },
        ));
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("v".into()),
            Some("length")
        ));

        // FK column count differs from the referenced columns.
        let mut t = base();
        t.foreign_keys.push(FkDef {
            original: None,
            name: "fk".into(),
            columns: vec!["a".into(), "id".into()],
            ref_schema: None,
            ref_table: "u".into(),
            ref_columns: vec!["id".into()],
            on_update: "RESTRICT".into(),
            on_delete: "CASCADE".into(),
            match_type: None,
            deferrable: false,
            initially_deferred: false,
            locked: false,
        });
        assert!(has(
            &validate(&t, &my),
            ElementRef::ForeignKey("fk".into()),
            Some("ref_columns")
        ));

        // CHECK on MySQL 5.7.
        let mut t = new_table("t", &mysql57());
        t.checks.push(CheckDef {
            original: None,
            name: "c".into(),
            expression: "id > 0".into(),
            locked: false,
        });
        let errs = validate(&t, &mysql57());
        assert!(
            errs.iter()
                .any(|e| e.element == ElementRef::Check("c".into())
                    && e.message.contains("CHECK constraints need MySQL 8.0.16")),
            "{errs:?}"
        );
        assert_eq!(validate(&t, &mysql8()), vec![]);
        assert_eq!(validate(&t, &mariadb()), vec![]);

        // Names failing ddl::validate_identifier.
        let mut t = base();
        t.columns.push(col("a.b", int()));
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a.b".into()),
            Some("name")
        ));
        let mut t = base();
        t.name = "x".repeat(65);
        assert!(has(&validate(&t, &my), ElementRef::Table, Some("name")));

        // Expression default failing check_expression.
        let mut t = base();
        t.columns[1].default = Some(DefaultSpec::Expression {
            text: "1; DROP TABLE t".into(),
        });
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a".into()),
            Some("default")
        ));
        // A value default is a literal: anything goes.
        let mut t = base();
        t.columns[1].default = Some(DefaultSpec::Value {
            text: "1; DROP TABLE t".into(),
        });
        assert_eq!(validate(&t, &my), vec![]);

        // Custom type text with `;`.
        let mut t = base();
        t.columns.push(col(
            "c",
            TypeSpec::Custom {
                text: "int; DROP TABLE t".into(),
            },
        ));
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("c".into()),
            Some("data_type")
        ));
    }

    #[test]
    fn validate_server_specific() {
        let my = mysql8();
        let pg = pg();

        // A type the server's catalog does not know.
        let mut t = base();
        t.columns[1].data_type = TypeSpec::Known {
            base: "jsonb".into(),
            length: None,
            precision: None,
            scale: None,
            unsigned: false,
            values: vec![],
        };
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a".into()),
            Some("data_type")
        ));

        // auto_increment / identity / unsigned on the wrong server.
        let mut t = new_table("t", &pg);
        t.columns[0].auto_increment = true;
        assert!(has(
            &validate(&t, &pg),
            ElementRef::Column("id".into()),
            Some("auto_increment")
        ));
        let mut t = new_table("t", &my);
        t.columns[0].identity = Some(dnt_core::meta::IdentityKind::Always);
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("id".into()),
            Some("identity")
        ));

        // enum without values.
        let mut t = base();
        t.columns[1].data_type = TypeSpec::Known {
            base: "enum".into(),
            length: None,
            precision: None,
            scale: None,
            unsigned: false,
            values: vec![],
        };
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a".into()),
            Some("values")
        ));

        // PK and index columns must exist.
        let mut t = base();
        t.primary_key.as_mut().unwrap().columns = vec!["nope".into()];
        assert!(has(
            &validate(&t, &my),
            ElementRef::PrimaryKey,
            Some("columns")
        ));
        let mut t = base();
        t.indexes.push(IndexDef {
            original: None,
            name: "i".into(),
            unique: false,
            parts: vec![IndexPartDef {
                column: Some("nope".into()),
                expression: None,
                prefix: None,
                descending: false,
            }],
            method: None,
            include: vec![],
            visible: true,
            comment: None,
            constraint: false,
            locked: false,
        });
        assert!(has(
            &validate(&t, &my),
            ElementRef::Index("i".into()),
            Some("parts")
        ));

        // Bad options on MySQL; any engine on PG.
        let mut t = base();
        t.options.engine = Some("InnoDB; DROP".into());
        assert!(has(&validate(&t, &my), ElementRef::Options, Some("engine")));
        let mut t = new_table("t", &pg);
        t.options.engine = Some("InnoDB".into());
        assert!(has(&validate(&t, &pg), ElementRef::Options, Some("engine")));

        // A locked server element (T5-7: the backend restored it from the base) skips
        // the text-safety checks; it is never rendered.
        let mut t = base();
        t.columns[1].original = Some("a".into());
        t.columns[1].locked = true;
        t.columns[1].data_type = TypeSpec::Custom {
            text: "geometry srid 4326 -- x".into(),
        };
        assert_eq!(validate(&t, &my), vec![]);
        // The catalog and referential-action checks still apply.
        let mut t = base();
        t.columns[1].original = Some("a".into());
        t.columns[1].locked = true;
        t.columns[1].data_type = TypeSpec::Known {
            base: "int; DROP TABLE t".into(),
            length: None,
            precision: None,
            scale: None,
            unsigned: false,
            values: vec![],
        };
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a".into()),
            Some("data_type")
        ));
        let mut t = base();
        t.foreign_keys.push(FkDef {
            original: Some("fk".into()),
            name: "fk".into(),
            columns: vec!["a".into()],
            ref_schema: None,
            ref_table: "u".into(),
            ref_columns: vec!["id".into()],
            on_update: "CASCADE, DROP COLUMN a".into(),
            on_delete: "CASCADE".into(),
            match_type: None,
            deferrable: false,
            initially_deferred: false,
            locked: true,
        });
        assert!(has(
            &validate(&t, &my),
            ElementRef::ForeignKey("fk".into()),
            Some("on_update")
        ));
        // ... but shape / capability rules are skipped for a locked server element.
        let mut t = base();
        t.columns[1].original = Some("a".into());
        t.columns[1].locked = true;
        t.columns[1].data_type = decimal(Some(4), Some(6));
        t.checks.push(CheckDef {
            original: Some("c".into()),
            name: "c".into(),
            expression: "a > 0".into(),
            locked: true,
        });
        assert_eq!(validate(&t, &mysql57()), vec![]);
        // A new element cannot be locked.
        let mut t = base();
        t.columns[1].locked = true;
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a".into()),
            Some("locked")
        ));
        let mut t = base();
        t.checks.push(CheckDef {
            original: None,
            name: "c".into(),
            expression: "a > 0".into(),
            locked: true,
        });
        assert!(has(
            &validate(&t, &my),
            ElementRef::Check("c".into()),
            Some("locked")
        ));
        let mut t = base();
        t.primary_key.as_mut().unwrap().locked = true;
        assert!(has(
            &validate(&t, &my),
            ElementRef::PrimaryKey,
            Some("locked")
        ));
    }

    #[test]
    fn locked_server_elements_skip_text_checks() {
        let my = mysql8();
        // MySQL 8 information_schema escapes quotes in generation expressions.
        let expr = r"concat(`a`,_utf8mb4\' \')";
        let mut t = base();
        t.columns.push(ColumnDef {
            original: Some("g".into()),
            generated: Some(expr.into()),
            locked: true,
            ..col("g", int())
        });
        assert_eq!(validate(&t, &my), vec![]);
        // The same text as an unlocked column's default expression is refused.
        let mut t = base();
        t.columns[1].default = Some(DefaultSpec::Expression { text: expr.into() });
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a".into()),
            Some("default")
        ));
        // Unlocked existing columns get every check too.
        t.columns[1].original = Some("a".into());
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a".into()),
            Some("default")
        ));
    }

    #[test]
    fn identifier_rules_only_for_new_or_changed_names() {
        let my = mysql8();
        // An unchanged column named `a.b` (it exists on the server) is fine.
        let mut t = base();
        t.original_name = Some("t".into());
        t.columns.push(ColumnDef {
            original: Some("a.b".into()),
            ..col("a.b", int())
        });
        assert_eq!(validate(&t, &my), vec![]);
        // Renaming a column to `a.b` is refused.
        let mut t = base();
        t.columns[1].original = Some("a".into());
        t.columns[1].name = "a.b".into();
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a.b".into()),
            Some("name")
        ));
        // An unchanged table name too long for a new one stays valid; renaming is not.
        let mut t = base();
        let long = "x".repeat(65);
        t.name = long.clone();
        t.original_name = Some(long);
        assert_eq!(validate(&t, &my), vec![]);
        t.original_name = Some("t".into());
        assert!(has(&validate(&t, &my), ElementRef::Table, Some("name")));
        // FK references are only checked for empty / NUL.
        let fk = |ref_col: &str| {
            let mut t = base();
            t.foreign_keys.push(FkDef {
                original: None,
                name: "fk".into(),
                columns: vec!["a".into()],
                ref_schema: Some("other.db".into()),
                ref_table: "u.v".into(),
                ref_columns: vec![ref_col.into()],
                on_update: "RESTRICT".into(),
                on_delete: "CASCADE".into(),
                match_type: None,
                deferrable: false,
                initially_deferred: false,
                locked: false,
            });
            validate(&t, &my)
        };
        assert_eq!(fk("x.y"), vec![]);
        assert!(has(
            &fk(""),
            ElementRef::ForeignKey("fk".into()),
            Some("ref_columns")
        ));
        assert!(has(
            &fk("a\0"),
            ElementRef::ForeignKey("fk".into()),
            Some("ref_columns")
        ));
    }

    #[test]
    fn validate_enum_values() {
        let my = mysql8();
        let with_values = |values: &[&str]| {
            let mut t = base();
            t.columns[1].data_type = TypeSpec::Known {
                base: "enum".into(),
                length: None,
                precision: None,
                scale: None,
                unsigned: false,
                values: values.iter().map(|v| v.to_string()).collect(),
            };
            t
        };
        assert_eq!(validate(&with_values(&["a", "b'c"]), &my), vec![]);
        for bad in [&["a", "b\\c"][..], &["a\0"], &["a", "b", "A"]] {
            assert!(
                has(
                    &validate(&with_values(bad), &my),
                    ElementRef::Column("a".into()),
                    Some("values")
                ),
                "{bad:?}"
            );
        }
        // A locked server column keeps them (from_detail locked it for this reason);
        // a new column marked locked gets the full checks.
        let mut t = with_values(&["a\\"]);
        t.columns[1].original = Some("a".into());
        t.columns[1].locked = true;
        assert_eq!(validate(&t, &my), vec![]);
        t.columns[1].original = None;
        assert!(has(
            &validate(&t, &my),
            ElementRef::Column("a".into()),
            Some("values")
        ));
        // A clause smuggled through a custom type, in any MySQL lexing mode.
        for text in [
            "int, ADD COLUMN evil int",
            r"enum('\', ADD COLUMN x int, ')",
            r#"enum("\", ADD COLUMN x int, ")"#,
        ] {
            let mut t = base();
            t.columns[1].data_type = TypeSpec::Custom { text: text.into() };
            assert!(
                has(
                    &validate(&t, &my),
                    ElementRef::Column("a".into()),
                    Some("data_type")
                ),
                "{text:?}"
            );
        }
    }
}
