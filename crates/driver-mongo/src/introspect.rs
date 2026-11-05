//! The object tree (spec §6.1): databases, collections and views, the detail of a
//! collection inferred from a fixed sample, its indexes, and its definition.
//!
//! Every call goes through the session's `Client`, and every cursor is read to its end, so
//! nothing is left open on the server.

use std::collections::BTreeMap;

use dnt_core::error::DbError;
use dnt_core::meta::{
    ColumnDetail, DatabaseInfo, IndexDetail, IndexPart, ObjectKind, ObjectRef, ObjectSummary,
    TableDetail, TableOptions,
};
use mongodb::bson::{doc, Bson, Document};
use mongodb::{Client, Cursor};

use crate::exec::Fail;
use crate::rows::{column_metas, columns_of};
use crate::shell::{CollMethod, ShellCommand};

/// How many documents the fixed sample holds.
pub(crate) const SAMPLE_SIZE: usize = 100;

/// The name of the index on `_id`, the primary key.
const ID_INDEX: &str = "_id_";

/// The fixed sample (spec §6.1): the documents with the largest `_id`. Never `$sample`, so
/// that the detail, the data view's coercion and an edit's preview and apply all see the
/// same schema while the data does not change.
pub(crate) fn sample_pipeline() -> Vec<Document> {
    vec![
        doc! { "$sort": { "_id": -1 } },
        doc! { "$limit": SAMPLE_SIZE as i32 },
    ]
}

/// The fixed sample of collection `coll` of `db` as a console command, for the session's
/// own execution path (cancel and timeouts included).
pub(crate) fn sample_command(db: Option<String>, coll: String) -> ShellCommand {
    ShellCommand::Coll {
        db,
        coll,
        method: CollMethod::Aggregate,
        args: vec![Bson::Array(
            sample_pipeline().into_iter().map(Bson::Document).collect(),
        )],
        modifiers: Vec::new(),
        explain: None,
    }
}

/// The inferred type of each top-level field of `docs`, in order: `_id` first, then by
/// first appearance. A field with several BSON types is `mixed(a|b)` (sorted). A sample
/// without documents has only `_id`, an `objectId`.
pub(crate) fn sampled_fields(docs: &[Document]) -> Vec<(String, String)> {
    if docs.is_empty() {
        return vec![("_id".into(), "objectId".into())];
    }
    let cols = columns_of(docs);
    column_metas(&cols, docs)
        .into_iter()
        .map(|m| (m.name, m.db_type))
        .collect()
}

/// [`sampled_fields`] by name.
pub(crate) fn field_types(docs: &[Document]) -> BTreeMap<String, String> {
    sampled_fields(docs).into_iter().collect()
}

/// Every document of `cursor`, read to its end (which closes it on the server).
async fn drain(mut cursor: Cursor<Document>) -> Result<Vec<Document>, Fail> {
    let mut docs = Vec::new();
    while cursor.advance().await? {
        docs.push(cursor.deserialize_current()?);
    }
    Ok(docs)
}

/// `listDatabases {nameOnly: true, authorizedDatabases: true}`, sorted by name. System
/// databases are listed too; the app hides them.
pub(crate) async fn list_databases(client: &Client) -> Result<Vec<DatabaseInfo>, Fail> {
    let reply = client
        .database("admin")
        .run_command(doc! { "listDatabases": 1, "nameOnly": true, "authorizedDatabases": true })
        .await?;
    let mut names: Vec<String> = reply
        .get_array("databases")
        .map(|dbs| {
            dbs.iter()
                .filter_map(|d| d.as_document()?.get_str("name").ok().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| DatabaseInfo {
            name,
            key_count: None,
        })
        .collect())
}

/// The `listCollections` filter of the objects of `kind`; `None`: the kind has none.
fn kind_filter(kind: ObjectKind) -> Option<Document> {
    match kind {
        ObjectKind::Collection => Some(doc! { "type": { "$in": ["collection", "timeseries"] } }),
        ObjectKind::View => Some(doc! { "type": "view" }),
        ObjectKind::Table
        | ObjectKind::MaterializedView
        | ObjectKind::Function
        | ObjectKind::Procedure
        | ObjectKind::Trigger
        | ObjectKind::Event
        | ObjectKind::Sequence
        | ObjectKind::Type
        | ObjectKind::Key => None,
    }
}

/// The collections (timeseries included) or views of `db` the user may see, sorted by
/// name; `system.*` collections included (the app hides them). Other kinds: none.
pub(crate) async fn list_objects(
    client: &Client,
    db: &str,
    kind: ObjectKind,
) -> Result<Vec<ObjectSummary>, Fail> {
    let Some(filter) = kind_filter(kind) else {
        return Ok(Vec::new());
    };
    let cursor = client
        .database(db)
        .run_cursor_command(doc! {
            "listCollections": 1,
            "filter": filter,
            "nameOnly": true,
            "authorizedCollections": true,
        })
        .await?;
    let mut names: Vec<String> = drain(cursor)
        .await?
        .iter()
        .filter_map(|d| d.get_str("name").ok().map(str::to_string))
        .collect();
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| ObjectSummary {
            name,
            kind,
            comment: None,
            row_estimate: None,
        })
        .collect())
}

