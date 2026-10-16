//! Collection statistics for the planner's cost model.

use std::collections::HashMap;
use std::sync::RwLock;

use bson::{Document, doc};
use mr_core::plan::{CollectionStats, StatsProvider};
use tokio::sync::watch;

use crate::Engine;

#[derive(Default)]
pub(crate) struct StatsCache {
    map: RwLock<HashMap<String, CollectionStats>>,
}

impl StatsProvider for StatsCache {
    fn stats(&self, collection: &str) -> CollectionStats {
        self.map
            .read()
            .expect("stats lock")
            .get(collection)
            .copied()
            .unwrap_or_default()
    }
}

impl Engine {
    pub(crate) fn stats(&self) -> &StatsCache {
        &self.inner.stats
    }

    /// Refresh statistics for every catalog collection. Failures are logged
    /// and leave previous values in place.
    pub async fn refresh_stats(&self) {
        let catalog = self.catalog();
        let sharded = self.sharded_collections().await;
        let mut fresh = HashMap::new();
        for info in catalog.collections() {
            let name = &info.def.name;
            match self
                .inner
                .db
                .collection::<Document>(name)
                .estimated_document_count()
                .await
            {
                Ok(count) => {
                    fresh.insert(
                        name.clone(),
                        CollectionStats {
                            count,
                            sharded: sharded.contains(name),
                        },
                    );
                }
                Err(e) => tracing::debug!(collection = %name, error = %e, "stats refresh failed"),
            }
        }
        self.inner
            .stats
            .map
            .write()
            .expect("stats lock")
            .extend(fresh);
    }

    /// Names of sharded collections in the data database (empty unless the
    /// deployment is a sharded cluster and we may read `config.collections`).
    async fn sharded_collections(&self) -> Vec<String> {
        let prefix = format!("{}.", self.inner.db.name());
        let coll = self
            .inner
            .client
            .database("config")
            .collection::<Document>("collections");
        let filter = doc! { "_id": { "$regex": format!("^{}", regex_escape(&prefix)) } };
        let Ok(mut cursor) = coll.find(filter).projection(doc! { "_id": 1 }).await else {
            return vec![];
        };
        let mut out = vec![];
        while let Ok(true) = cursor.advance().await {
            if let Ok(d) = cursor.deserialize_current() {
                if let Ok(id) = d.get_str("_id") {
                    out.push(id[prefix.len()..].to_string());
                }
            }
        }
        out
    }
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(crate) async fn refresh_loop(engine: Engine, mut rx: watch::Receiver<bool>) {
    let every = engine.inner.config.stats_refresh;
    while !crate::sleep_or_shutdown(every, &mut rx).await {
        engine.refresh_stats().await;
    }
}
