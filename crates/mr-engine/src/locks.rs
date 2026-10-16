//! Intent locks (D17).
//!
//! Snapshot isolation alone permits write skew between "insert a row that
//! references K" and "delete K": each transaction reads a snapshot in which
//! the other has not happened. Both therefore write the same small document
//! `_mr_locks { _id: "<collection>|<field>|<key>" }` inside their
//! transactions; MongoDB reports a write conflict to one of them, which then
//! retries and sees the other's committed effects. A TTL index removes old
//! lock documents. User documents are never touched.

use std::collections::HashSet;

use bson::{Bson, Document, doc};
use futures::TryStreamExt;
use mongodb::{ClientSession, Database};
use mr_core::path::key_repr;

use crate::error::Result;

pub(crate) const LOCKS_COLL: &str = "_mr_locks";
const CHUNK: usize = 1000;

fn lock_id(collection: &str, field: &str, key: &Bson) -> String {
    format!("{collection}|{field}|{}", key_repr(key))
}

/// Take intent locks on `collection.field = key` for each key, inside the
/// session's active transaction.
pub(crate) async fn lock(
    meta: &Database,
    s: &mut ClientSession,
    collection: &str,
    field: &str,
    keys: &[Bson],
) -> Result<()> {
    let mut seen = HashSet::new();
    let ids: Vec<String> = keys
        .iter()
        .map(|k| lock_id(collection, field, k))
        .filter(|id| seen.insert(id.clone()))
        .collect();
    let coll = meta.collection::<Document>(LOCKS_COLL);
    let now = bson::DateTime::now();
    for chunk in ids.chunks(CHUNK) {
        // `$inc` guarantees a real write (a same-millisecond `$set` alone
        // could be a no-op that produces no write conflict).
        let res = coll
            .update_many(
                doc! { "_id": { "$in": chunk } },
                doc! { "$set": { "at": now }, "$inc": { "n": 1 } },
            )
            .session(&mut *s)
            .await?;
        if res.matched_count as usize == chunk.len() {
            continue;
        }
        let mut cursor = coll
            .find(doc! { "_id": { "$in": chunk } })
            .projection(doc! { "_id": 1 })
            .session(&mut *s)
            .await?;
        let existing: HashSet<String> = cursor
            .stream(&mut *s)
            .try_collect::<Vec<Document>>()
            .await?
            .into_iter()
            .filter_map(|d| d.get_str("_id").ok().map(str::to_string))
            .collect();
        let missing: Vec<Document> = chunk
            .iter()
            .filter(|id| !existing.contains(*id))
            .map(|id| doc! { "_id": id, "at": now, "n": 1 })
            .collect();
        if !missing.is_empty() {
            coll.insert_many(missing).session(&mut *s).await?;
        }
    }
    Ok(())
}
