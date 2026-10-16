//! Server configuration (TOML file + environment overrides).

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use mr_engine::EngineConfig;
use serde::Deserialize;

use crate::auth::Role;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mongodb: MongoConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub engine: EngineTuning,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MongoConfig {
    pub uri: String,
    pub database: String,
    #[serde(default)]
    pub metadata_database: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    /// Maximum HTTP request body, in bytes.
    pub max_body_bytes: usize,
    /// Target maximum size of one gRPC query response message, in bytes.
    pub max_grpc_message_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "0.0.0.0:50051".parse().unwrap(),
            http_addr: "0.0.0.0:8080".parse().unwrap(),
            max_body_bytes: 16 * 1024 * 1024,
            max_grpc_message_bytes: 3 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    /// Accept unauthenticated requests as admin. For local development only.
    pub disabled: bool,
    pub keys: Vec<ApiKey>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKey {
    pub name: String,
    /// Hex SHA-256 of the key (`mongo-relations hash-key <key>`).
    pub sha256: String,
    pub role: Role,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EngineTuning {
    pub semi_join_max_keys: usize,
    pub chunk_size: usize,
    pub max_include_depth: usize,
    pub max_txn_attempts: u32,
    pub lock_ttl_secs: u64,
    pub max_cascade_depth: usize,
    pub stats_refresh_secs: u64,
}

impl Default for EngineTuning {
    fn default() -> Self {
        let e = EngineConfig::new("");
        Self {
            semi_join_max_keys: e.planner.semi_join_max_keys,
            chunk_size: e.planner.chunk_size,
            max_include_depth: e.planner.max_include_depth,
            max_txn_attempts: e.max_txn_attempts,
            lock_ttl_secs: e.lock_ttl.as_secs(),
            max_cascade_depth: e.max_cascade_depth,
            stats_refresh_secs: e.stats_refresh.as_secs(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut cfg: Config = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("reading {}", p.display()))?;
                toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?
            }
            None => Config {
                mongodb: MongoConfig {
                    uri: String::new(),
                    database: String::new(),
                    metadata_database: None,
                },
                server: ServerConfig::default(),
                auth: AuthConfig::default(),
                engine: EngineTuning::default(),
            },
        };
        cfg.apply_env(|k| std::env::var(k).ok())?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Environment overrides: `MR_MONGODB_URI`, `MR_DATABASE`,
    /// `MR_METADATA_DATABASE`, `MR_GRPC_ADDR`, `MR_HTTP_ADDR`, `MR_AUTH_DISABLED`.
    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) -> anyhow::Result<()> {
        if let Some(v) = get("MR_MONGODB_URI") {
            self.mongodb.uri = v;
        }
        if let Some(v) = get("MR_DATABASE") {
            self.mongodb.database = v;
        }
        if let Some(v) = get("MR_METADATA_DATABASE") {
            self.mongodb.metadata_database = Some(v);
        }
        if let Some(v) = get("MR_GRPC_ADDR") {
            self.server.grpc_addr = v.parse().context("MR_GRPC_ADDR")?;
        }
        if let Some(v) = get("MR_HTTP_ADDR") {
            self.server.http_addr = v.parse().context("MR_HTTP_ADDR")?;
        }
        if let Some(v) = get("MR_AUTH_DISABLED") {
            self.auth.disabled = matches!(v.as_str(), "1" | "true" | "yes");
        }
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.mongodb.uri.is_empty() {
            bail!("mongodb.uri is required (or set MR_MONGODB_URI)");
        }
        if self.mongodb.database.is_empty() {
            bail!("mongodb.database is required (or set MR_DATABASE)");
        }
        if !self.auth.disabled && self.auth.keys.is_empty() {
            bail!(
                "no API keys configured; add [[auth.keys]] or set auth.disabled = true for development"
            );
        }
        for k in &self.auth.keys {
            if k.sha256.len() != 64 || hex::decode(&k.sha256).is_err() {
                bail!("auth key `{}`: sha256 must be 64 hex characters", k.name);
            }
        }
        Ok(())
    }

    pub fn engine_config(&self) -> EngineConfig {
        let mut e = EngineConfig::new(&self.mongodb.database);
        e.metadata_database = self.mongodb.metadata_database.clone();
        e.planner.semi_join_max_keys = self.engine.semi_join_max_keys;
        e.planner.chunk_size = self.engine.chunk_size.max(1);
        e.planner.max_include_depth = self.engine.max_include_depth;
        e.max_txn_attempts = self.engine.max_txn_attempts;
        e.lock_ttl = Duration::from_secs(self.engine.lock_ttl_secs);
        e.max_cascade_depth = self.engine.max_cascade_depth;
        e.stats_refresh = Duration::from_secs(self.engine.stats_refresh_secs.max(1));
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_and_env_overrides() {
        let text = include_str!("../../../config.example.toml");
        let mut cfg: Config = toml::from_str(text).unwrap();
        cfg.apply_env(|k| (k == "MR_DATABASE").then(|| "other".to_string()))
            .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.mongodb.database, "other");
        assert_eq!(cfg.auth.keys.len(), 1);
    }

    #[test]
    fn requires_keys_unless_disabled() {
        let mut cfg: Config =
            toml::from_str("[mongodb]\nuri = \"mongodb://x\"\ndatabase = \"d\"\n").unwrap();
        assert!(cfg.validate().is_err());
        cfg.auth.disabled = true;
        cfg.validate().unwrap();
    }
}
