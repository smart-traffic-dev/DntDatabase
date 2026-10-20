//! SQL used by the introspector. Everything reads `pg_catalog`; every schema / object name
//! is bound (`$1`, `$2`), and per-relation queries take the relation's OID (looked up by
//! name first) as `$1`. Results are ordered again in Rust (byte order) so they do not
//! depend on collations.

pub(crate) const CURRENT_DATABASE: &str = "SELECT current_database()::text";

pub(crate) const LIST_DATABASES: &str = "SELECT datname::text FROM pg_database \
     WHERE NOT datistemplate AND has_database_privilege(datname, 'CONNECT')";

/// Schemas the user may use, without TOAST and temporary schemas.
pub(crate) const LIST_SCHEMAS: &str = "SELECT n.nspname::text FROM pg_namespace n \
     WHERE n.nspname !~ '^pg_(toast|temp_)' AND has_schema_privilege(n.oid, 'USAGE')";

/// Any table privilege makes a table, view or materialized view visible.
const ANY_TABLE_PRIVILEGE: &str = "'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER'";

/// Relations of one schema (`$1` = schema, `$2` = the wanted `relkind`s as one string,
/// e.g. `rp`): name, comment, row estimate. Partitions are left out (their parent is
/// listed). Reads no statistics beyond `reltuples` (a column, not a computation).
pub(crate) fn list_relations() -> String {
    format!(
        "SELECT c.relname::text, obj_description(c.oid, 'pg_class'), \
         CASE WHEN c.reltuples < 0 THEN NULL ELSE c.reltuples::int8 END \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND strpos($2, c.relkind::text) > 0 AND NOT c.relispartition \
         AND has_table_privilege(c.oid, {ANY_TABLE_PRIVILEGE})"
    )
}

/// Column names and types (`format_type`, as in table details) of every visible table,
/// partitioned table, view, materialized view and foreign table of one schema (`$1` =
/// schema), ordered by relation then `attnum`. Same privilege filter as
/// [`list_relations`]; partitions are left out like there.
pub(crate) fn list_columns() -> String {
    format!(
        "SELECT c.relname::text, a.attname::text, format_type(a.atttypid, a.atttypmod) \
         FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
         AND NOT c.relispartition AND a.attnum > 0 AND NOT a.attisdropped \
         AND has_table_privilege(c.oid, {ANY_TABLE_PRIVILEGE}) \
         ORDER BY c.relname, a.attnum"
    )
}

/// `has_sequence_privilege` raises an error on a non-sequence, and the planner may test it
/// before `relkind`: the `CASE` keeps it to sequences.
const SEQUENCE_PRIVILEGE: &str =
    "CASE WHEN c.relkind = 'S' THEN has_sequence_privilege(c.oid, 'USAGE,SELECT') END";

/// Sequences of one schema (`$1` = schema): name, comment.
pub(crate) fn list_sequences() -> String {
    format!(
        "SELECT c.relname::text, obj_description(c.oid, 'pg_class') \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind = 'S' AND {SEQUENCE_PRIVILEGE}"
    )
}

/// Functions or procedures (`$1` = schema, `$2` = `prokind`, `f` or `p`): name, comment.
pub(crate) const LIST_ROUTINES: &str = "SELECT p.proname::text, obj_description(p.oid, 'pg_proc') \
     FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
     WHERE n.nspname = $1 AND p.prokind::text = $2 \
     AND has_function_privilege(p.oid, 'EXECUTE')";

/// User triggers on visible relations of one schema (`$1` = schema): name, comment.
pub(crate) fn list_triggers() -> String {
    format!(
        "SELECT t.tgname::text, obj_description(t.oid, 'pg_trigger') \
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND NOT t.tgisinternal \
         AND has_table_privilege(c.oid, {ANY_TABLE_PRIVILEGE})"
    )
}

/// Enum types and stand-alone composite types (`$1` = schema): name, comment.
pub(crate) const LIST_TYPES: &str = "SELECT t.typname::text, obj_description(t.oid, 'pg_type') \
     FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
     LEFT JOIN pg_class c ON c.oid = t.typrelid \
     WHERE n.nspname = $1 AND (t.typtype = 'e' OR (t.typtype = 'c' AND c.relkind = 'c')) \
     AND has_type_privilege(t.oid, 'USAGE')";

/// One visible relation (`$1` = schema, `$2` = name, `$3` = accepted `relkind`s): oid,
/// relkind, row estimate, comment, partition key, tablespace, total size.
pub(crate) fn relation() -> String {
    format!(
        "SELECT c.oid, c.relkind::text, \
         CASE WHEN c.reltuples < 0 OR c.relkind = 'v' THEN NULL ELSE c.reltuples::int8 END, \
         obj_description(c.oid, 'pg_class'), \
         CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END, \
         ts.spcname::text, \
         CASE WHEN c.relkind = 'v' THEN NULL ELSE pg_total_relation_size(c.oid) END \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_tablespace ts ON ts.oid = c.reltablespace \
         WHERE n.nspname = $1 AND c.relname = $2 AND strpos($3, c.relkind::text) > 0 \
         AND has_table_privilege(c.oid, {ANY_TABLE_PRIVILEGE})"
    )
}

