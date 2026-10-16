//! MongoDB execution engine for mongo-relations.
//!
//! [`Engine`] owns the MongoDB client and the current [`Catalog`], and
//! exposes reads ([`Engine::query`]), integrity-enforcing writes
//! ([`Engine::write`]) and schema management ([`Engine::apply_schema`]).
//! [`Engine::spawn_background`] starts the catalog follower, the statistics
//! refresher and the embed-sync worker.

mod catalog_store;
mod embed;
mod error;
mod locks;
mod read;
mod runner;
mod stats;
mod sync;
mod write;

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use bson::doc;
use mongodb::{Client, Database};
use mr_core::Catalog;
use mr_core::plan::PlannerConfig;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use catalog_store::{ApplyReport, SchemaVersion};
pub use error::{Error, ErrorKind, Result};
pub use mr_core;
pub use read::DocStream;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Database holding user collections.
    pub database: String,
    /// Database holding `_mr_*` system collections (defaults to `database`).
    pub metadata_database: Option<String>,
    pub planner: PlannerConfig,
    /// Attempts for a transaction that hits transient errors (write conflicts).
    pub max_txn_attempts: u32,
    /// How long intent-lock documents are kept before the TTL index removes them.
    pub lock_ttl: Duration,
    /// Maximum depth of `on_delete: cascade` chains.
    pub max_cascade_depth: usize,
    /// How often collection statistics are refreshed.
    pub stats_refresh: Duration,
}

impl EngineConfig {
    pub fn new(database: impl Into<String>) -> Self {
        Self {
            database: database.into(),
            metadata_database: None,
            planner: PlannerConfig::default(),
            max_txn_attempts: 10,
            lock_ttl: Duration::from_secs(3600),
            max_cascade_depth: 32,
            stats_refresh: Duration::from_secs(60),
        }
    }
}

/// What the connected deployment supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topology {
    pub server_version: String,
    /// Replica set or sharded cluster: transactions and change streams work.
    pub supports_transactions: bool,
}

/// Cheaply cloneable handle to the engine.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    db: Database,
    meta: Database,
    catalog: ArcSwap<Catalog>,
    stats: stats::StatsCache,
    config: EngineConfig,
    topology: Topology,
    apply_lock: tokio::sync::Mutex<()>,
}

impl Engine {
    /// Connect to MongoDB, prepare system collections and load the catalog.
    pub async fn connect(uri: &str, config: EngineConfig) -> Result<Self> {
        let client = Client::with_uri_str(uri).await?;
        Self::with_client(client, config).await
    }

    pub async fn with_client(client: Client, config: EngineConfig) -> Result<Self> {
        let db = client.database(&config.database);
        let meta = client.database(
            config
                .metadata_database
                .as_deref()
                .unwrap_or(&config.database),
        );
        let topology = detect_topology(&client).await?;
        if !topology.supports_transactions {
            tracing::warn!(
                "MongoDB is not a replica set or sharded cluster: strict integrity, batches and \
                 embed sync are unavailable"
            );
        }
        let engine = Self {
            inner: Arc::new(Inner {
                client,
                db,
                meta,
                catalog: ArcSwap::from_pointee(Catalog::empty()),
                stats: stats::StatsCache::default(),
                config,
                topology,
                apply_lock: tokio::sync::Mutex::new(()),
            }),
        };
        engine.bootstrap().await?;
        engine.reload_catalog().await?;
        engine.refresh_stats().await;
        Ok(engine)
    }

    /// The current catalog snapshot.
    pub fn catalog(&self) -> Arc<Catalog> {
        self.inner.catalog.load_full()
    }

    pub fn database(&self) -> &Database {
        &self.inner.db
    }

    pub fn client(&self) -> &Client {
        &self.inner.client
    }

    pub fn config(&self) -> &EngineConfig {
        &self.inner.config
    }

    pub fn topology(&self) -> &Topology {
        &self.inner.topology
    }

    /// Check that MongoDB answers.
    pub async fn ping(&self) -> Result<()> {
        self.inner.db.run_command(doc! { "ping": 1 }).await?;
        Ok(())
    }

    /// Start background tasks: catalog follower, stats refresher and (when
    /// supported) the embed-sync worker. They stop when the returned handle
    /// is shut down or dropped.
    pub fn spawn_background(&self) -> Background {
        let (tx, rx) = watch::channel(false);
        let mut tasks = vec![
            tokio::spawn(catalog_store::follow_catalog(self.clone(), rx.clone())),
            tokio::spawn(stats::refresh_loop(self.clone(), rx.clone())),
        ];
        if self.inner.topology.supports_transactions {
            tasks.push(tokio::spawn(sync::run(self.clone(), rx)));
        }
        Background {
            shutdown: tx,
            tasks,
        }
    }
}

/// Handle to background tasks.
pub struct Background {
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Background {
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        for t in std::mem::take(&mut self.tasks) {
            let _ = t.await;
        }
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

async fn detect_topology(client: &Client) -> Result<Topology> {
    let admin = client.database("admin");
    let hello = admin.run_command(doc! { "hello": 1 }).await?;
    let info = admin.run_command(doc! { "buildInfo": 1 }).await?;
    let server_version = info.get_str("version").unwrap_or("unknown").to_string();
    let major: u32 = server_version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    if major < 5 {
        return Err(Error::Internal(format!(
            "MongoDB {server_version} is not supported; 5.0 or newer is required"
        )));
    }
    let supports_transactions =
        hello.contains_key("setName") || hello.get_str("msg").is_ok_and(|m| m == "isdbgrid");
    Ok(Topology {
        server_version,
        supports_transactions,
    })
}

/// Sleep for `d` or until shutdown is signalled; returns `true` on shutdown.
pub(crate) async fn sleep_or_shutdown(d: Duration, rx: &mut watch::Receiver<bool>) -> bool {
    if *rx.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(d) => *rx.borrow(),
        r = rx.changed() => r.is_err() || *rx.borrow(),
    }
}
