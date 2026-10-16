//! Write IR.

use bson::{Bson, Document};
use serde::{Deserialize, Serialize};

use crate::query::Expr;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WriteOp {
    Insert {
        collection: String,
        #[serde(with = "crate::ejson::document_vec")]
        documents: Vec<Document>,
    },
    Update {
        collection: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Expr>,
        /// Operator update (`$set`, `$inc`, `$push`, ...).
        #[serde(with = "crate::ejson::document")]
        update: Document,
        /// Update every match (default) or only the first.
        #[serde(default = "yes")]
        multi: bool,
    },
    Delete {
        collection: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Expr>,
        #[serde(default = "yes")]
        multi: bool,
    },
}

fn yes() -> bool {
    true
}

impl WriteOp {
    pub fn collection(&self) -> &str {
        match self {
            WriteOp::Insert { collection, .. }
            | WriteOp::Update { collection, .. }
            | WriteOp::Delete { collection, .. } => collection,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WriteResult {
    #[serde(
        with = "crate::ejson::bson_vec",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub inserted_ids: Vec<Bson>,
    pub matched: u64,
    pub modified: u64,
    pub deleted: u64,
    /// Documents deleted or modified by `on_delete` actions, per collection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cascaded: Vec<CascadeCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CascadeCount {
    pub collection: String,
    pub deleted: u64,
    pub nulled: u64,
}

impl WriteResult {
    pub fn add_cascade(&mut self, collection: &str, deleted: u64, nulled: u64) {
        if deleted == 0 && nulled == 0 {
            return;
        }
        match self
            .cascaded
            .iter_mut()
            .find(|c| c.collection == collection)
        {
            Some(c) => {
                c.deleted += deleted;
                c.nulled += nulled;
            }
            None => self.cascaded.push(CascadeCount {
                collection: collection.to_string(),
                deleted,
                nulled,
            }),
        }
    }

    pub fn merge(&mut self, other: WriteResult) {
        self.inserted_ids.extend(other.inserted_ids);
        self.matched += other.matched;
        self.modified += other.modified;
        self.deleted += other.deleted;
        for c in other.cascaded {
            self.add_cascade(&c.collection, c.deleted, c.nulled);
        }
    }
}
