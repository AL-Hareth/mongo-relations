//! Asynchronous embed synchronization via change streams (D10).
//!
//! One watcher per relevant collection:
//! * as an embed **target**: when copied fields change, rewrite every copy;
//!   when the target is deleted, remove its copies (found by `<into>._id`);
//! * as an embed **owner**: when the FK changes (including writes that bypass
//!   the layer), recompute that document's snapshot.
//!
//! Resume tokens are persisted in `_mr_sync_state` so processing continues
//! where it stopped. All operations are idempotent, so replaying a few
//! events after a restart is harmless.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bson::{Bson, Document, doc};
use futures::StreamExt;
use mongodb::change_stream::event::{ChangeStreamEvent, OperationType, ResumeToken};
use mongodb::options::{FullDocumentType, UpdateOptions};
use mr_core::Catalog;
use mr_core::catalog::Embed;
use mr_core::path::{get_path, paths_overlap};
use mr_core::schema::ID_FIELD;
use tokio::sync::watch;

use crate::error::Result;
use crate::runner::Plain;
use crate::{Engine, embed};

pub(crate) const SYNC_COLL: &str = "_mr_sync_state";
const TOKEN_SAVE_INTERVAL: Duration = Duration::from_secs(1);

/// Supervisor: (re)starts watchers whenever the catalog version changes.
pub(crate) async fn run(engine: Engine, mut shutdown: watch::Receiver<bool>) {
    loop {
        let catalog = engine.catalog();
        let version = catalog.version();
        let watched: BTreeSet<String> = catalog
            .embeds()
            .iter()
            .flat_map(|e| [e.target.clone(), e.owner.clone()])
            .collect();
        let (stop_tx, stop_rx) = watch::channel(false);
        let tasks: Vec<_> = watched
            .into_iter()
            .map(|c| {
                tokio::spawn(watch_collection(
                    engine.clone(),
                    c,
                    catalog.clone(),
                    stop_rx.clone(),
                ))
            })
            .collect();
        if !tasks.is_empty() {
            tracing::info!(version, watchers = tasks.len(), "embed sync started");
        }
        loop {
            if crate::sleep_or_shutdown(Duration::from_secs(2), &mut shutdown).await {
                let _ = stop_tx.send(true);
                for t in tasks {
                    let _ = t.await;
                }
                return;
            }
            if engine.catalog().version() != version {
                break;
            }
        }
        let _ = stop_tx.send(true);
        for t in tasks {
            let _ = t.await;
        }
    }
}

async fn watch_collection(
    engine: Engine,
    coll: String,
    catalog: Arc<Catalog>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        match watch_once(&engine, &coll, &catalog, &mut stop).await {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!(collection = %coll, error = %e, "embed sync watcher failed; restarting")
            }
        }
        if crate::sleep_or_shutdown(Duration::from_secs(1), &mut stop).await {
            return;
        }
    }
}

async fn load_token(engine: &Engine, coll: &str) -> Option<ResumeToken> {
    let state = engine.inner.meta.collection::<Document>(SYNC_COLL);
    let doc = state.find_one(doc! { "_id": coll }).await.ok().flatten()?;
    bson::deserialize_from_bson(doc.get("token")?.clone()).ok()
}

async fn save_token(engine: &Engine, coll: &str, token: Option<&ResumeToken>) -> Result<()> {
    let state = engine.inner.meta.collection::<Document>(SYNC_COLL);
    match token {
        Some(t) => {
            let t = bson::serialize_to_bson(t)?;
            state
                .update_one(
                    doc! { "_id": coll },
                    doc! { "$set": { "token": t, "at": bson::DateTime::now() } },
                )
                .upsert(true)
                .await?;
        }
        None => {
            state.delete_one(doc! { "_id": coll }).await?;
        }
    }
    Ok(())
}

