//! Versioned schema storage (D2) and schema application.
//!
//! Each applied schema is stored as `{ _id: <version>, schema, applied_at }`
//! in `_mr_schema`. The unique `_id` makes concurrent applies from several
//! server instances safe: only one can insert version `n + 1`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use bson::{Document, doc};
use futures::{StreamExt, TryStreamExt};
use mongodb::IndexModel;
use mongodb::options::IndexOptions;
use mr_core::Catalog;
use mr_core::schema::Schema;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::error::{Error, Result};
use crate::{Engine, locks};

pub(crate) const SCHEMA_COLL: &str = "_mr_schema";
/// Prefix of indexes owned by the layer; others are never touched.
const INDEX_PREFIX: &str = "mr_";

#[derive(Debug, Serialize, Deserialize)]
struct SchemaDoc {
    #[serde(rename = "_id")]
    version: i64,
    schema: Schema,
    applied_at: bson::DateTime,
}

/// Summary of a stored schema version.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SchemaVersion {
    pub version: u64,
    pub applied_at: bson::DateTime,
}

/// What `apply_schema` did (or would do, on a dry run).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ApplyReport {
    pub version: u64,
    /// A new version was stored (false when the schema was unchanged).
    pub changed: bool,
    pub dry_run: bool,
    pub created_collections: Vec<String>,
    pub created_indexes: Vec<String>,
    pub dropped_indexes: Vec<String>,
    pub validators_updated: Vec<String>,
}

