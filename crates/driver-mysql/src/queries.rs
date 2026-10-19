//! SQL used by the introspector. Every schema / table name is bound as a `?` parameter;
//! `SHOW` statements, which cannot bind, are built with `quote_ident` by the caller.
//! Results are ordered again in Rust (byte order) so they do not depend on collations.

/// Quotes an identifier for MySQL: wrapped in backticks, inner backticks doubled.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

pub(crate) const LIST_DATABASES: &str = "SHOW DATABASES";

/// `TABLE_ROWS` / `TABLE_COMMENT` for every table and view of one database (`?` = db).
pub(crate) const TABLE_STATS: &str = "SELECT TABLE_NAME, TABLE_ROWS, TABLE_COMMENT \
     FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?";

/// MariaDB sequences (`?` = db).
pub(crate) const SEQUENCES: &str = "SELECT TABLE_NAME, TABLE_COMMENT \
     FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'SEQUENCE'";

/// Functions or procedures (`?` = db, routine type).
pub(crate) const ROUTINES: &str = "SELECT ROUTINE_NAME, ROUTINE_COMMENT \
     FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = ?";

/// Triggers of a database (`?` = db). Triggers carry no comment.
pub(crate) const TRIGGERS: &str = "SELECT TRIGGER_NAME, NULL \
     FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ?";

/// Events of a database (`?` = db).
pub(crate) const EVENTS: &str = "SELECT EVENT_NAME, EVENT_COMMENT \
     FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ?";

/// One table's row (`?` = db, table).
pub(crate) const TABLE: &str = "SELECT TABLE_TYPE, ENGINE, TABLE_COLLATION, CREATE_OPTIONS, \
     TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH, TABLE_COMMENT \
     FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";

/// Columns by ordinal (`?` = db, table).
pub(crate) const COLUMNS: &str = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_TYPE, \
     IS_NULLABLE, COLUMN_DEFAULT, EXTRA, GENERATION_EXPRESSION, COLUMN_COMMENT \
     FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
     ORDER BY ORDINAL_POSITION";

/// Index parts (`?` = db, table). Servers without functional indexes have no
/// `EXPRESSION` column, so a NULL stands in for it.
pub(crate) const INDEXES: &str = "SELECT INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME, \
     INDEX_TYPE, NULL \
     FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
     ORDER BY INDEX_NAME, SEQ_IN_INDEX";

/// [`INDEXES`] with functional-index expressions (`caps.functional_index`).
pub(crate) const INDEXES_WITH_EXPRESSION: &str = "SELECT INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, \
     COLUMN_NAME, INDEX_TYPE, EXPRESSION \
     FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
     ORDER BY INDEX_NAME, SEQ_IN_INDEX";

/// Foreign key parts (`?` = db, table).
pub(crate) const FOREIGN_KEYS: &str = "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, \
     k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, \
     r.UPDATE_RULE, r.DELETE_RULE \
     FROM information_schema.KEY_COLUMN_USAGE k \
     JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
       ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
      AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
      AND r.TABLE_NAME = k.TABLE_NAME \
     WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
     ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION";

/// MySQL 8.0.16+: `CHECK_CONSTRAINTS` has no table column, so join `TABLE_CONSTRAINTS`
/// (`?` = db, table).
pub(crate) const CHECKS_MYSQL: &str = "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE \
     FROM information_schema.CHECK_CONSTRAINTS cc \
     JOIN information_schema.TABLE_CONSTRAINTS tc \
       ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA \
      AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME \
     WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK'";

/// MariaDB: `CHECK_CONSTRAINTS` carries `TABLE_NAME` (`?` = db, table).
pub(crate) const CHECKS_MARIADB: &str = "SELECT CONSTRAINT_NAME, CHECK_CLAUSE \
     FROM information_schema.CHECK_CONSTRAINTS \
     WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ?";

/// Triggers of one table (`?` = db, table).
pub(crate) const TABLE_TRIGGERS: &str = "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION \
     FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?";

/// Partitioning of a partitioned table (`?` = db, table).
pub(crate) const PARTITIONING: &str = "SELECT PARTITION_METHOD, PARTITION_EXPRESSION \
     FROM information_schema.PARTITIONS \
     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND PARTITION_NAME IS NOT NULL \
     ORDER BY PARTITION_ORDINAL_POSITION LIMIT 1";
