//! Execution contexts: run commands either standalone or inside a session
//! (and thus inside its transaction, if one is active).

use bson::Document;
use futures::TryStreamExt;
use mongodb::{ClientSession, Database};

use crate::error::Result;

#[async_trait::async_trait]
pub trait Runner: Send {
    async fn aggregate(&mut self, coll: &str, pipeline: Vec<Document>) -> Result<Vec<Document>>;
}

/// Runs without a session.
pub struct Plain<'a> {
    pub db: &'a Database,
}

#[async_trait::async_trait]
impl Runner for Plain<'_> {
    async fn aggregate(&mut self, coll: &str, pipeline: Vec<Document>) -> Result<Vec<Document>> {
        let cursor = self
            .db
            .collection::<Document>(coll)
            .aggregate(pipeline)
            .await?;
        Ok(cursor.try_collect().await?)
    }
}

/// Runs within a session.
pub struct InSession<'a> {
    pub db: &'a Database,
    pub session: &'a mut ClientSession,
}

#[async_trait::async_trait]
impl Runner for InSession<'_> {
    async fn aggregate(&mut self, coll: &str, pipeline: Vec<Document>) -> Result<Vec<Document>> {
        let mut cursor = self
            .db
            .collection::<Document>(coll)
            .aggregate(pipeline)
            .session(&mut *self.session)
            .await?;
        Ok(cursor.stream(self.session).try_collect().await?)
    }
}
