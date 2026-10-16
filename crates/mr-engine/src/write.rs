//! Writes with referential integrity (D4, D8).
//!
//! A write runs in a multi-document transaction when it involves integrity
//! work (strict FK checks, `on_delete` actions, `ON UPDATE RESTRICT`) or when
//! several operations must be atomic. Transient errors (write conflicts, for
//! instance from intent locks) retry the whole transaction with backoff.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bson::{Bson, Document, doc, oid::ObjectId};
use futures::future::BoxFuture;
use mongodb::ClientSession;
use mongodb::options::{ReadConcern, TransactionOptions, WriteConcern};
use mr_core::catalog::{Constraint, Embed};
use mr_core::compile::{self, filter_doc, filter_stages};
use mr_core::path::{key_repr, key_values, remove_path, set_path};
use mr_core::plan::{Planner, Pred};
use mr_core::query::Expr;
use mr_core::schema::{ID_FIELD, OnDelete};
use mr_core::validate::{touched_paths, touches, validate_document, validate_update};
use mr_core::write::{WriteOp, WriteResult};
use mr_core::{Catalog, Error as CoreError};
use rand::Rng;

use crate::error::{Error, Result};
use crate::read::resolve_pred;
use crate::runner::{InSession, Runner};
use crate::{Engine, embed, locks};

impl Engine {
    /// Execute write operations atomically and return combined counts.
    pub async fn write(&self, mut ops: Vec<WriteOp>) -> Result<WriteResult> {
        if ops.is_empty() {
            return Ok(WriteResult::default());
        }
        let catalog = self.catalog();
        for op in &mut ops {
            prepare(&catalog, op)?;
        }
        let txn = ops.len() > 1 || ops.iter().any(|op| needs_txn(&catalog, op));
        if txn && !self.inner.topology.supports_transactions {
            return Err(Error::Unsupported(
                "this write needs a transaction (strict integrity or a multi-op batch), which \
                 requires a replica set or sharded cluster"
                    .into(),
            ));
        }
        let mut session = self.inner.client.start_session().await?;
        if !txn {
            return Tx::new(self, &catalog, &mut session, false)
                .apply(&ops)
                .await;
        }
        let opts = TransactionOptions::builder()
            .read_concern(ReadConcern::snapshot())
            .write_concern(WriteConcern::majority())
            .build();
        let max = self.inner.config.max_txn_attempts.max(1);
        let mut attempt = 0;
        loop {
            attempt += 1;
            session
                .start_transaction()
                .with_options(opts.clone())
                .await?;
            let res = Tx::new(self, &catalog, &mut session, true)
                .apply(&ops)
                .await;
            let err = match res {
                Ok(result) => match commit(&mut session).await {
                    Ok(()) => return Ok(result),
                    Err(e) => e,
                },
                Err(e) => {
                    let _ = session.abort_transaction().await;
                    e
                }
            };
            if !err.is_transient() {
                return Err(err);
            }
            if attempt >= max {
                return Err(Error::TooManyRetries {
                    attempts: attempt,
                    last: Box::new(err),
                });
            }
            tracing::debug!(attempt, error = %err, "retrying transaction");
            backoff(attempt).await;
        }
    }

    /// Convenience wrapper for a single insert.
    pub async fn insert(&self, collection: &str, documents: Vec<Document>) -> Result<WriteResult> {
        self.write(vec![WriteOp::Insert {
            collection: collection.into(),
            documents,
        }])
        .await
    }

    /// Convenience wrapper for a single multi-document update.
    pub async fn update(
        &self,
        collection: &str,
        filter: Option<Expr>,
        update: Document,
    ) -> Result<WriteResult> {
        self.write(vec![WriteOp::Update {
            collection: collection.into(),
            filter,
            update,
            multi: true,
        }])
        .await
    }

    /// Convenience wrapper for a single multi-document delete.
    pub async fn delete(&self, collection: &str, filter: Option<Expr>) -> Result<WriteResult> {
        self.write(vec![WriteOp::Delete {
            collection: collection.into(),
            filter,
            multi: true,
        }])
        .await
    }
}

