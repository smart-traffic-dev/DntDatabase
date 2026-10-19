//! Per-connection caches: `NodeCache` (browse results by tree path) and `SchemaCache`.

use dnt_core::meta::{ColumnBrief, ObjectKind};
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
///
/// Every invalidation bumps an epoch. A caller reads [`NodeCache::epoch`] before fetching
/// and stores with [`NodeCache::insert_if_epoch`], so a result fetched across a refresh
/// (and possibly stale) is not cached.
#[derive(Debug, Default)]
pub struct NodeCache {
    inner: Mutex<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    map: HashMap<NodeKey, serde_json::Value>,
    epoch: u64,
}

impl NodeCache {
    fn lock(&self) -> MutexGuard<'_, Entries> {
        // The map stays consistent even if a holder panicked: every operation is one call.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, key: &NodeKey) -> Option<serde_json::Value> {
        self.lock().map.get(key).cloned()
    }

    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Stores `value` unless an invalidation happened since `epoch` was read; returns
    /// whether it was stored.
    pub fn insert_if_epoch(&self, key: NodeKey, value: serde_json::Value, epoch: u64) -> bool {
        let mut entries = self.lock();
        if entries.epoch != epoch {
            return false;
        }
        entries.map.insert(key, value);
        true
    }

    /// Drops every entry whose key starts with `prefix`.
    pub fn invalidate_prefix(&self, prefix: &NodeKey) {
        let mut entries = self.lock();
        entries.epoch += 1;
        entries.map.retain(|k, _| !k.starts_with(prefix));
    }

    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().map.is_empty()
    }
}

/// Table / view names and their columns (with types) of one database (PostgreSQL: of the
/// schemas in `current_schemas(false)` of that database's metadata session). One per
/// database, loaded lazily (see `SessionManager::schema_cache_for`).
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
    pub columns: Vec<ColumnBrief>,
}

/// A table or view offered by autocomplete after `db.` / `schema.`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct CompletionObject {
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(segments: &[&str]) -> NodeKey {
        NodeKey(segments.iter().map(|s| s.to_string()).collect())
    }

    fn put(cache: &NodeCache, key: NodeKey, value: serde_json::Value) {
        assert!(cache.insert_if_epoch(key, value, cache.epoch()));
    }

    #[test]
    fn result_fetched_across_an_invalidation_is_not_stored() {
        let cache = NodeCache::default();
        let before = cache.epoch();
        // A refresh of any prefix lands while the fetch runs.
        cache.invalidate_prefix(&key(&["db:other"]));
        assert!(!cache.insert_if_epoch(key(&["db:a", "folder:Table"]), json!(1), before));
        assert!(cache.is_empty());
        // A fetch started after the refresh is stored.
        put(&cache, key(&["db:a", "folder:Table"]), json!(2));
        assert_eq!(cache.get(&key(&["db:a", "folder:Table"])), Some(json!(2)));
    }

    #[test]
    fn invalidate_prefix_matches_whole_segments() {
        let cache = NodeCache::default();
        put(&cache, key(&["db:shop", "folder:Table"]), json!(1));
        put(
            &cache,
            key(&["db:shop", "obj:Table:orders", "ddl"]),
            json!(2),
        );
        put(&cache, key(&["db:shopping", "folder:Table"]), json!(3));
        put(&cache, key(&["db:other"]), json!(4));

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
        put(&cache, key(&["db:a", "obj:Table:t", "detail"]), json!(1));
        put(&cache, key(&["db:a", "obj:Table:t", "ddl"]), json!(2));
        put(&cache, key(&["db:a", "obj:Table:u", "ddl"]), json!(3));
        cache.invalidate_prefix(&key(&["db:a", "obj:Table:t"]));
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.get(&key(&["db:a", "obj:Table:u", "ddl"])),
            Some(json!(3))
        );
    }
}