impl Engine {
    /// Create system collections and their indexes.
    pub(crate) async fn bootstrap(&self) -> Result<()> {
        let ttl = IndexOptions::builder()
            .name("mr_ttl".to_string())
            .expire_after(self.inner.config.lock_ttl)
            .build();
        self.inner
            .meta
            .collection::<Document>(locks::LOCKS_COLL)
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "at": 1 })
                    .options(ttl)
                    .build(),
            )
            .await?;
        Ok(())
    }

    async fn latest_schema(&self) -> Result<Option<SchemaDoc>> {
        Ok(self
            .inner
            .meta
            .collection::<SchemaDoc>(SCHEMA_COLL)
            .find_one(doc! {})
            .sort(doc! { "_id": -1 })
            .await?)
    }

    /// Load the newest stored schema and install it if it is newer than the
    /// current catalog. Returns the current catalog.
    pub async fn reload_catalog(&self) -> Result<Arc<Catalog>> {
        if let Some(latest) = self.latest_schema().await? {
            let version = latest.version as u64;
            if version > self.catalog().version() {
                let catalog = Catalog::new(latest.schema, version)?;
                self.inner.catalog.store(Arc::new(catalog));
                tracing::info!(version, "catalog loaded");
            }
        }
        Ok(self.catalog())
    }

    /// Stored schema versions, newest first.
    pub async fn schema_history(&self, limit: i64) -> Result<Vec<SchemaVersion>> {
        let docs: Vec<Document> = self
            .inner
            .meta
            .collection::<Document>(SCHEMA_COLL)
            .find(doc! {})
            .projection(doc! { "_id": 1, "applied_at": 1 })
            .sort(doc! { "_id": -1 })
            .limit(limit)
            .await?
            .try_collect()
            .await?;
        Ok(docs
            .into_iter()
            .filter_map(|d| {
                Some(SchemaVersion {
                    version: d.get_i64("_id").ok()? as u64,
                    applied_at: *d.get_datetime("applied_at").ok()?,
                })
            })
            .collect())
    }

    /// Validate `schema`, create collections, indexes and `$jsonSchema`
    /// validators, then store it as a new version (declarative, like `db push`).
    pub async fn apply_schema(&self, schema: Schema, dry_run: bool) -> Result<ApplyReport> {
        let _guard = self.inner.apply_lock.lock().await;
        let current = self.latest_schema().await?;
        let current_version = current.as_ref().map_or(0, |c| c.version as u64);
        let changed = current.as_ref().is_none_or(|c| c.schema != schema);
        let version = if changed {
            current_version + 1
        } else {
            current_version
        };
        let catalog = Catalog::new(schema.clone(), version)?;

        let mut report = ApplyReport {
            version,
            changed,
            dry_run,
            ..Default::default()
        };
        let db = &self.inner.db;
        let existing: HashSet<String> = db.list_collection_names().await?.into_iter().collect();

        for info in catalog.collections() {
            let name = &info.def.name;
            let validator = doc! { "$jsonSchema": catalog.json_schema(name)? };
            if !existing.contains(name) {
                report.created_collections.push(name.clone());
                if !dry_run {
                    db.create_collection(name)
                        .validator(validator.clone())
                        .validation_level(mongodb::options::ValidationLevel::Moderate)
                        .await?;
                }
            } else if !dry_run {
                db.run_command(doc! {
                    "collMod": name,
                    "validator": validator,
                    "validationLevel": "moderate",
                })
                .await?;
            }
            report.validators_updated.push(name.clone());

            let coll = db.collection::<Document>(name);
            let wanted = catalog.indexes(name)?;
            let have: Vec<String> = if existing.contains(name) {
                coll.list_index_names().await?
            } else {
                vec![]
            };
            for idx in &wanted {
                if have.contains(&idx.name) {
                    continue;
                }
                report.created_indexes.push(format!("{name}.{}", idx.name));
                if !dry_run {
                    let opts = IndexOptions::builder()
                        .name(idx.name.clone())
                        .unique(idx.unique)
                        .build();
                    coll.create_index(
                        IndexModel::builder()
                            .keys(idx.keys.clone())
                            .options(opts)
                            .build(),
                    )
                    .await?;
                }
            }
            for name_have in have.iter().filter(|n| n.starts_with(INDEX_PREFIX)) {
                if !wanted.iter().any(|w| &w.name == name_have) {
                    report.dropped_indexes.push(format!("{name}.{name_have}"));
                    if !dry_run {
                        coll.drop_index(name_have).await?;
                    }
                }
            }
        }

        // Collections that left the schema keep their data but lose the
        // layer's validator and indexes.
        if let Some(cur) = &current {
            for old in &cur.schema.collections {
                if schema.collection(&old.name).is_some() || !existing.contains(&old.name) {
                    continue;
                }
                report.validators_updated.push(old.name.clone());
                let coll = db.collection::<Document>(&old.name);
                let have = coll.list_index_names().await?;
                for n in have.iter().filter(|n| n.starts_with(INDEX_PREFIX)) {
                    report.dropped_indexes.push(format!("{}.{n}", old.name));
                }
                if !dry_run {
                    db.run_command(doc! { "collMod": &old.name, "validator": {} })
                        .await?;
                    for n in have.iter().filter(|n| n.starts_with(INDEX_PREFIX)) {
                        coll.drop_index(n).await?;
                    }
                }
            }
        }

        if dry_run || !changed {
            if !dry_run && self.catalog().version() < version {
                self.inner.catalog.store(Arc::new(catalog));
            }
            return Ok(report);
        }

        let res = self
            .inner
            .meta
            .collection::<SchemaDoc>(SCHEMA_COLL)
            .insert_one(SchemaDoc {
                version: version as i64,
                schema,
                applied_at: bson::DateTime::now(),
            })
            .await;
        if let Err(e) = res {
            if crate::error::mongo_code(&e) == Some(11000) {
                return Err(Error::SchemaConflict(format!(
                    "schema version {version} was applied concurrently; reload and retry"
                )));
            }
            return Err(e.into());
        }
        self.inner.catalog.store(Arc::new(catalog));
        tracing::info!(version, "schema applied");
        Ok(report)
    }
}

/// Follow `_mr_schema` so that schema changes applied through any server
/// instance reach this one. Falls back to polling when change streams fail.
pub(crate) async fn follow_catalog(engine: Engine, mut rx: watch::Receiver<bool>) {
    let coll = engine.inner.meta.collection::<Document>(SCHEMA_COLL);
    loop {
        if let Err(e) = engine.reload_catalog().await {
            tracing::warn!(error = %e, "catalog reload failed");
        }
        if engine.inner.topology.supports_transactions {
            match coll
                .watch()
                .pipeline([doc! { "$match": { "operationType": "insert" } }])
                .await
            {
                Ok(mut stream) => loop {
                    tokio::select! {
                        ev = stream.next() => match ev {
                            Some(Ok(_)) => {
                                if let Err(e) = engine.reload_catalog().await {
                                    tracing::warn!(error = %e, "catalog reload failed");
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(error = %e, "catalog change stream failed");
                                break;
                            }
                            None => break,
                        },
                        _ = rx.changed() => return,
                    }
                },
                Err(e) => tracing::warn!(error = %e, "cannot watch catalog"),
            }
        }
        if crate::sleep_or_shutdown(Duration::from_secs(5), &mut rx).await {
            return;
        }
    }
}
