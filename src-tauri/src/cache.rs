//! Per-connection caches: `NodeCache` (browse results by tree path) and `SchemaCache`.

use dnt_core::meta::ObjectKind;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

/// Path of a tree node below its connection, one typed segment per level:
/// `db:<name>`, `schema:<name>`, `folder:<ObjectKind>`, `obj:<ObjectKind>:<name>`, then an
/// optional `detail` / `ddl`. The connection itself is not part of the key (one cache per
/// connection).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct NodeKey(pub Vec<String>);

impl NodeKey {
    /// True when `prefix`'s segments are the first segments of `self` (an empty prefix
    /// matches every key).
    pub fn starts_with(&self, prefix: &NodeKey) -> bool {
        self.0.starts_with(&prefix.0)
    }
}

/// Browse results stored as JSON, so one map holds every result type.
#[derive(Debug, Default)]
pub struct NodeCache {
    entries: Mutex<HashMap<NodeKey, serde_json::Value>>,
}

impl NodeCache {
    fn lock(&self) -> MutexGuard<'_, HashMap<NodeKey, serde_json::Value>> {
        // The map stays consistent even if a holder panicked: every operation is one call.
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, key: &NodeKey) -> Option<serde_json::Value> {
        self.lock().get(key).cloned()
    }

    pub fn insert(&self, key: NodeKey, value: serde_json::Value) {
        self.lock().insert(key, value);
    }

    /// Drops every entry whose key starts with `prefix`.
    pub fn invalidate_prefix(&self, prefix: &NodeKey) {
        self.lock().retain(|k, _| !k.starts_with(prefix));
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// Table / view names and their column names of the connection's default database
/// (PostgreSQL: of the schemas in `current_schemas(false)`), loaded in the background.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SchemaCache {
    pub database: String,
    pub tables: Vec<SchemaCacheTable>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SchemaCacheTable {
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
    pub columns: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(segments: &[&str]) -> NodeKey {
        NodeKey(segments.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn invalidate_prefix_matches_whole_segments() {
        let cache = NodeCache::default();
        cache.insert(key(&["db:shop", "folder:Table"]), json!(1));
        cache.insert(key(&["db:shop", "obj:Table:orders", "ddl"]), json!(2));
        cache.insert(key(&["db:shopping", "folder:Table"]), json!(3));
        cache.insert(key(&["db:other"]), json!(4));

        cache.invalidate_prefix(&key(&["db:shop"]));
        assert_eq!(cache.len(), 2);
        // `db:shop` is not a prefix of `db:shopping`: segments compare whole.
        assert_eq!(
            cache.get(&key(&["db:shopping", "folder:Table"])),
            Some(json!(3))
        );
        assert!(cache.get(&key(&["db:shop", "folder:Table"])).is_none());

        cache.invalidate_prefix(&key(&[]));
        assert!(cache.is_empty());
    }

    #[test]
    fn deeper_prefix_keeps_siblings() {
        let cache = NodeCache::default();
        cache.insert(key(&["db:a", "obj:Table:t", "detail"]), json!(1));
        cache.insert(key(&["db:a", "obj:Table:t", "ddl"]), json!(2));
        cache.insert(key(&["db:a", "obj:Table:u", "ddl"]), json!(3));
        cache.invalidate_prefix(&key(&["db:a", "obj:Table:t"]));
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.get(&key(&["db:a", "obj:Table:u", "ddl"])),
            Some(json!(3))
        );
    }
}
