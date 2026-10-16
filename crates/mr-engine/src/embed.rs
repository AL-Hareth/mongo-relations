//! Computing denormalized embed snapshots (shared by writes and sync).

use std::collections::HashMap;

use bson::{Bson, Document, doc};
use mr_core::catalog::Embed;
use mr_core::compile;
use mr_core::path::{key_repr, key_values};

use crate::error::Result;
use crate::runner::Runner;

/// Fetch snapshots of the targets with the given keys, indexed by key.
pub(crate) async fn fetch(
    r: &mut dyn Runner,
    e: &Embed,
    keys: Vec<Bson>,
) -> Result<HashMap<String, Document>> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let (proj, _) = compile::projection(
        &Some(e.fields.clone()),
        &[],
        std::slice::from_ref(&e.target_field),
    );
    let mut pipeline = vec![doc! { "$match": { e.target_field.as_str(): { "$in": keys } } }];
    if let Some(p) = proj {
        pipeline.push(doc! { "$project": p });
    }
    let mut out = HashMap::new();
    for t in r.aggregate(&e.target, pipeline).await? {
        if let Some(k) = t.get(&e.target_field) {
            out.insert(key_repr(k), e.snapshot(&t));
        }
    }
    Ok(out)
}

/// The snapshot an owner document should hold given fetched snapshots
/// (`None` means the snapshot field should be absent).
pub(crate) fn value_for(
    e: &Embed,
    owner: &Document,
    snaps: &HashMap<String, Document>,
) -> Option<Bson> {
    let keys = key_values(owner, &e.field);
    if e.array {
        Some(Bson::Array(
            keys.iter()
                .filter_map(|k| snaps.get(&key_repr(k)))
                .cloned()
                .map(Bson::Document)
                .collect(),
        ))
    } else {
        keys.first()
            .and_then(|k| snaps.get(&key_repr(k)))
            .cloned()
            .map(Bson::Document)
    }
}
