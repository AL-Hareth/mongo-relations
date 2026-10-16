//! Query execution.
//!
//! 1. Plan the query (pure, in `mr-core`).
//! 2. Resolve semi-joins by pre-querying related keys; predicates with too
//!    many keys fall back to `$lookup`.
//! 3. Stream the root pipeline in chunks; for each chunk run every batch
//!    include as one `$in` query per include node and stitch the results in.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;

use bson::{Bson, Document, doc};
use futures::future::BoxFuture;
use futures::{Stream, TryStreamExt};
use mr_core::compile;
use mr_core::path::{key_repr, key_values, remove_path};
use mr_core::plan::{IncludePlan, Planner, Pred, QueryPlan, RelStrategy, describe};
use mr_core::query::{Expr, JoinStrategy, Query};

use crate::Engine;
use crate::error::Result;
use crate::runner::{Plain, Runner};

/// A stream of result chunks.
pub type DocStream = Pin<Box<dyn Stream<Item = Result<Vec<Document>>> + Send>>;

impl Engine {
    pub(crate) fn plan(&self, q: &Query) -> Result<QueryPlan> {
        let catalog = self.catalog();
        Ok(Planner::new(&catalog, self.stats(), &self.inner.config.planner).plan(q)?)
    }

    /// Run a query, streaming results in chunks of `planner.chunk_size`.
    pub async fn query(&self, q: &Query) -> Result<DocStream> {
        let mut plan = self.plan(q)?;
        let db = self.inner.db.clone();
        resolve_plan(&mut Plain { db: &db }, &mut plan).await?;
        let compiled = compile::root_pipeline(&plan);
        let chunk = self.inner.config.planner.chunk_size.max(1);
        let cursor = db
            .collection::<Document>(&plan.collection)
            .aggregate(compiled.pipeline)
            .allow_disk_use(true)
            .batch_size(chunk as u32)
            .await?;

        struct State {
            cursor: mongodb::Cursor<Document>,
            db: mongodb::Database,
            includes: Vec<IncludePlan>,
            strip: Vec<String>,
            chunk: usize,
        }
        let state = State {
            cursor,
            db,
            includes: plan.includes,
            strip: compiled.strip,
            chunk,
        };
        let stream = futures::stream::try_unfold(state, |mut st| async move {
            let mut docs = Vec::with_capacity(st.chunk);
            while docs.len() < st.chunk {
                match st.cursor.try_next().await? {
                    Some(d) => docs.push(d),
                    None => break,
                }
            }
            if docs.is_empty() {
                return Ok(None);
            }
            load_includes(&mut Plain { db: &st.db }, &st.includes, &mut docs).await?;
            strip(&mut docs, &st.strip);
            Ok(Some((docs, st)))
        });
        Ok(Box::pin(stream))
    }

    /// Run a query and collect every result.
    pub async fn query_all(&self, q: &Query) -> Result<Vec<Document>> {
        let chunks: Vec<Vec<Document>> = self.query(q).await?.try_collect().await?;
        Ok(chunks.into_iter().flatten().collect())
    }

    /// Count documents matching a filter (relation paths allowed).
    pub async fn count(&self, collection: &str, filter: Option<&Expr>) -> Result<u64> {
        let catalog = self.catalog();
        let pred = Planner::new(&catalog, self.stats(), &self.inner.config.planner)
            .plan_filter(collection, filter)?;
        let mut runner = Plain { db: &self.inner.db };
        let pred = resolve_pred(&mut runner, pred).await?;
        if pred == Pred::Const(true) {
            let n = self
                .inner
                .db
                .collection::<Document>(collection)
                .count_documents(doc! {})
                .await?;
            return Ok(n);
        }
        let docs = runner
            .aggregate(collection, compile::count_pipeline(&pred))
            .await?;
        Ok(docs.first().map_or(0, |d| match d.get("n") {
            Some(Bson::Int32(n)) => *n as u64,
            Some(Bson::Int64(n)) => *n as u64,
            _ => 0,
        }))
    }