/// Collections and views have a detail; nothing else does on MongoDB.
fn check_kind(obj: &ObjectRef) -> Result<(), Fail> {
    match obj.kind {
        ObjectKind::Collection | ObjectKind::View => Ok(()),
        ObjectKind::Table
        | ObjectKind::MaterializedView
        | ObjectKind::Function
        | ObjectKind::Procedure
        | ObjectKind::Trigger
        | ObjectKind::Event
        | ObjectKind::Sequence
        | ObjectKind::Type
        | ObjectKind::Key => Err(Fail::Db(DbError::Unsupported {
            feature: format!("{:?} objects on MongoDB", obj.kind),
            version: String::new(),
        })),
    }
}

/// The detail of a collection or view (spec §6.1): one column per top-level field of the
/// fixed sample, typed by [`sampled_fields`] (`nullable` except `_id`), and, for a
/// collection, its indexes from `listIndexes`, `_id_` being the primary key.
pub(crate) async fn table_detail(client: &Client, obj: &ObjectRef) -> Result<TableDetail, Fail> {
    check_kind(obj)?;
    let coll = client
        .database(&obj.database)
        .collection::<Document>(&obj.name);
    let sample = drain(coll.aggregate(sample_pipeline()).await?).await?;
    let columns = sampled_fields(&sample)
        .into_iter()
        .enumerate()
        .map(|(i, (name, db_type))| ColumnDetail {
            nullable: name != "_id",
            name,
            ordinal: i as u32 + 1,
            db_type,
            ..ColumnDetail::default()
        })
        .collect();
    let (primary_key, indexes) = match obj.kind {
        ObjectKind::Collection => {
            let cursor = client
                .database(&obj.database)
                .run_cursor_command(doc! { "listIndexes": &obj.name })
                .await?;
            split_indexes(drain(cursor).await?)
        }
        // A view has no indexes of its own (`listIndexes` refuses it).
        _ => (None, Vec::new()),
    };
    Ok(TableDetail {
        object: obj.clone(),
        columns,
        primary_key,
        indexes,
        foreign_keys: Vec::new(),
        checks: Vec::new(),
        triggers: Vec::new(),
        options: TableOptions::default(),
        row_estimate: None,
        size_bytes: None,
        comment: None,
    })
}

/// The primary key (`_id_`) and the other indexes, sorted by name, of `listIndexes`
/// documents. Every collection has a unique `_id`: without an `_id_` index (a clustered
/// collection), the primary key is still `_id`.
fn split_indexes(specs: Vec<Document>) -> (Option<IndexDetail>, Vec<IndexDetail>) {
    let mut primary = None;
    let mut others = Vec::new();
    for spec in &specs {
        let idx = index_detail(spec);
        if idx.primary {
            primary = Some(idx);
        } else {
            others.push(idx);
        }
    }
    others.sort_by(|a, b| a.name.cmp(&b.name));
    let primary = primary.unwrap_or_else(|| IndexDetail {
        name: ID_INDEX.into(),
        unique: true,
        primary: true,
        columns: vec!["_id".into()],
        parts: vec![IndexPart {
            column: Some("_id".into()),
            ..IndexPart::default()
        }],
        ..IndexDetail::default()
    });
    (Some(primary), others)
}

/// One `listIndexes` document: its key fields in order (descending for a negative
/// direction), its method for a special index (`text`, `2dsphere`, `hashed`…), `unique`
/// from its options, partial with a `partialFilterExpression`, hidden as not visible. The
/// definition is the index document as relaxed Extended JSON, without its version.
fn index_detail(spec: &Document) -> IndexDetail {
    let name = spec.get_str("name").unwrap_or_default().to_string();
    let primary = name == ID_INDEX;
    let keys = spec.get_document("key").cloned().unwrap_or_default();
    let mut method = None;
    let parts: Vec<IndexPart> = keys
        .iter()
        .map(|(field, dir)| {
            if let Bson::String(m) = dir {
                method.get_or_insert_with(|| m.clone());
            }
            IndexPart {
                column: Some(field.clone()),
                descending: match dir {
                    Bson::Int32(n) => *n < 0,
                    Bson::Int64(n) => *n < 0,
                    Bson::Double(d) => *d < 0.0,
                    _ => false,
                },
                ..IndexPart::default()
            }
        })
        .collect();
    let mut definition = spec.clone();
    definition.remove("v");
    definition.remove("ns");
    IndexDetail {
        unique: primary || spec.get_bool("unique").unwrap_or(false),
        primary,
        partial: spec.contains_key("partialFilterExpression"),
        columns: keys.keys().cloned().collect(),
        method,
        definition: Some(
            Bson::Document(definition)
                .into_relaxed_extjson()
                .to_string(),
        ),
        parts,
        visible: !spec.get_bool("hidden").unwrap_or(false),
        name,
        ..IndexDetail::default()
    }
}