async fn watch_once(
    engine: &Engine,
    coll: &str,
    catalog: &Catalog,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    let token = load_token(engine, coll).await;
    let filter = doc! { "$match": { "operationType": { "$in": ["insert", "update", "replace", "delete", "invalidate"] } } };
    let collection = engine.inner.db.collection::<Document>(coll);
    let open = |token: Option<ResumeToken>| {
        let mut w = collection
            .watch()
            .pipeline([filter.clone()])
            .full_document(FullDocumentType::UpdateLookup);
        if let Some(t) = token {
            w = w.resume_after(t);
        }
        w
    };
    let mut stream = match open(token.clone()).await {
        Ok(s) => s,
        // History lost / fatal: the token cannot be used any more.
        Err(e)
            if token.is_some()
                && matches!(
                    crate::error::mongo_code(&e),
                    Some(286) | Some(280) | Some(260)
                ) =>
        {
            tracing::warn!(collection = %coll, "resume token expired; embeds may be stale until rebuilt");
            save_token(engine, coll, None).await?;
            open(None).await?
        }
        Err(e) => return Err(e.into()),
    };

    // Track tokens from events: `ChangeStream::resume_token` must not be
    // called after a pending `next()` was cancelled by `select!`.
    let mut token: Option<ResumeToken> = None;
    let mut last_save = Instant::now();
    let mut dirty = false;
    loop {
        tokio::select! {
            ev = stream.next() => {
                let Some(ev) = ev else { break };
                let ev = ev?;
                if ev.operation_type == OperationType::Invalidate {
                    save_token(engine, coll, None).await?;
                    return Err(crate::Error::Internal(format!("change stream on `{coll}` invalidated")));
                }
                let id = ev.id.clone();
                handle(engine, catalog, coll, ev).await?;
                token = Some(id);
                dirty = true;
                if last_save.elapsed() >= TOKEN_SAVE_INTERVAL {
                    save_token(engine, coll, token.as_ref()).await?;
                    last_save = Instant::now();
                    dirty = false;
                }
            }
            _ = stop.changed() => break,
        }
    }
    if dirty {
        save_token(engine, coll, token.as_ref()).await?;
    }
    Ok(())
}

fn changed_fields(ev: &ChangeStreamEvent<Document>) -> Option<Vec<String>> {
    let d = ev.update_description.as_ref()?;
    let mut out: Vec<String> = d.updated_fields.keys().cloned().collect();
    out.extend(d.removed_fields.iter().cloned());
    if let Some(t) = &d.truncated_arrays {
        out.extend(t.iter().map(|t| t.field.clone()));
    }
    Some(out)
}

fn touches(changed: &Option<Vec<String>>, field: &str) -> bool {
    changed
        .as_ref()
        .is_none_or(|c| c.iter().any(|p| paths_overlap(p, field)))
}

