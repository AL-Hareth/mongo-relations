//! Query IR (D3, D14). Clients build these with the typed builder; the SQL
//! front-end planned for later will produce the same structures.

use bson::{Bson, Document};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Query {
    pub collection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Expr>,
    /// Fields to return (dotted paths). `None` returns whole documents.
    /// `_id` is always returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<Include>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl Query {
    pub fn new(collection: impl Into<String>) -> Self {
        Self {
            collection: collection.into(),
            ..Default::default()
        }
    }
}

/// Load a related collection into each result document (a join).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Include {
    pub relation: String,
    /// Output field; defaults to the relation name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Filter on related documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Expr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortKey>,
    /// Per-parent limit (to-many relations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Inner-join semantics: drop parents without a matching related document.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<Include>,
    /// Force a join strategy instead of letting the planner choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<JoinStrategy>,
}

impl Include {
    pub fn new(relation: impl Into<String>) -> Self {
        Self {
            relation: relation.into(),
            ..Default::default()
        }
    }

    pub fn alias(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.relation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinStrategy {
    /// A `$lookup` stage inside the aggregation pipeline.
    Lookup,
    /// A separate batched `$in` query per include level.
    Batch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortKey {
    pub field: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub desc: bool,
}

impl SortKey {
    pub fn asc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            desc: false,
        }
    }
    pub fn desc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            desc: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl CmpOp {
    pub fn mongo(self) -> &'static str {
        match self {
            CmpOp::Eq => "$eq",
            CmpOp::Ne => "$ne",
            CmpOp::Gt => "$gt",
            CmpOp::Gte => "$gte",
            CmpOp::Lt => "$lt",
            CmpOp::Lte => "$lte",
        }
    }
}

/// Filter expression. Paths may cross to-one relations (`author.country`);
/// to-many relations are filtered with [`Expr::Any`] / [`Expr::All`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Cmp {
        path: String,
        op: CmpOp,
        #[serde(with = "crate::ejson::bson")]
        value: Bson,
    },
    In {
        path: String,
        #[serde(with = "crate::ejson::bson_vec")]
        values: Vec<Bson>,
    },
    Nin {
        path: String,
        #[serde(with = "crate::ejson::bson_vec")]
        values: Vec<Bson>,
    },
    Exists {
        path: String,
        #[serde(default = "yes")]
        exists: bool,
    },
    /// The path is null or missing. On a relation path: no related document.
    IsNull {
        path: String,
    },
    Regex {
        path: String,
        pattern: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        options: String,
    },
    /// At least one related document matches (SQL `EXISTS`).
    Any {
        relation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Box<Expr>>,
    },
    /// Every related document matches (vacuously true without related documents).
    All {
        relation: String,
        filter: Box<Expr>,
    },
    /// A raw MongoDB filter on the current collection (escape hatch).
    Raw(#[serde(with = "crate::ejson::document")] Document),
}

fn yes() -> bool {
    true
}

/// Shorthand constructors.
impl Expr {
    pub fn cmp(path: impl Into<String>, op: CmpOp, value: impl Into<Bson>) -> Self {
        Expr::Cmp {
            path: path.into(),
            op,
            value: value.into(),
        }
    }
    pub fn eq(path: impl Into<String>, value: impl Into<Bson>) -> Self {
        Self::cmp(path, CmpOp::Eq, value)
    }
    pub fn ne(path: impl Into<String>, value: impl Into<Bson>) -> Self {
        Self::cmp(path, CmpOp::Ne, value)
    }
    pub fn gt(path: impl Into<String>, value: impl Into<Bson>) -> Self {
        Self::cmp(path, CmpOp::Gt, value)
    }
    pub fn gte(path: impl Into<String>, value: impl Into<Bson>) -> Self {
        Self::cmp(path, CmpOp::Gte, value)
    }
    pub fn lt(path: impl Into<String>, value: impl Into<Bson>) -> Self {
        Self::cmp(path, CmpOp::Lt, value)
    }
    pub fn lte(path: impl Into<String>, value: impl Into<Bson>) -> Self {
        Self::cmp(path, CmpOp::Lte, value)
    }
    pub fn is_in<V: Into<Bson>>(
        path: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        Expr::In {
            path: path.into(),
            values: values.into_iter().map(Into::into).collect(),
        }
    }
    pub fn any(relation: impl Into<String>, filter: Option<Expr>) -> Self {
        Expr::Any {
            relation: relation.into(),
            filter: filter.map(Box::new),
        }
    }
    pub fn all(relation: impl Into<String>, filter: Expr) -> Self {
        Expr::All {
            relation: relation.into(),
            filter: Box::new(filter),
        }
    }
    pub fn and(self, other: Expr) -> Self {
        match self {
            Expr::And(mut v) => {
                v.push(other);
                Expr::And(v)
            }
            e => Expr::And(vec![e, other]),
        }
    }
    pub fn or(self, other: Expr) -> Self {
        match self {
            Expr::Or(mut v) => {
                v.push(other);
                Expr::Or(v)
            }
            e => Expr::Or(vec![e, other]),
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn not(self) -> Self {
        Expr::Not(Box::new(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;

    #[test]
    fn json_shape() {
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let q = Query {
            collection: "posts".into(),
            filter: Some(Expr::eq("author.country", "US").and(Expr::eq("author_id", oid))),
            include: vec![Include {
                limit: Some(3),
                ..Include::new("comments")
            }],
            ..Query::new("posts")
        };
        let json = serde_json::to_value(&q).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "collection": "posts",
                "filter": { "and": [
                    { "cmp": { "path": "author.country", "op": "eq", "value": "US" } },
                    { "cmp": { "path": "author_id", "op": "eq",
                               "value": { "$oid": "65a1b2c3d4e5f60718293a4b" } } }
                ]},
                "include": [ { "relation": "comments", "limit": 3 } ]
            })
        );
        let back: Query = serde_json::from_value(json).unwrap();
        assert_eq!(back, q);
    }
}