async fn commit(session: &mut ClientSession) -> Result<()> {
    for _ in 0..5 {
        match session.commit_transaction().await.map_err(Error::from) {
            Ok(()) => return Ok(()),
            Err(e) if e.is_unknown_commit() => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(session.commit_transaction().await?)
}

async fn backoff(attempt: u32) {
    let base = 5u64.saturating_mul(1 << attempt.min(7)).min(500);
    let jitter = rand::rng().random_range(0..=base);
    tokio::time::sleep(Duration::from_millis(base / 2 + jitter / 2)).await;
}

/// Validate and normalize an operation before execution. Runs once, so
/// generated `_id`s stay stable across transaction retries.
fn prepare(catalog: &Catalog, op: &mut WriteOp) -> Result<()> {
    let info = catalog.collection(op.collection())?;
    match op {
        WriteOp::Insert {
            documents,
            collection,
        } => {
            if documents.is_empty() {
                return Err(
                    CoreError::InvalidWrite("insert needs at least one document".into()).into(),
                );
            }
            let embeds: Vec<&Embed> = catalog.embeds_on(collection).collect();
            for d in documents.iter_mut() {
                validate_document(&info.def, d).map_err(CoreError::Validation)?;
                if !d.contains_key(ID_FIELD) {
                    d.insert(ID_FIELD, ObjectId::new());
                }
                // Snapshots are maintained by the layer.
                for e in &embeds {
                    remove_path(d, &e.into);
                }
            }
        }
        WriteOp::Update {
            update, collection, ..
        } => {
            validate_update(&info.def, update).map_err(CoreError::Validation)?;
            let touched = touched_paths(update);
            for e in catalog.embeds_on(collection) {
                if touches(&touched, &e.into) {
                    return Err(CoreError::InvalidWrite(format!(
                        "`{}` is maintained by relation `{}` and cannot be written",
                        e.into, e.relation
                    ))
                    .into());
                }
            }
        }
        WriteOp::Delete { .. } => {}
    }
    Ok(())
}

fn needs_txn(catalog: &Catalog, op: &WriteOp) -> bool {
    match op {
        WriteOp::Insert {
            collection,
            documents,
        } => catalog.outbound(collection).any(|c| {
            c.is_strict()
                && documents
                    .iter()
                    .any(|d| !key_values(d, &c.field).is_empty())
        }),
        WriteOp::Update {
            collection, update, ..
        } => {
            let touched = touched_paths(update);
            catalog
                .outbound(collection)
                .any(|c| c.is_strict() && touches(&touched, &c.field))
                || catalog.inbound(collection).any(|c| {
                    c.is_strict()
                        && c.target_field != ID_FIELD
                        && touches(&touched, &c.target_field)
                })
        }
        WriteOp::Delete { collection, .. } => catalog
            .inbound(collection)
            .any(|c| c.is_strict() && c.on_delete != OnDelete::NoAction),
    }
}

fn distinct_keys<'a>(docs: impl IntoIterator<Item = &'a Document>, field: &str) -> Vec<Bson> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    for d in docs {
        for k in key_values(d, field) {
            if seen.insert(key_repr(&k)) {
                out.push(k);
            }
        }
    }
    out
}

fn ids_of(docs: &[Document]) -> Vec<Bson> {
    docs.iter()
        .filter_map(|d| d.get(ID_FIELD).cloned())
        .collect()
}

/// One write execution (possibly one transaction attempt).
struct Tx<'a> {
    engine: &'a Engine,
    catalog: &'a Catalog,
    s: &'a mut ClientSession,
    in_txn: bool,
    result: WriteResult,
    /// `(collection, _id)` already deleted in this execution (cascade cycle guard).
    deleted: HashSet<(String, String)>,
}

impl<'a> Tx<'a> {
    fn new(
        engine: &'a Engine,
        catalog: &'a Catalog,
        s: &'a mut ClientSession,
        in_txn: bool,
    ) -> Self {
        Self {
            engine,
            catalog,
            s,
            in_txn,
            result: WriteResult::default(),
            deleted: HashSet::new(),
        }
    }

    fn coll(&self, name: &str) -> mongodb::Collection<Document> {
        self.engine.inner.db.collection(name)
    }

    async fn aggregate(&mut self, coll: &str, pipeline: Vec<Document>) -> Result<Vec<Document>> {
        InSession {
            db: &self.engine.inner.db,
            session: &mut *self.s,
        }
        .aggregate(coll, pipeline)
        .await
    }

    async fn exists(&mut self, coll: &str, filter: Document) -> Result<bool> {
        let found = self
            .aggregate(
                coll,
                vec![
                    doc! { "$match": filter },
                    doc! { "$limit": 1 },
                    doc! { "$project": { "_id": 1 } },
                ],
            )
            .await?;
        Ok(!found.is_empty())
    }

