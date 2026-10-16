//! Query planning (D9).
//!
//! The planner normalizes a [`Query`] against the [`Catalog`]:
//! relation paths become quantified relational predicates ([`RelPred`]),
//! `All` is rewritten to `Not(Any(Not ..))`, and required includes become
//! `Any` predicates on their parent. It then picks a strategy for every
//! relational predicate and include using a small cost model fed by
//! collection statistics.

use bson::{Bson, Document, doc};
use serde::Serialize;

use crate::catalog::{Cardinality, Catalog, Relation};
use crate::error::{Result, query_err};
use crate::query::{Expr, Include, JoinStrategy, Query, SortKey};

/// Statistics the planner uses to estimate costs.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CollectionStats {
    pub count: u64,
    pub sharded: bool,
}

pub trait StatsProvider {
    fn stats(&self, collection: &str) -> CollectionStats;
}

/// Stats provider that knows nothing; every collection looks empty and unsharded.
pub struct NoStats;

impl StatsProvider for NoStats {
    fn stats(&self, _: &str) -> CollectionStats {
        CollectionStats::default()
    }
}

impl<F: Fn(&str) -> CollectionStats> StatsProvider for F {
    fn stats(&self, collection: &str) -> CollectionStats {
        self(collection)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannerConfig {
    /// Maximum keys a semi-join pre-query may return before falling back to `$lookup`.
    pub semi_join_max_keys: usize,
    /// Documents fetched per chunk when streaming results; also the parent
    /// estimate for unlimited queries.
    pub chunk_size: usize,
    /// Relative cost of one extra round trip to MongoDB.
    pub round_trip_cost: f64,
    /// Relative cost per parent document per hop of a `$lookup`.
    pub lookup_cost_per_parent: f64,
    /// Relative cost per parent document of a batched `$in` fetch.
    pub batch_cost_per_parent: f64,
    /// Maximum include nesting depth.
    pub max_include_depth: usize,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            semi_join_max_keys: 10_000,
            chunk_size: 500,
            round_trip_cost: 1.0,
            lookup_cost_per_parent: 0.05,
            batch_cost_per_parent: 0.005,
            max_include_depth: 8,
        }
    }
}

/// Normalized predicate over one collection.
#[derive(Debug, Clone, PartialEq)]
pub enum Pred {
    And(Vec<Pred>),
    Or(Vec<Pred>),
    Not(Box<Pred>),
    /// A MongoDB filter fragment over this collection's own fields.
    Leaf(Document),
    Const(bool),
    Rel(Box<RelPred>),
}

impl Pred {
    pub fn and(parts: Vec<Pred>) -> Pred {
        let mut out = Vec::with_capacity(parts.len());
        for p in parts {
            match p {
                Pred::Const(true) => {}
                Pred::Const(false) => return Pred::Const(false),
                Pred::And(inner) => out.extend(inner),
                p => out.push(p),
            }
        }
        match out.len() {
            0 => Pred::Const(true),
            1 => out.pop().unwrap(),
            _ => Pred::And(out),
        }
    }

