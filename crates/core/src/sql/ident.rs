//! Identifier quoting and the `SELECT` statements of the table data view.
//!
//! Identifiers are ALWAYS quoted (never "only when needed"): MySQL in backticks, PostgreSQL
//! in double quotes, the quote character doubled inside. Nothing the user typed is ever
//! interpolated unquoted; numbers (`LIMIT` / `OFFSET`) are formatted from integers.

use crate::server::DbKind;

/// `name` quoted for `kind`: `` `a``b` `` (MySQL) or `"a""b"` (PostgreSQL).
pub fn quote_ident(kind: DbKind, name: &str) -> String {
    match kind {
        DbKind::MySql => format!("`{}`", name.replace('`', "``")),
        DbKind::Postgres => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// The qualified name of a relation: `` `db`.`t` `` on MySQL (the database qualifies it,
/// a schema is not used), `"schema"."t"` on PostgreSQL (the session is on the database;
/// without a schema the name is left unqualified).
pub fn qualified_name(kind: DbKind, database: &str, schema: Option<&str>, name: &str) -> String {
    let prefix = match kind {
        DbKind::MySql => Some(database),
        DbKind::Postgres => schema,
    };
    match prefix {
        Some(p) => format!("{}.{}", quote_ident(kind, p), quote_ident(kind, name)),
        None => quote_ident(kind, name),
    }
}

/// `SELECT * FROM <relation> [ORDER BY <col> ASC|DESC, ...] LIMIT <limit> OFFSET <offset>`.
/// `relation` must come from [`qualified_name`]; `order` lists `(column, ascending)` and
/// the columns are quoted here.
pub fn select_page_sql(
    kind: DbKind,
    relation: &str,
    order: &[(&str, bool)],
    limit: u64,
    offset: u64,
) -> String {
    select_sql(kind, relation, &[], "", order, Some((limit, offset)))
}

/// `SELECT [<extra_cols>, ]* FROM <relation><where_sql> [ORDER BY …] [LIMIT <limit> OFFSET
/// <offset>]`. `where_sql` must come from [`crate::sql::filter::build_where`] or
/// [`crate::sql::build::raw_where_clause`] (empty or ` WHERE …`); `order` as in
/// [`select_page_sql`].
///
/// `extra_cols` are expressions the backend chose (PG `xmin::text`, quoted hidden key
/// columns), spliced as is before `*`. On MySQL the `*` is then qualified
/// (`` `db`.`t`.* ``): MySQL refuses a bare `*` after other select items. With no
/// extra columns the statement is `SELECT * FROM …` as before.
pub fn select_sql(
    kind: DbKind,
    relation: &str,
    extra_cols: &[String],
    where_sql: &str,
    order: &[(&str, bool)],
    page: Option<(u64, u64)>,
) -> String {
    let list = if extra_cols.is_empty() {
        "*".to_string()
    } else {
        let star = match kind {
            DbKind::MySql => format!("{relation}.*"),
            DbKind::Postgres => "*".to_string(),
        };
        format!("{}, {star}", extra_cols.join(", "))
    };
    let order = if order.is_empty() {
        String::new()
    } else {
        let keys: Vec<String> = order
            .iter()
            .map(|(column, asc)| {
                format!(
                    "{} {}",
                    quote_ident(kind, column),
                    if *asc { "ASC" } else { "DESC" }
                )
            })
            .collect();
        format!(" ORDER BY {}", keys.join(", "))
    };
    let page = match page {
        Some((limit, offset)) => format!(" LIMIT {limit} OFFSET {offset}"),
        None => String::new(),
    };
    format!("SELECT {list} FROM {relation}{where_sql}{order}{page}")
}

/// `SELECT COUNT(*) FROM <relation>`.
pub fn count_sql(relation: &str) -> String {
    count_where_sql(relation, "")
}

/// `SELECT COUNT(*) FROM <relation><where_sql>` (`where_sql` as in [`select_sql`]).
pub fn count_where_sql(relation: &str, where_sql: &str) -> String {
    format!("SELECT COUNT(*) FROM {relation}{where_sql}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_are_doubled_per_engine() {
        assert_eq!(quote_ident(DbKind::MySql, "orders"), "`orders`");
        assert_eq!(quote_ident(DbKind::MySql, "we`ird\"col"), "`we``ird\"col`");
        assert_eq!(quote_ident(DbKind::Postgres, "orders"), "\"orders\"");
        assert_eq!(
            quote_ident(DbKind::Postgres, "we`ird\"col"),
            "\"we`ird\"\"col\""
        );
        assert_eq!(quote_ident(DbKind::MySql, "đơn_hàng"), "`đơn_hàng`");
        assert_eq!(quote_ident(DbKind::Postgres, "đơn_hàng"), "\"đơn_hàng\"");
        assert_eq!(quote_ident(DbKind::Postgres, ""), "\"\"");
    }

    #[test]
    fn mysql_qualifies_by_database_postgres_by_schema() {
        assert_eq!(
            qualified_name(DbKind::MySql, "shop", None, "đơn_hàng"),
            "`shop`.`đơn_hàng`"
        );
        assert_eq!(
            qualified_name(DbKind::MySql, "sh`op", Some("ignored"), "t"),
            "`sh``op`.`t`"
        );
        assert_eq!(
            qualified_name(DbKind::Postgres, "shop", Some("pub\"lic"), "Orders"),
            "\"pub\"\"lic\".\"Orders\""
        );
        assert_eq!(qualified_name(DbKind::Postgres, "shop", None, "t"), "\"t\"");
    }

    #[test]
    fn select_page_quotes_the_order_column() {
        let rel = qualified_name(DbKind::MySql, "shop", None, "weird_names");
        assert_eq!(
            select_page_sql(DbKind::MySql, &rel, &[], 101, 0),
            "SELECT * FROM `shop`.`weird_names` LIMIT 101 OFFSET 0"
        );
        assert_eq!(
            select_page_sql(DbKind::MySql, &rel, &[("we`ird\"col", false)], 11, 200),
            "SELECT * FROM `shop`.`weird_names` ORDER BY `we``ird\"col` DESC LIMIT 11 OFFSET 200"
        );
        let rel = qualified_name(DbKind::Postgres, "shop", Some("public"), "đơn_hàng");
        assert_eq!(
            select_page_sql(DbKind::Postgres, &rel, &[("x\"; DROP", true)], 5, 5),
            "SELECT * FROM \"public\".\"đơn_hàng\" ORDER BY \"x\"\"; DROP\" ASC LIMIT 5 OFFSET 5"
        );
        assert_eq!(
            select_page_sql(DbKind::Postgres, &rel, &[("a", true), ("b", true)], 5, 0),
            "SELECT * FROM \"public\".\"đơn_hàng\" ORDER BY \"a\" ASC, \"b\" ASC LIMIT 5 OFFSET 0"
        );
        assert_eq!(
            count_sql(&rel),
            "SELECT COUNT(*) FROM \"public\".\"đơn_hàng\""
        );
    }

    #[test]
    fn select_with_where_and_without_a_page() {
        let rel = qualified_name(DbKind::Postgres, "shop", Some("public"), "t");
        assert_eq!(
            select_sql(
                DbKind::Postgres,
                &rel,
                &[],
                " WHERE (\"a\" = $1)",
                &[("a", false)],
                None
            ),
            "SELECT * FROM \"public\".\"t\" WHERE (\"a\" = $1) ORDER BY \"a\" DESC"
        );
        assert_eq!(
            select_sql(
                DbKind::MySql,
                "`t`",
                &[],
                " WHERE (`a` = ?)",
                &[],
                Some((5, 10))
            ),
            "SELECT * FROM `t` WHERE (`a` = ?) LIMIT 5 OFFSET 10"
        );
        assert_eq!(
            count_where_sql("`t`", " WHERE (`a` IS NULL)"),
            "SELECT COUNT(*) FROM `t` WHERE (`a` IS NULL)"
        );
    }

    #[test]
    fn select_puts_extra_cols_first() {
        let rel = qualified_name(DbKind::Postgres, "shop", Some("public"), "t");
        assert_eq!(
            select_sql(
                DbKind::Postgres,
                &rel,
                &["xmin::text".into()],
                " WHERE (\"id\" = $1)",
                &[("id", true)],
                Some((100, 0))
            ),
            "SELECT xmin::text, * FROM \"public\".\"t\" WHERE (\"id\" = $1) \
             ORDER BY \"id\" ASC LIMIT 100 OFFSET 0"
        );
        // MySQL only accepts `*` after other items when it is qualified.
        let rel = qualified_name(DbKind::MySql, "shop", None, "t");
        assert_eq!(
            select_sql(
                DbKind::MySql,
                &rel,
                &["`hid`".into(), "`k2`".into()],
                "",
                &[("n`x", false)],
                Some((5, 10))
            ),
            "SELECT `hid`, `k2`, `shop`.`t`.* FROM `shop`.`t` ORDER BY `n``x` DESC \
             LIMIT 5 OFFSET 10"
        );
    }
}