    async fn lock(&mut self, coll: &str, field: &str, keys: &[Bson]) -> Result<()> {
        if self.in_txn && !keys.is_empty() {
            locks::lock(&self.engine.inner.meta, self.s, coll, field, keys).await?;
        }
        Ok(())
    }

    async fn resolve(&mut self, collection: &str, filter: Option<&Expr>) -> Result<Pred> {
        let pred = Planner::new(
            self.catalog,
            self.engine.stats(),
            &self.engine.inner.config.planner,
        )
        .plan_filter(collection, filter)?;
        let mut runner = InSession {
            db: &self.engine.inner.db,
            session: &mut *self.s,
        };
        resolve_pred(&mut runner, pred).await
    }

    async fn apply(mut self, ops: &[WriteOp]) -> Result<WriteResult> {
        for op in ops {
            match op {
                WriteOp::Insert {
                    collection,
                    documents,
                } => {
                    self.insert(collection, documents.clone()).await?;
                }
                WriteOp::Update {
                    collection,
                    filter,
                    update,
                    multi,
                } => {
                    self.update(collection, filter.as_ref(), update, *multi)
                        .await?;
                }
                WriteOp::Delete {
                    collection,
                    filter,
                    multi,
                } => {
                    let pred = self.resolve(collection, filter.as_ref()).await?;
                    let mut stages = filter_stages(&pred);
                    if !multi {
                        stages.push(doc! { "$limit": 1 });
                    }
                    let n = self.delete_matching(collection.clone(), stages, 0).await?;
                    self.result.deleted += n;
                }
            }
        }
        Ok(self.result)
    }