    /// Describe how a query would run: the logical plan and the root
    /// pipeline after semi-join resolution.
    pub async fn explain(&self, q: &Query) -> Result<Document> {
        let mut plan = self.plan(q)?;
        let logical = describe(&plan);
        resolve_plan(&mut Plain { db: &self.inner.db }, &mut plan).await?;
        let compiled = compile::root_pipeline(&plan);
        Ok(doc! {
            "plan": logical,
            "pipeline": compiled.pipeline,
            "strip": compiled.strip,
        })
    }
}

/// Resolve semi-joins in the root filter and in every include filter.
pub(crate) async fn resolve_plan(r: &mut dyn Runner, plan: &mut QueryPlan) -> Result<()> {
    let f = std::mem::replace(&mut plan.filter, Pred::Const(true));
    plan.filter = resolve_pred(r, f).await?;
    resolve_includes(r, &mut plan.includes).await
}

fn resolve_includes<'a>(
    r: &'a mut dyn Runner,
    incs: &'a mut [IncludePlan],
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        for inc in incs {
            let f = std::mem::replace(&mut inc.filter, Pred::Const(true));
            inc.filter = resolve_pred(r, f).await?;
            resolve_includes(r, &mut inc.includes).await?;
        }
        Ok(())
    })
}

/// Replace semi-join predicates with `$in` leaves (or switch them to
/// `$lookup` when the key set is too large).
pub(crate) fn resolve_pred<'a>(r: &'a mut dyn Runner, p: Pred) -> BoxFuture<'a, Result<Pred>> {
    Box::pin(async move {
        Ok(match p {
            Pred::And(v) => {
                let mut out = Vec::with_capacity(v.len());
                for x in v {
                    let x = resolve_pred(r, x).await?;
                    if x == Pred::Const(false) {
                        return Ok(x);
                    }
                    out.push(x);
                }
                Pred::and(out)
            }
            Pred::Or(v) => {
                let mut out = Vec::with_capacity(v.len());
                for x in v {
                    let x = resolve_pred(r, x).await?;
                    if x == Pred::Const(true) {
                        return Ok(x);
                    }
                    out.push(x);
                }
                Pred::or(out)
            }
            Pred::Not(x) => Pred::not(resolve_pred(r, *x).await?),
            Pred::Rel(mut rel) => {
                let inner = std::mem::replace(&mut rel.inner, Pred::Const(true));
                rel.inner = resolve_pred(r, inner).await?;
                if rel.inner == Pred::Const(false) {
                    return Ok(Pred::Const(false));
                }
                match rel.strategy {
                    RelStrategy::SemiJoin { max_keys } => match semi_join(r, &rel, max_keys).await?
                    {
                        Some(keys) => {
                            let parent = &rel.relation.path[0].from_field;
                            compile::semi_join_leaf(parent, keys)
                        }
                        None => {
                            rel.strategy = RelStrategy::Lookup;
                            Pred::Rel(rel)
                        }
                    },
                    RelStrategy::FkExists | RelStrategy::Lookup => Pred::Rel(rel),
                }
            }
            leaf @ (Pred::Leaf(_) | Pred::Const(_)) => leaf,
        })
    })
}

/// Run a semi-join; `None` means more than `cap` keys (use `$lookup`).
async fn semi_join(
    r: &mut dyn Runner,
    rel: &mr_core::plan::RelPred,
    cap: usize,
) -> Result<Option<Vec<Bson>>> {
    let plan = compile::semi_join_plan(rel);
    let mut keys: Option<Vec<Bson>> = None;
    for step in &plan.steps {
        let pipeline = compile::semi_join_step_pipeline(step, keys.take(), cap);
        let docs = r.aggregate(&step.collection, pipeline).await?;
        if docs.len() > cap {
            return Ok(None);
        }
        let ks: Vec<Bson> = docs
            .into_iter()
            .filter_map(|mut d| d.remove("_id"))
            .collect();
        if ks.is_empty() {
            return Ok(Some(vec![]));
        }
        keys = Some(ks);
    }
    Ok(keys)
}

fn strip(docs: &mut [Document], fields: &[String]) {
    if fields.is_empty() {
        return;
    }
    for d in docs {
        for f in fields {
            remove_path(d, f);
        }
    }
}

/// Distinct key values at `field` across `docs`, in first-seen order.
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