async fn handle(
    engine: &Engine,
    catalog: &Catalog,
    coll: &str,
    ev: ChangeStreamEvent<Document>,
) -> Result<()> {
    match ev.operation_type {
        OperationType::Insert | OperationType::Update | OperationType::Replace => {
            // Deleted since: the delete event will follow.
            let Some(full) = ev.full_document.as_ref() else {
                return Ok(());
            };
            let changed = if ev.operation_type == OperationType::Update {
                changed_fields(&ev)
            } else {
                None
            };
            for e in catalog.embeds_from(coll) {
                let relevant = touches(&changed, &e.target_field)
                    || e.fields.iter().any(|f| touches(&changed, f));
                // Inserts only matter if something already references the new key.
                if relevant {
                    refresh_target(engine, e, full).await?;
                }
            }
            for e in catalog.embeds_on(coll) {
                if touches(&changed, &e.field) {
                    refresh_owner(engine, coll, e, full).await?;
                }
            }
        }
        OperationType::Delete => {
            let Some(id) = ev
                .document_key
                .as_ref()
                .and_then(|k| k.get(ID_FIELD))
                .cloned()
            else {
                return Ok(());
            };
            for e in catalog.embeds_from(coll) {
                remove_copies(engine, e, &id).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Rewrite every copy of `target` after it changed.
async fn refresh_target(engine: &Engine, e: &Embed, target: &Document) -> Result<()> {
    let owners = engine.inner.db.collection::<Document>(&e.owner);
    let (Some(key), Some(id)) = (target.get(&e.target_field), target.get(ID_FIELD)) else {
        return Ok(());
    };
    let snap = e.snapshot(target);
    let into = e.into.as_str();
    let id_path = format!("{into}.{ID_FIELD}");
    if e.array {
        let opts = UpdateOptions::builder()
            .array_filters(vec![doc! { "x._id": id.clone() }])
            .build();
        owners
            .update_many(
                doc! { e.field.as_str(): key.clone(), id_path.as_str(): id.clone() },
                doc! { "$set": { format!("{into}.$[x]"): snap.clone() } },
            )
            .with_options(opts)
            .await?;
        owners
            .update_many(
                doc! { e.field.as_str(): key.clone(), id_path.as_str(): { "$ne": id.clone() } },
                doc! { "$push": { into: snap } },
            )
            .await?;
    } else {
        owners
            .update_many(
                doc! { e.field.as_str(): key.clone() },
                doc! { "$set": { into: snap } },
            )
            .await?;
    }
    // The target's key itself changed (only possible for non-`_id` keys):
    // copies held by owners still pointing at the old key are stale.
    if e.target_field != ID_FIELD {
        let stale = doc! { id_path.as_str(): id.clone(), e.field.as_str(): { "$ne": key.clone() } };
        let update = if e.array {
            doc! { "$pull": { into: { ID_FIELD: id.clone() } } }
        } else {
            doc! { "$unset": { into: "" } }
        };
        owners.update_many(stale, update).await?;
    }
    Ok(())
}

/// Recompute the snapshot on one owner document after its FK changed.
async fn refresh_owner(engine: &Engine, coll: &str, e: &Embed, owner: &Document) -> Result<()> {
    let keys = mr_core::path::key_values(owner, &e.field);
    let snaps = embed::fetch(
        &mut Plain {
            db: &engine.inner.db,
        },
        e,
        keys,
    )
    .await?;
    let want = embed::value_for(e, owner, &snaps);
    let have = get_path(owner, &e.into);
    if want.as_ref() == have {
        return Ok(());
    }
    let Some(id) = owner.get(ID_FIELD) else {
        return Ok(());
    };
    let update = match want {
        Some(v) => doc! { "$set": { e.into.as_str(): v } },
        None => doc! { "$unset": { e.into.as_str(): "" } },
    };
    engine
        .inner
        .db
        .collection::<Document>(coll)
        .update_one(doc! { ID_FIELD: id.clone() }, update)
        .await?;
    Ok(())
}

/// Remove copies of a deleted target.
async fn remove_copies(engine: &Engine, e: &Embed, id: &Bson) -> Result<()> {
    let owners = engine.inner.db.collection::<Document>(&e.owner);
    let into = e.into.as_str();
    let filter = doc! { format!("{into}.{ID_FIELD}"): id.clone() };
    let update = if e.array {
        doc! { "$pull": { into: { ID_FIELD: id.clone() } } }
    } else {
        doc! { "$unset": { into: "" } }
    };
    owners.update_many(filter, update).await?;
    Ok(())
}

impl Engine {
    /// Recompute every embed snapshot from scratch (e.g. after the sync
    /// worker was offline longer than the oplog window). Returns the number
    /// of owner documents updated.
    pub async fn rebuild_embeds(&self) -> Result<u64> {
        use futures::TryStreamExt;
        let catalog = self.catalog();
        let mut updated = 0;
        for e in catalog.embeds() {
            let coll = self.inner.db.collection::<Document>(&e.owner);
            let mut cursor = coll
                .find(doc! {})
                .projection(doc! { e.field.as_str(): 1, e.into.as_str(): 1 })
                .await?;
            let mut batch = Vec::with_capacity(500);
            loop {
                let next = cursor.try_next().await?;
                let done = next.is_none();
                if let Some(d) = next {
                    batch.push(d);
                }
                if batch.len() == 500 || (done && !batch.is_empty()) {
                    let keys: Vec<Bson> = {
                        let mut seen = std::collections::HashSet::new();
                        batch
                            .iter()
                            .flat_map(|d| mr_core::path::key_values(d, &e.field))
                            .filter(|k| seen.insert(mr_core::path::key_repr(k)))
                            .collect()
                    };
                    let snaps = embed::fetch(&mut Plain { db: &self.inner.db }, e, keys).await?;
                    for d in batch.drain(..) {
                        let want = embed::value_for(e, &d, &snaps);
                        if want.as_ref() == get_path(&d, &e.into) {
                            continue;
                        }
                        let Some(id) = d.get(ID_FIELD) else { continue };
                        let update = match want {
                            Some(v) => doc! { "$set": { e.into.as_str(): v } },
                            None => doc! { "$unset": { e.into.as_str(): "" } },
                        };
                        coll.update_one(doc! { ID_FIELD: id.clone() }, update)
                            .await?;
                        updated += 1;
                    }
                }
                if done {
                    break;
                }
            }
        }
        Ok(updated)
    }
}