    pub fn or(parts: Vec<Pred>) -> Pred {
        let mut out = Vec::with_capacity(parts.len());
        for p in parts {
            match p {
                Pred::Const(false) => {}
                Pred::Const(true) => return Pred::Const(true),
                Pred::Or(inner) => out.extend(inner),
                p => out.push(p),
            }
        }
        match out.len() {
            0 => Pred::Const(false),
            1 => out.pop().unwrap(),
            _ => Pred::Or(out),
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn not(p: Pred) -> Pred {
        match p {
            Pred::Const(b) => Pred::Const(!b),
            Pred::Not(inner) => *inner,
            p => Pred::Not(Box::new(p)),
        }
    }

    /// Does this predicate contain relational predicates (at any depth)?
    pub fn has_rel(&self) -> bool {
        match self {
            Pred::And(v) | Pred::Or(v) => v.iter().any(Pred::has_rel),
            Pred::Not(p) => p.has_rel(),
            Pred::Rel(_) => true,
            Pred::Leaf(_) | Pred::Const(_) => false,
        }
    }

    pub fn is_true(&self) -> bool {
        matches!(self, Pred::Const(true))
    }
}

/// `EXISTS (related document matching inner)`.
#[derive(Debug, Clone, PartialEq)]
pub struct RelPred {
    /// Unique per plan; names temporary fields.
    pub id: usize,
    pub relation: Relation,
    /// Predicate over the relation's target collection.
    pub inner: Pred,
    pub strategy: RelStrategy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelStrategy {
    /// Strict integrity guarantees the referenced document exists, so a
    /// non-null FK check suffices.
    FkExists,
    /// Pre-query the related keys and substitute `$in`; fall back to
    /// `$lookup` if more than `max_keys` keys come back.
    SemiJoin {
        max_keys: usize,
    },
    Lookup,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryPlan {
    pub collection: String,
    pub filter: Pred,
    pub select: Option<Vec<String>>,
    pub sort: Vec<SortKey>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
    pub includes: Vec<IncludePlan>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IncludePlan {
    pub alias: String,
    pub relation: Relation,
    pub strategy: JoinStrategy,
    /// Predicate over the relation's target.
    pub filter: Pred,
    pub select: Option<Vec<String>>,
    pub sort: Vec<SortKey>,
    pub limit: Option<u64>,
    pub includes: Vec<IncludePlan>,
}

impl IncludePlan {
    pub fn is_one(&self) -> bool {
        self.relation.cardinality == Cardinality::One
    }
}

pub struct Planner<'a> {
    catalog: &'a Catalog,
    stats: &'a dyn StatsProvider,
    config: &'a PlannerConfig,
    next_id: usize,
}

impl<'a> Planner<'a> {
    pub fn new(
        catalog: &'a Catalog,
        stats: &'a dyn StatsProvider,
        config: &'a PlannerConfig,
    ) -> Self {
        Self {
            catalog,
            stats,
            config,
            next_id: 0,
        }
    }

    pub fn plan(&mut self, q: &Query) -> Result<QueryPlan> {
        let info = self.catalog.collection(&q.collection)?;
        let mut filter = match &q.filter {
            Some(e) => self.normalize(&q.collection, e)?,
            None => Pred::Const(true),
        };
        check_select(&q.select)?;
        self.check_sort(&q.collection, &q.sort)?;
        let mut required = vec![];
        let parents = self
            .stats
            .stats(&info.def.name)
            .count
            .min(q.limit.unwrap_or(self.config.chunk_size as u64))
            .max(1);
        let includes = self.plan_includes(
            &q.collection,
            &q.include,
            parents as f64,
            false,
            1,
            &mut required,
        )?;
        if !required.is_empty() {
            required.insert(0, filter);
            filter = Pred::and(required);
        }
        Ok(QueryPlan {
            collection: q.collection.clone(),
            filter,
            select: q.select.clone(),
            sort: q.sort.clone(),
            skip: q.skip,
            limit: q.limit,
            includes,
        })
    }

    /// Plan a filter-only query (used for counts, updates and deletes).
    pub fn plan_filter(&mut self, collection: &str, filter: Option<&Expr>) -> Result<Pred> {
        self.catalog.collection(collection)?;
        match filter {
            Some(e) => self.normalize(collection, e),
            None => Ok(Pred::Const(true)),
        }
    }

    fn plan_includes(
        &mut self,
        collection: &str,
        includes: &[Include],
        parents: f64,
        parent_is_lookup: bool,
        depth: usize,
        required_out: &mut Vec<Pred>,
    ) -> Result<Vec<IncludePlan>> {
        if !includes.is_empty() && depth > self.config.max_include_depth {
            return Err(query_err!(
                "includes are nested deeper than {} levels",
                self.config.max_include_depth
            ));
        }
        let mut out: Vec<IncludePlan> = Vec::with_capacity(includes.len());
        for inc in includes {
            let rel = self.catalog.relation(collection, &inc.relation)?.clone();
            let alias = inc.alias().to_string();
            check_segment("include alias", &alias)?;
            if alias.contains('.') {
                return Err(query_err!("include alias `{alias}` must not contain `.`"));
            }
            if out.iter().any(|o| o.alias == alias) {
                return Err(query_err!("include alias `{alias}` is used twice"));
            }
            if rel.cardinality == Cardinality::One && inc.limit.is_some() {
                return Err(query_err!(
                    "include `{alias}`: limit only applies to to-many relations"
                ));
            }
            check_select(&inc.select)?;
            self.check_sort(&rel.target, &inc.sort)?;

            let mut filter = match &inc.filter {
                Some(e) => self.normalize(&rel.target, e)?,
                None => Pred::Const(true),
            };
            if inc.required {
                required_out.push(self.rel_pred(rel.clone(), filter.clone()));
            }

            let strategy = self.include_strategy(&rel, parents, parent_is_lookup, inc)?;
            let fanout = match rel.cardinality {
                Cardinality::One => 1.0,
                Cardinality::Many => {
                    let src = self.stats.stats(&rel.source).count.max(1) as f64;
                    let tgt = self.stats.stats(&rel.target).count as f64;
                    let ratio = (tgt / src).clamp(1.0, 1000.0);
                    inc.limit.map_or(ratio, |l| ratio.min(l as f64))
                }
            };
            let mut nested_required = vec![];
            let includes = self.plan_includes(
                &rel.target,
                &inc.include,
                parents * fanout,
                strategy == JoinStrategy::Lookup,
                depth + 1,
                &mut nested_required,
            )?;
            if !nested_required.is_empty() {
                nested_required.insert(0, filter);
                filter = Pred::and(nested_required);
            }
            out.push(IncludePlan {
                alias,
                relation: rel,
                strategy,
                filter,
                select: inc.select.clone(),
                sort: inc.sort.clone(),
                limit: inc.limit,
                includes,
            });
        }
        Ok(out)
    }

    fn include_strategy(
        &self,
        rel: &Relation,
        parents: f64,
        parent_is_lookup: bool,
        inc: &Include,
    ) -> Result<JoinStrategy> {
        if parent_is_lookup {
            if inc.strategy == Some(JoinStrategy::Batch) {
                return Err(query_err!(
                    "include `{}` cannot use the batch strategy inside a lookup include",
                    inc.alias()
                ));
            }
            return Ok(JoinStrategy::Lookup);
        }
        if let Some(s) = inc.strategy {
            return Ok(s);
        }
        if rel.path.iter().any(|h| self.stats.stats(&h.to).sharded) {
            return Ok(JoinStrategy::Batch);
        }
        let hops = rel.path.len() as f64;
        let lookup = parents * self.config.lookup_cost_per_parent * hops;
        let batch =
            self.config.round_trip_cost * hops + parents * self.config.batch_cost_per_parent;
        Ok(if lookup < batch {
            JoinStrategy::Lookup
        } else {
            JoinStrategy::Batch
        })
    }

    fn rel_pred(&mut self, relation: Relation, inner: Pred) -> Pred {
        let strategy = match relation
            .owned_constraint
            .map(|i| self.catalog.constraint(i))
        {
            Some(c) if c.is_strict() && inner.is_true() && relation.path.len() == 1 => {
                RelStrategy::FkExists
            }
            _ => RelStrategy::SemiJoin {
                max_keys: self.config.semi_join_max_keys,
            },
        };
        let id = self.next_id;
        self.next_id += 1;
        Pred::Rel(Box::new(RelPred {
            id,
            relation,
            inner,
            strategy,
        }))
    }

    /// Split `a.b.c` into a leading relation of `collection` and the remainder.
    fn split_relation<'p>(
        &self,
        collection: &str,
        path: &'p str,
    ) -> Option<(Relation, Option<&'p str>)> {
        let (head, rest) = match path.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (path, None),
        };
        self.catalog
            .relation(collection, head)
            .ok()
            .map(|r| (r.clone(), rest))
    }

    /// Normalize a leaf whose path may cross relations. `leaf` builds the
    /// final predicate on the last collection's own field path.
    fn path_pred(
        &mut self,
        collection: &str,
        path: &str,
        leaf: &dyn Fn(&str) -> Pred,
        on_relation: Option<&dyn Fn(Pred) -> Pred>,
    ) -> Result<Pred> {
        check_path(path)?;
        match self.split_relation(collection, path) {
            None => Ok(leaf(path)),
            Some((rel, None)) => match on_relation {
                Some(f) => {
                    let any = self.rel_pred(rel, Pred::Const(true));
                    Ok(f(any))
                }
                None => Err(query_err!(
                    "`{path}` is a relation; compare one of its fields (e.g. `{path}._id`) or \
                     use exists / is_null"
                )),
            },
            Some((rel, Some(rest))) => {
                if rel.cardinality == Cardinality::Many {
                    return Err(query_err!(
                        "`{}` is a to-many relation; filter it with `any` or `all`",
                        rel.name
                    ));
                }
                let target = rel.target.clone();
                let inner = self.path_pred(&target, rest, leaf, on_relation)?;
                Ok(self.rel_pred(rel, inner))
            }
        }
    }

    pub fn normalize(&mut self, collection: &str, e: &Expr) -> Result<Pred> {
        Ok(match e {
            Expr::And(v) => Pred::and(
                v.iter()
                    .map(|x| self.normalize(collection, x))
                    .collect::<Result<_>>()?,
            ),
            Expr::Or(v) => Pred::or(
                v.iter()
                    .map(|x| self.normalize(collection, x))
                    .collect::<Result<_>>()?,
            ),
            Expr::Not(x) => Pred::not(self.normalize(collection, x)?),
            Expr::Cmp { path, op, value } => {
                let (op, value) = (op.mongo(), value.clone());
                self.path_pred(
                    collection,
                    path,
                    &|p| leaf(p, doc! { op: value.clone() }),
                    None,
                )?
            }
            Expr::In { path, values } => {
                let values = Bson::Array(values.clone());
                self.path_pred(
                    collection,
                    path,
                    &|p| leaf(p, doc! { "$in": values.clone() }),
                    None,
                )?
            }
            Expr::Nin { path, values } => {
                let values = Bson::Array(values.clone());
                self.path_pred(
                    collection,
                    path,
                    &|p| leaf(p, doc! { "$nin": values.clone() }),
                    None,
                )?
            }
            Expr::Exists { path, exists } => {
                let exists = *exists;
                self.path_pred(
                    collection,
                    path,
                    &|p| leaf(p, doc! { "$exists": exists }),
                    Some(&|any| if exists { any } else { Pred::not(any) }),
                )?
            }
            Expr::IsNull { path } => self.path_pred(
                collection,
                path,
                &|p| Pred::Leaf(doc! { p: Bson::Null }),
                Some(&|any| Pred::not(any)),
            )?,
            Expr::Regex {
                path,
                pattern,
                options,
            } => {
                if options.chars().any(|c| !"imsx".contains(c)) {
                    return Err(query_err!("invalid regex options `{options}`"));
                }
                let (pattern, options) = (pattern.clone(), options.clone());
                self.path_pred(
                    collection,
                    path,
                    &|p| {
                        leaf(
                            p,
                            doc! { "$regex": pattern.clone(), "$options": options.clone() },
                        )
                    },
                    None,
                )?
            }
            Expr::Any { relation, filter } => {
                let rel = self.catalog.relation(collection, relation)?.clone();
                let inner = match filter {
                    Some(f) => self.normalize(&rel.target, f)?,
                    None => Pred::Const(true),
                };
                self.rel_pred(rel, inner)
            }
            Expr::All { relation, filter } => {
                let rel = self.catalog.relation(collection, relation)?.clone();
                let inner = Pred::not(self.normalize(&rel.target, filter)?);
                Pred::not(self.rel_pred(rel, inner))
            }
            Expr::Raw(d) => {
                check_raw(d)?;
                Pred::Leaf(d.clone())
            }
        })
    }

    fn check_sort(&self, collection: &str, sort: &[SortKey]) -> Result<()> {
        for k in sort {
            check_path(&k.field)?;
            if self.split_relation(collection, &k.field).is_some() {
                return Err(query_err!(
                    "sorting by a related field (`{}`) is not supported",
                    k.field
                ));
            }
        }
        Ok(())
    }
}

fn leaf(path: &str, cond: Document) -> Pred {
    Pred::Leaf(doc! { path: cond })
}

fn check_segment(what: &str, s: &str) -> Result<()> {
    if s.is_empty() || s.starts_with('$') || s.contains('\0') {
        return Err(query_err!("invalid {what} `{s}`"));
    }
    Ok(())
}

fn check_path(path: &str) -> Result<()> {
    for seg in path.split('.') {
        check_segment("field path", seg).map_err(|_| query_err!("invalid field path `{path}`"))?;
    }
    Ok(())
}

fn check_select(select: &Option<Vec<String>>) -> Result<()> {
    for p in select.iter().flatten() {
        check_path(p)?;
    }
    Ok(())
}

/// Operators that execute server-side JavaScript are never accepted.
const FORBIDDEN_OPERATORS: &[&str] = &["$where", "$function", "$accumulator"];

pub fn check_raw(d: &Document) -> Result<()> {
    fn walk(v: &Bson) -> Result<()> {
        match v {
            Bson::Document(d) => {
                for (k, v) in d {
                    if FORBIDDEN_OPERATORS.contains(&k.as_str()) {
                        return Err(query_err!("operator `{k}` is not allowed"));
                    }
                    walk(v)?;
                }
                Ok(())
            }
            Bson::Array(a) => a.iter().try_for_each(walk),
            Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => {
                Err(query_err!("JavaScript values are not allowed"))
            }
            _ => Ok(()),
        }
    }
    walk(&Bson::Document(d.clone()))
}

/// A human-readable description of a plan, returned by `explain`.
pub fn describe(plan: &QueryPlan) -> Document {
    fn pred(p: &Pred) -> Bson {
        match p {
            Pred::And(v) => doc! { "and": v.iter().map(pred).collect::<Vec<_>>() }.into(),
            Pred::Or(v) => doc! { "or": v.iter().map(pred).collect::<Vec<_>>() }.into(),
            Pred::Not(x) => doc! { "not": pred(x) }.into(),
            Pred::Leaf(d) => d.clone().into(),
            Pred::Const(b) => (*b).into(),
            Pred::Rel(r) => doc! {
                "exists": &r.relation.name,
                "target": &r.relation.target,
                "strategy": strategy_name(r.strategy),
                "where": pred(&r.inner),
            }
            .into(),
        }
    }
    fn strategy_name(s: RelStrategy) -> String {
        match s {
            RelStrategy::FkExists => "fk_exists".into(),
            RelStrategy::SemiJoin { max_keys } => format!("semi_join(max_keys={max_keys})"),
            RelStrategy::Lookup => "lookup".into(),
        }
    }
    fn include(i: &IncludePlan) -> Document {
        doc! {
            "alias": &i.alias,
            "relation": &i.relation.name,
            "target": &i.relation.target,
            "strategy": match i.strategy { JoinStrategy::Lookup => "lookup", JoinStrategy::Batch => "batch" },
            "hops": i.relation.path.len() as i32,
            "filter": pred(&i.filter),
            "include": i.includes.iter().map(include).collect::<Vec<_>>(),
        }
    }
    doc! {
        "collection": &plan.collection,
        "filter": pred(&plan.filter),
        "include": plan.includes.iter().map(include).collect::<Vec<_>>(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::schema::*;

    pub fn catalog() -> Catalog {
        let s = Schema {
            collections: vec![
                CollectionDef {
                    name: "users".into(),
                    fields: vec![FieldDef::new("name", FieldType::String)],
                    relations: vec![
                        RelationDef::has_many("posts", "posts", "author_id"),
                        RelationDef::belongs_to("company", "companies", "company_id")
                            .integrity(Integrity::Loose),
                    ],
                },
                CollectionDef::new("companies"),
                CollectionDef {
                    name: "posts".into(),
                    fields: vec![],
                    relations: vec![
                        RelationDef::belongs_to("author", "users", "author_id"),
                        RelationDef::has_many("comments", "comments", "post_id"),
                        RelationDef::many_to_many("tags", "tags", Via::Local("tag_ids".into())),
                    ],
                },
                CollectionDef {
                    name: "comments".into(),
                    fields: vec![],
                    relations: vec![RelationDef::belongs_to("post", "posts", "post_id")],
                },
                CollectionDef::new("tags"),
            ],
        };
        Catalog::new(s, 1).unwrap()
    }

    fn plan(q: &Query) -> Result<QueryPlan> {
        let cat = catalog();
        let cfg = PlannerConfig::default();
        Planner::new(&cat, &NoStats, &cfg).plan(q)
    }

    #[test]
    fn relation_paths_become_rel_preds() {
        let q = Query {
            filter: Some(Expr::eq("author.company.name", "acme").and(Expr::eq("title", "x"))),
            ..Query::new("posts")
        };
        let p = plan(&q).unwrap();
        let Pred::And(parts) = &p.filter else {
            panic!("{:?}", p.filter)
        };
        let Pred::Rel(author) = &parts[0] else {
            panic!()
        };
        assert_eq!(author.relation.name, "author");
        let Pred::Rel(company) = &author.inner else {
            panic!()
        };
        assert_eq!(company.relation.name, "company");
        assert_eq!(
            company.inner,
            Pred::Leaf(doc! { "name": { "$eq": "acme" } })
        );
        assert_eq!(parts[1], Pred::Leaf(doc! { "title": { "$eq": "x" } }));
    }

    #[test]
    fn to_many_paths_are_rejected() {
        let q = Query {
            filter: Some(Expr::eq("comments.body", "x")),
            ..Query::new("posts")
        };
        assert!(plan(&q).unwrap_err().to_string().contains("any"));
    }

    #[test]
    fn all_is_not_any_not() {
        let q = Query {
            filter: Some(Expr::all("comments", Expr::eq("ok", true))),
            ..Query::new("posts")
        };
        let p = plan(&q).unwrap();
        let Pred::Not(any) = &p.filter else { panic!() };
        let Pred::Rel(r) = any.as_ref() else { panic!() };
        assert!(matches!(r.inner, Pred::Not(_)));
    }

    #[test]
    fn strict_fk_existence_shortcut() {
        let q = Query {
            filter: Some(Expr::Exists {
                path: "author".into(),
                exists: true,
            }),
            ..Query::new("posts")
        };
        let p = plan(&q).unwrap();
        let Pred::Rel(r) = &p.filter else { panic!() };
        assert_eq!(r.strategy, RelStrategy::FkExists);

        // Loose relations cannot use it.
        let q = Query {
            filter: Some(Expr::IsNull {
                path: "company".into(),
            }),
            ..Query::new("users")
        };
        let p = plan(&q).unwrap();
        let Pred::Not(r) = &p.filter else { panic!() };
        let Pred::Rel(r) = r.as_ref() else { panic!() };
        assert!(matches!(r.strategy, RelStrategy::SemiJoin { .. }));
    }

    #[test]
    fn required_include_filters_parent() {
        let q = Query {
            include: vec![Include {
                required: true,
                filter: Some(Expr::eq("approved", true)),
                ..Include::new("comments")
            }],
            ..Query::new("posts")
        };
        let p = plan(&q).unwrap();
        assert!(matches!(p.filter, Pred::Rel(_)));
    }

    #[test]
    fn cost_model_picks_lookup_for_small_pages_and_batch_for_large() {
        let cat = catalog();
        let cfg = PlannerConfig::default();
        let stats = |_: &str| CollectionStats {
            count: 1_000_000,
            sharded: false,
        };
        let mut q = Query {
            include: vec![Include::new("author")],
            limit: Some(10),
            ..Query::new("posts")
        };
        let p = Planner::new(&cat, &stats, &cfg).plan(&q).unwrap();
        assert_eq!(p.includes[0].strategy, JoinStrategy::Lookup);

        q.limit = Some(1000);
        let p = Planner::new(&cat, &stats, &cfg).plan(&q).unwrap();
        assert_eq!(p.includes[0].strategy, JoinStrategy::Batch);

        let sharded = |c: &str| CollectionStats {
            count: 10,
            sharded: c == "users",
        };
        q.limit = Some(1);
        let p = Planner::new(&cat, &sharded, &cfg).plan(&q).unwrap();
        assert_eq!(p.includes[0].strategy, JoinStrategy::Batch);
    }

    #[test]
    fn lookup_children_stay_lookup() {
        let q = Query {
            include: vec![Include {
                strategy: Some(JoinStrategy::Lookup),
                include: vec![Include {
                    strategy: Some(JoinStrategy::Batch),
                    ..Include::new("posts")
                }],
                ..Include::new("author")
            }],
            ..Query::new("posts")
        };
        assert!(plan(&q).is_err());
    }

    #[test]
    fn rejects_dangerous_raw_and_bad_paths() {
        let q = Query {
            filter: Some(Expr::Raw(doc! { "$where": "1" })),
            ..Query::new("posts")
        };
        assert!(plan(&q).is_err());
        let q = Query {
            filter: Some(Expr::Raw(doc! { "$expr": { "$function": {} } })),
            ..Query::new("posts")
        };
        assert!(plan(&q).is_err());
        let q = Query {
            filter: Some(Expr::eq("$bad", 1)),
            ..Query::new("posts")
        };
        assert!(plan(&q).is_err());
    }
}