    /// Verify that every key exists in `c.target.c.target_field`.
    /// `own` are documents being inserted in the same statement, which may
    /// satisfy self-references.
    async fn verify_exist(
        &mut self,
        c: &Constraint,
        keys: Vec<Bson>,
        own: &[Document],
    ) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        self.lock(&c.target, &c.target_field, &keys).await?;
        let tf = c.target_field.as_str();
        let found = self
            .aggregate(
                &c.target,
                vec![
                    doc! { "$match": { tf: { "$in": keys.clone() } } },
                    doc! { "$group": { "_id": format!("${tf}") } },
                ],
            )
            .await?;
        let mut present: HashSet<String> = found
            .iter()
            .filter_map(|d| d.get("_id"))
            .map(key_repr)
            .collect();
        if c.target == c.owner {
            present.extend(
                own.iter()
                    .flat_map(|d| key_values(d, tf))
                    .map(|k| key_repr(&k)),
            );
        }
        let missing: Vec<Bson> = keys
            .into_iter()
            .filter(|k| !present.contains(&key_repr(k)))
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Error::ForeignKey {
                collection: c.owner.clone(),
                field: c.field.clone(),
                target: c.target.clone(),
                missing,
            })
        }
    }

    async fn snapshots(&mut self, e: &Embed, keys: Vec<Bson>) -> Result<HashMap<String, Document>> {
        let mut runner = InSession {
            db: &self.engine.inner.db,
            session: &mut *self.s,
        };
        embed::fetch(&mut runner, e, keys).await
    }

    async fn insert(&mut self, collection: &str, mut docs: Vec<Document>) -> Result<()> {
        let constraints: Vec<Constraint> = self
            .catalog
            .outbound(collection)
            .filter(|c| c.is_strict())
            .cloned()
            .collect();
        for c in &constraints {
            let keys = distinct_keys(docs.iter(), &c.field);
            self.verify_exist(c, keys, &docs).await?;
        }
        let embeds: Vec<Embed> = self.catalog.embeds_on(collection).cloned().collect();
        for e in &embeds {
            let keys = distinct_keys(docs.iter(), &e.field);
            let snaps = self.snapshots(e, keys).await?;
            for d in docs.iter_mut() {
                match embed::value_for(e, d, &snaps) {
                    Some(v) => set_path(d, &e.into, v),
                    None => {
                        remove_path(d, &e.into);
                    }
                }
            }
        }
        let ids = ids_of(&docs);
        self.coll(collection)
            .insert_many(docs)
            .session(&mut *self.s)
            .await?;
        self.result.inserted_ids.extend(ids);
        Ok(())
    }

    async fn update(
        &mut self,
        collection: &str,
        filter: Option<&Expr>,
        update: &Document,
        multi: bool,
    ) -> Result<()> {
        let touched = touched_paths(update);
        let fk: Vec<Constraint> = self
            .catalog
            .outbound(collection)
            .filter(|c| c.is_strict() && touches(&touched, &c.field))
            .cloned()
            .collect();
        let embeds: Vec<Embed> = self
            .catalog
            .embeds_on(collection)
            .filter(|e| touches(&touched, &e.field))
            .cloned()
            .collect();
        let restrict: Vec<Constraint> = self
            .catalog
            .inbound(collection)
            .filter(|c| {
                c.is_strict() && c.target_field != ID_FIELD && touches(&touched, &c.target_field)
            })
            .cloned()
            .collect();
        let pred = self.resolve(collection, filter).await?;

        if fk.is_empty() && embeds.is_empty() && restrict.is_empty() {
            if let Some(f) = filter_doc(&pred) {
                let coll = self.coll(collection);
                let res = if multi {
                    coll.update_many(f, update.clone())
                        .session(&mut *self.s)
                        .await?
                } else {
                    coll.update_one(f, update.clone())
                        .session(&mut *self.s)
                        .await?
                };
                self.result.matched += res.matched_count;
                self.result.modified += res.modified_count;
                return Ok(());
            }
        }

        // Identify the documents first so effects can be checked afterwards.
        let mut fields: Vec<String> = restrict.iter().map(|c| c.target_field.clone()).collect();
        let mut stages = filter_stages(&pred);
        if !multi {
            stages.push(doc! { "$limit": 1 });
        }
        let (proj, _) = compile::projection(&Some(fields.clone()), &[], &[]);
        stages.push(doc! { "$project": proj.expect("projection") });
        let before = self.aggregate(collection, stages).await?;
        if before.is_empty() {
            return Ok(());
        }
        let ids = ids_of(&before);
        let res = self
            .coll(collection)
            .update_many(doc! { "_id": { "$in": ids.clone() } }, update.clone())
            .session(&mut *self.s)
            .await?;
        self.result.matched += res.matched_count;
        self.result.modified += res.modified_count;
        if fk.is_empty() && embeds.is_empty() && restrict.is_empty() {
            return Ok(());
        }

        fields.extend(fk.iter().map(|c| c.field.clone()));
        fields.extend(embeds.iter().map(|e| e.field.clone()));
        let (proj, _) = compile::projection(&Some(fields), &[], &[]);
        let after = self
            .aggregate(
                collection,
                vec![
                    doc! { "$match": { "_id": { "$in": ids } } },
                    doc! { "$project": proj.expect("projection") },
                ],
            )
            .await?;

        // ON UPDATE RESTRICT: a referenced key may not disappear.
        for c in &restrict {
            let new: HashSet<String> = distinct_keys(after.iter(), &c.target_field)
                .iter()
                .map(key_repr)
                .collect();
            let removed: Vec<Bson> = distinct_keys(before.iter(), &c.target_field)
                .into_iter()
                .filter(|k| !new.contains(&key_repr(k)))
                .collect();
            if removed.is_empty() {
                continue;
            }
            self.lock(collection, &c.target_field, &removed).await?;
            if self
                .exists(&c.owner, doc! { c.field.as_str(): { "$in": removed } })
                .await?
            {
                return Err(Error::Restrict {
                    action: "update",
                    collection: collection.to_string(),
                    referenced_by: format!("{}.{}", c.owner, c.field),
                });
            }
        }
        for c in &fk {
            let keys = distinct_keys(after.iter(), &c.field);
            self.verify_exist(c, keys, &[]).await?;
        }
        for e in &embeds {
            self.refresh_embed(collection, e, &after).await?;
        }
        Ok(())
    }

    /// Recompute an embed's snapshots for owner documents (grouped so that
    /// documents sharing a snapshot are updated together).
    async fn refresh_embed(
        &mut self,
        collection: &str,
        e: &Embed,
        owners: &[Document],
    ) -> Result<()> {
        let keys = distinct_keys(owners.iter(), &e.field);
        let snaps = self.snapshots(e, keys).await?;
        let mut groups: HashMap<String, (Option<Bson>, Vec<Bson>)> = HashMap::new();
        for o in owners {
            let Some(id) = o.get(ID_FIELD) else { continue };
            let v = embed::value_for(e, o, &snaps);
            let key = v.as_ref().map_or_else(
                || "-".to_string(),
                |v| v.clone().into_canonical_extjson().to_string(),
            );
            groups
                .entry(key)
                .or_insert_with(|| (v, vec![]))
                .1
                .push(id.clone());
        }
        for (_, (value, ids)) in groups {
            let update = match value {
                Some(v) => doc! { "$set": { e.into.as_str(): v } },
                None => doc! { "$unset": { e.into.as_str(): "" } },
            };
            self.coll(collection)
                .update_many(doc! { "_id": { "$in": ids } }, update)
                .session(&mut *self.s)
                .await?;
        }
        Ok(())
    }

    /// Delete documents matched by `stages` on `collection`, applying
    /// `on_delete` for every strict constraint referencing them.
    fn delete_matching(
        &mut self,
        collection: String,
        stages: Vec<Document>,
        depth: usize,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let max = self.engine.inner.config.max_cascade_depth;
            if depth > max {
                return Err(Error::CascadeTooDeep(max));
            }
            let inbound: Vec<Constraint> = self
                .catalog
                .inbound(&collection)
                .filter(|c| c.is_strict() && c.on_delete != OnDelete::NoAction)
                .cloned()
                .collect();

            if inbound.is_empty() && depth == 0 {
                if let [doc] = stages.as_slice() {
                    if let Ok(f) = doc.get_document("$match") {
                        let res = self
                            .coll(&collection)
                            .delete_many(f.clone())
                            .session(&mut *self.s)
                            .await?;
                        return Ok(res.deleted_count);
                    }
                }
                if stages.is_empty() {
                    let res = self
                        .coll(&collection)
                        .delete_many(doc! {})
                        .session(&mut *self.s)
                        .await?;
                    return Ok(res.deleted_count);
                }
            }

            let fields: Vec<String> = inbound.iter().map(|c| c.target_field.clone()).collect();
            let (proj, _) = compile::projection(&Some(fields), &[], &[]);
            let mut pipeline = stages;
            pipeline.push(doc! { "$project": proj.expect("projection") });
            let mut docs = self.aggregate(&collection, pipeline).await?;
            docs.retain(|d| {
                d.get(ID_FIELD)
                    .is_some_and(|id| self.deleted.insert((collection.clone(), key_repr(id))))
            });
            if docs.is_empty() {
                return Ok(0);
            }
            let ids = ids_of(&docs);

            // Lock and check restrictions before acting.
            let mut actions = vec![];
            for c in &inbound {
                let keys = distinct_keys(docs.iter(), &c.target_field);
                if keys.is_empty() {
                    continue;
                }
                self.lock(&collection, &c.target_field, &keys).await?;
                if c.on_delete == OnDelete::Restrict {
                    let mut f = doc! { c.field.as_str(): { "$in": keys.clone() } };
                    if c.owner == collection {
                        f.insert(ID_FIELD, doc! { "$nin": ids.clone() });
                    }
                    if self.exists(&c.owner, f).await? {
                        return Err(Error::Restrict {
                            action: "delete",
                            collection: collection.clone(),
                            referenced_by: format!("{}.{}", c.owner, c.field),
                        });
                    }
                } else {
                    actions.push((c, keys));
                }
            }

            for (c, keys) in actions {
                let f = doc! { c.field.as_str(): { "$in": keys.clone() } };
                match c.on_delete {
                    OnDelete::Cascade => {
                        let n = self
                            .delete_matching(c.owner.clone(), vec![doc! { "$match": f }], depth + 1)
                            .await?;
                        self.result.add_cascade(&c.owner, n, 0);
                    }
                    OnDelete::SetNull => {
                        let update = if c.array {
                            doc! { "$pull": { c.field.as_str(): { "$in": keys } } }
                        } else {
                            doc! { "$set": { c.field.as_str(): Bson::Null } }
                        };
                        let res = self
                            .coll(&c.owner)
                            .update_many(f, update)
                            .session(&mut *self.s)
                            .await?;
                        self.result.add_cascade(&c.owner, 0, res.modified_count);
                    }
                    OnDelete::Restrict | OnDelete::NoAction => {}
                }
            }

            let res = self
                .coll(&collection)
                .delete_many(doc! { "_id": { "$in": ids } })
                .session(&mut *self.s)
                .await?;
            Ok(res.deleted_count)
        })
    }
}