/// Load every batch include of `parents` (lookup includes were already
/// resolved inside the pipeline that produced them).
pub(crate) fn load_includes<'a>(
    r: &'a mut dyn Runner,
    includes: &'a [IncludePlan],
    parents: &'a mut [Document],
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        if parents.is_empty() {
            return Ok(());
        }
        for inc in includes
            .iter()
            .filter(|i| i.strategy == JoinStrategy::Batch)
        {
            if inc.relation.path.len() == 1 {
                load_direct(r, inc, parents).await?;
            } else {
                load_junction(r, inc, parents).await?;
            }
        }
        Ok(())
    })
}

/// Fetch children, load their own batch includes, and index them by join key.
async fn fetch_children(
    r: &mut dyn Runner,
    inc: &IncludePlan,
    collection: &str,
    compiled: compile::Compiled,
    join_field: &str,
) -> Result<(Vec<Document>, HashMap<String, Vec<usize>>)> {
    let mut children = r.aggregate(collection, compiled.pipeline).await?;
    // The per-key top-N fetch can return a child once per key it belongs to.
    let mut seen = HashSet::new();
    children.retain(|c| c.get("_id").is_none_or(|id| seen.insert(key_repr(id))));
    load_includes(r, &inc.includes, &mut children).await?;
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in children.iter().enumerate() {
        for k in key_values(c, join_field) {
            index.entry(key_repr(&k)).or_default().push(i);
        }
    }
    strip(&mut children, &compiled.strip);
    Ok((children, index))
}

fn attach(
    parent: &mut Document,
    inc: &IncludePlan,
    mut positions: Vec<usize>,
    children: &[Document],
) {
    positions.sort_unstable();
    positions.dedup();
    let value = if inc.is_one() {
        positions
            .first()
            .map_or(Bson::Null, |&i| Bson::Document(children[i].clone()))
    } else {
        if let Some(l) = inc.limit {
            positions.truncate(l as usize);
        }
        Bson::Array(
            positions
                .iter()
                .map(|&i| Bson::Document(children[i].clone()))
                .collect(),
        )
    };
    parent.insert(inc.alias.clone(), value);
}

async fn load_direct(
    r: &mut dyn Runner,
    inc: &IncludePlan,
    parents: &mut [Document],
) -> Result<()> {
    let hop = &inc.relation.path[0];
    let keys = distinct_keys(parents.iter(), &hop.from_field);
    let (children, index) = if keys.is_empty() {
        (vec![], HashMap::new())
    } else {
        let compiled = compile::batch_pipeline(inc, keys);
        fetch_children(r, inc, &hop.to, compiled, &hop.to_field).await?
    };
    for p in parents.iter_mut() {
        let positions = key_values(p, &hop.from_field)
            .iter()
            .flat_map(|k| index.get(&key_repr(k)).into_iter().flatten().copied())
            .collect();
        attach(p, inc, positions, &children);
    }
    Ok(())
}

async fn load_junction(
    r: &mut dyn Runner,
    inc: &IncludePlan,
    parents: &mut [Document],
) -> Result<()> {
    let (h1, h2) = (&inc.relation.path[0], &inc.relation.path[1]);
    let keys = distinct_keys(parents.iter(), &h1.from_field);
    let mut links: HashMap<String, Vec<Bson>> = HashMap::new();
    let mut target_keys = vec![];
    if !keys.is_empty() {
        let pairs = r
            .aggregate(&h1.to, compile::junction_pipeline(inc, keys))
            .await?;
        let mut seen = HashSet::new();
        for p in pairs {
            let (Some(a), Some(b)) = (p.get("a"), p.get("b")) else {
                continue;
            };
            links.entry(key_repr(a)).or_default().push(b.clone());
            if seen.insert(key_repr(b)) {
                target_keys.push(b.clone());
            }
        }
    }
    let (children, index) = if target_keys.is_empty() {
        (vec![], HashMap::new())
    } else {
        let compiled = compile::junction_target_pipeline(inc, target_keys);
        fetch_children(r, inc, &h2.to, compiled, &h2.to_field).await?
    };
    for p in parents.iter_mut() {
        let positions = key_values(p, &h1.from_field)
            .iter()
            .flat_map(|a| links.get(&key_repr(a)).into_iter().flatten())
            .flat_map(|b| index.get(&key_repr(b)).into_iter().flatten().copied())
            .collect();
        attach(p, inc, positions, &children);
    }
    Ok(())
}