/// The definition of a collection or view (spec §6.1), as indented JSON: a collection's
/// `options` from `listCollections` (its validator among them), a view's `viewOn` and
/// `pipeline`.
pub(crate) async fn object_ddl(client: &Client, obj: &ObjectRef) -> Result<String, Fail> {
    check_kind(obj)?;
    let cursor = client
        .database(&obj.database)
        .run_cursor_command(doc! { "listCollections": 1, "filter": { "name": &obj.name } })
        .await?;
    let Some(info) = drain(cursor).await?.into_iter().next() else {
        return Err(Fail::Db(not_found(obj)));
    };
    let options = info.get_document("options").cloned().unwrap_or_default();
    let shown = match info.get_str("type") {
        Ok("view") => {
            let mut view = Document::new();
            if let Some(on) = options.get("viewOn") {
                view.insert("viewOn", on.clone());
            }
            view.insert(
                "pipeline",
                options
                    .get("pipeline")
                    .cloned()
                    .unwrap_or(Bson::Array(Vec::new())),
            );
            view
        }
        _ => options,
    };
    serde_json::to_string_pretty(&Bson::Document(shown).into_relaxed_extjson()).map_err(|e| {
        Fail::Db(DbError::Internal {
            message: format!("format the definition: {e}"),
        })
    })
}

/// A collection or view that `listCollections` does not return.
fn not_found(obj: &ObjectRef) -> DbError {
    DbError::Server {
        code: Some("NamespaceNotFound".into()),
        sqlstate: None,
        message: format!(
            "{}.{} does not exist or is not visible to this user",
            obj.database, obj.name
        ),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::oid::ObjectId;

    #[test]
    fn sample_is_fixed() {
        assert_eq!(
            sample_pipeline(),
            [doc! { "$sort": { "_id": -1 } }, doc! { "$limit": 100 }]
        );
        match sample_command(Some("d".into()), "c".into()) {
            ShellCommand::Coll { args, .. } => assert_eq!(
                args,
                [Bson::Array(vec![
                    Bson::Document(doc! { "$sort": { "_id": -1 } }),
                    Bson::Document(doc! { "$limit": 100 }),
                ])]
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn fields_id_first_then_appearance_with_mixed_types() {
        let docs = vec![
            doc! { "b": 1, "_id": ObjectId::new(), "m": "x" },
            doc! { "_id": 2_i64, "m": 3, "c": true },
        ];
        assert_eq!(
            sampled_fields(&docs),
            [
                ("_id".to_string(), "mixed(long|objectId)".to_string()),
                ("b".into(), "int".into()),
                ("m".into(), "mixed(int|string)".into()),
                ("c".into(), "bool".into()),
            ]
        );
        assert_eq!(sampled_fields(&[]), [("_id".into(), "objectId".into())]);
    }

    #[test]
    fn indexes_primary_unique_partial_and_sorted() {
        let specs = vec![
            doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
            doc! { "v": 2, "key": { "sku": 1, "at": -1 }, "name": "sku_1_at_-1", "unique": true },
            doc! { "v": 2, "key": { "body": "text" }, "name": "body_text",
            "partialFilterExpression": { "a": 1 }, "hidden": true },
        ];
        let (pk, others) = split_indexes(specs);
        let pk = pk.unwrap();
        assert_eq!(
            (pk.name.as_str(), pk.primary, pk.unique),
            ("_id_", true, true)
        );
        assert_eq!(pk.columns, ["_id"]);
        let names: Vec<&str> = others.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["body_text", "sku_1_at_-1"]);
        let text = &others[0];
        assert_eq!(text.method.as_deref(), Some("text"));
        assert!(text.partial && !text.visible && !text.unique);
        let sku = &others[1];
        assert!(sku.unique && !sku.primary && !sku.partial);
        assert_eq!(sku.columns, ["sku", "at"]);
        assert_eq!(
            sku.parts.iter().map(|p| p.descending).collect::<Vec<_>>(),
            [false, true]
        );
        assert_eq!(
            sku.definition.as_deref(),
            Some(r#"{"key":{"sku":1,"at":-1},"name":"sku_1_at_-1","unique":true}"#)
        );
        // No `_id_` index (a clustered collection): `_id` is still the primary key.
        let (pk, others) = split_indexes(Vec::new());
        assert_eq!(pk.unwrap().columns, ["_id"]);
        assert!(others.is_empty());
    }
}