/// Columns by `attnum` (`$1` = relation oid): name, attnum, type, not null, default /
/// generation expression, `attidentity`, `attgenerated`, comment, explicit (non-default)
/// collation name.
pub(crate) const COLUMNS: &str = "SELECT a.attname::text, a.attnum::int4, \
     format_type(a.atttypid, a.atttypmod), a.attnotnull, pg_get_expr(ad.adbin, ad.adrelid), \
     a.attidentity::text, a.attgenerated::text, col_description(a.attrelid, a.attnum), \
     CASE WHEN a.attcollation <> 0 AND a.attcollation <> t.typcollation \
          THEN co.collname::text END \
     FROM pg_attribute a \
     JOIN pg_type t ON t.oid = a.atttypid \
     LEFT JOIN pg_collation co ON co.oid = a.attcollation \
     LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
     WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
     ORDER BY a.attnum";

/// Indexes (`$1` = relation oid): name, unique, primary, access method, key columns,
/// expressions, `CREATE INDEX` statement, definition of the constraint it backs.
pub(crate) const INDEXES: &str = "SELECT ic.relname::text, i.indisunique, i.indisprimary, \
     am.amname::text, \
     ARRAY(SELECT a.attname::text FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord) \
           JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
           WHERE k.ord <= i.indnkeyatts ORDER BY k.ord), \
     pg_get_expr(i.indexprs, i.indrelid), pg_get_indexdef(i.indexrelid), \
     pg_get_constraintdef(con.oid) \
     FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid \
     JOIN pg_am am ON am.oid = ic.relam \
     LEFT JOIN pg_constraint con ON con.conindid = i.indexrelid AND con.conrelid = i.indrelid \
          AND con.contype IN ('p', 'u', 'x') \
     WHERE i.indrelid = $1";

/// Foreign keys (`$1` = relation oid): name, columns, referenced schema / table /
/// columns, `confupdtype`, `confdeltype`, `pg_get_constraintdef`, the referenced table as
/// the server may print it unqualified (`quote_ident(relname)`) and schema-qualified.
pub(crate) const FOREIGN_KEYS: &str = "SELECT con.conname::text, \
     ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(attnum, ord) \
           JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum \
           ORDER BY k.ord), \
     nf.nspname::text, cf.relname::text, \
     ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(attnum, ord) \
           JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum \
           ORDER BY k.ord), \
     con.confupdtype::text, con.confdeltype::text, pg_get_constraintdef(con.oid), \
     quote_ident(cf.relname), quote_ident(nf.nspname) || '.' || quote_ident(cf.relname) \
     FROM pg_constraint con JOIN pg_class cf ON cf.oid = con.confrelid \
     JOIN pg_namespace nf ON nf.oid = cf.relnamespace \
     WHERE con.conrelid = $1 AND con.contype = 'f'";

/// Check constraints (`$1` = relation oid): name, expression, `pg_get_constraintdef`.
pub(crate) const CHECKS: &str = "SELECT con.conname::text, pg_get_expr(con.conbin, con.conrelid), \
     pg_get_constraintdef(con.oid) \
     FROM pg_constraint con WHERE con.conrelid = $1 AND con.contype = 'c'";

/// User triggers of one relation (`$1` = relation oid): name, `tgtype` bits.
pub(crate) const TABLE_TRIGGERS: &str = "SELECT t.tgname::text, t.tgtype::int4 \
     FROM pg_trigger t WHERE t.tgrelid = $1 AND NOT t.tgisinternal";

/// View / materialized view body (`$1` = oid).
pub(crate) const VIEW_DEF: &str = "SELECT pg_get_viewdef($1::oid, true)";

/// Function or procedure definitions (`$1` = schema, `$2` = name, `$3` = `prokind`), one
/// row per overload, ordered by argument list.
pub(crate) const ROUTINE_DEFS: &str = "SELECT pg_get_functiondef(p.oid) \
     FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
     WHERE n.nspname = $1 AND p.proname = $2 AND p.prokind::text = $3 \
     AND has_function_privilege(p.oid, 'EXECUTE') \
     ORDER BY pg_get_function_identity_arguments(p.oid)";

/// Trigger definitions (`$1` = schema, `$2` = trigger name), one per table carrying it.
pub(crate) fn trigger_defs() -> String {
    format!(
        "SELECT pg_get_triggerdef(t.oid, true) \
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND t.tgname = $2 AND NOT t.tgisinternal \
         AND has_table_privilege(c.oid, {ANY_TABLE_PRIVILEGE}) \
         ORDER BY c.relname"
    )
}

/// A visible sequence (`$1` = schema, `$2` = name): type, start, increment, min, max,
/// cache, cycle.
pub(crate) fn sequence() -> String {
    format!(
        "SELECT format_type(s.seqtypid, NULL), s.seqstart, \
         s.seqincrement, s.seqmin, s.seqmax, s.seqcache, s.seqcycle \
         FROM pg_sequence s JOIN pg_class c ON c.oid = s.seqrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2 AND {SEQUENCE_PRIVILEGE}"
    )
}

/// A visible enum or stand-alone composite type (`$1` = schema, `$2` = name): oid,
/// `typtype`, `typrelid`.
pub(crate) const TYPE: &str = "SELECT t.oid, t.typtype::text, t.typrelid \
     FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
     LEFT JOIN pg_class c ON c.oid = t.typrelid \
     WHERE n.nspname = $1 AND t.typname = $2 \
     AND (t.typtype = 'e' OR (t.typtype = 'c' AND c.relkind = 'c')) \
     AND has_type_privilege(t.oid, 'USAGE')";

/// Enum labels in order (`$1` = type oid).
pub(crate) const ENUM_LABELS: &str =
    "SELECT enumlabel::text FROM pg_enum WHERE enumtypid = $1 ORDER BY enumsortorder";

/// Attributes of a composite type (`$1` = its `typrelid`): name, type.
pub(crate) const COMPOSITE_ATTRIBUTES: &str = "SELECT a.attname::text, \
     format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
     WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum";
