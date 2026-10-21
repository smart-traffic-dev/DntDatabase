//! Table designer model: the editable `TableDef`, the type catalog, building a draft
//! from introspection and validating it (spec §4.2). Pure Rust: no database access.

mod from_detail;
mod model;
mod types;
mod validate;

pub use from_detail::{from_detail, new_table};
pub use model::*;
pub use types::{parse_type, type_catalog, type_text};
pub use validate::validate;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ipc_shapes() {
        let e = DraftError {
            element: ElementRef::Column("a".into()),
            field: Some("name".into()),
            message: "m".into(),
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"element": {"kind": "column", "name": "a"}, "field": "name", "message": "m"})
        );
        assert_eq!(
            serde_json::to_value(ElementRef::Table).unwrap(),
            json!({"kind": "table"})
        );
        assert_eq!(
            serde_json::to_value(TypeSpec::Custom { text: "x".into() }).unwrap(),
            json!({"kind": "custom", "text": "x"})
        );
        assert_eq!(
            serde_json::to_value(DefaultSpec::Value { text: "1".into() }).unwrap(),
            json!({"kind": "value", "text": "1"})
        );
        let t = new_table("t", &test_util::pg());
        let back: TableDef = serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use dnt_core::server::{DbKind, ServerInfo, ServerVariant};

    pub fn server(variant: ServerVariant, version: (u32, u32, u32)) -> ServerInfo {
        ServerInfo {
            kind: if variant == ServerVariant::Postgres {
                DbKind::Postgres
            } else {
                DbKind::MySql
            },
            variant,
            version,
            version_string: format!("{}.{}.{}", version.0, version.1, version.2),
        }
    }

    pub fn mysql8() -> ServerInfo {
        server(ServerVariant::MySql8, (8, 0, 36))
    }
    pub fn mysql57() -> ServerInfo {
        server(ServerVariant::MySql57, (5, 7, 44))
    }
    pub fn mariadb() -> ServerInfo {
        server(ServerVariant::MariaDb, (11, 4, 2))
    }
    pub fn pg() -> ServerInfo {
        server(ServerVariant::Postgres, (17, 0, 0))
    }
}
