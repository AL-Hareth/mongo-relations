//! Compile plans into MongoDB aggregation pipelines.
//!
//! The engine resolves semi-joins first (replacing those [`RelPred`]s with
//! `$in` leaves, or switching them to `$lookup` when too many keys come back).
//! Whatever relational predicates remain are compiled to `$lookup` stages.

use bson::{Bson, Document, doc};

use crate::catalog::Hop;
use crate::plan::{IncludePlan, Pred, QueryPlan, RelPred, RelStrategy};
use crate::query::{JoinStrategy, SortKey};

/// Prefix for temporary fields added during execution.
pub const TMP: &str = "__mr_";

fn tmp_rel(id: usize) -> String {
    format!("{TMP}r{id}")
}

/// The filter fragment for a strict FK existence check.
fn fk_exists_leaf(r: &RelPred) -> Document {
    let hop = &r.relation.path[0];
    if hop.from_array {
        doc! { format!("{}.0", hop.from_field): { "$exists": true } }
    } else {
        doc! { hop.from_field.clone(): { "$ne": Bson::Null } }
    }
}

/// Compile a predicate into a `$match` document, referencing the temporary
/// fields produced by [`lookup_stages_for`].
pub fn match_doc(p: &Pred) -> Document {
    match p {
        Pred::Leaf(d) => d.clone(),
        Pred::Const(true) => Document::new(),
        Pred::Const(false) => doc! { "$expr": false },
        Pred::And(v) => doc! { "$and": v.iter().map(match_doc).collect::<Vec<_>>() },
        Pred::Or(v) => doc! { "$or": v.iter().map(match_doc).collect::<Vec<_>>() },
        Pred::Not(x) => doc! { "$nor": [match_doc(x)] },
        Pred::Rel(r) => match r.strategy {
            RelStrategy::FkExists => fk_exists_leaf(r),
            RelStrategy::SemiJoin { .. } | RelStrategy::Lookup => {
                doc! { format!("{}.0", tmp_rel(r.id)): { "$exists": true } }
            }
        },
    }
}

fn collect_lookups<'a>(p: &'a Pred, out: &mut Vec<&'a RelPred>) {
    match p {
        Pred::And(v) | Pred::Or(v) => v.iter().for_each(|x| collect_lookups(x, out)),
        Pred::Not(x) => collect_lookups(x, out),
        Pred::Rel(r) if r.strategy != RelStrategy::FkExists => out.push(r),
        _ => {}
    }
}

fn needs_lookup(p: &Pred) -> bool {
    let mut v = vec![];
    collect_lookups(p, &mut v);
    !v.is_empty()
}

/// A pure filter document, if the predicate needs no `$lookup`.
pub fn filter_doc(p: &Pred) -> Option<Document> {
    if needs_lookup(p) {
        None
    } else {
        Some(match_doc(p))
    }
}

/// Pipeline stages that filter by `p`. Conjuncts that need no lookup run
/// first so they can use indexes; relational ones follow as `$lookup`s.
pub fn filter_stages(p: &Pred) -> Vec<Document> {
    let conjuncts: Vec<&Pred> = match p {
        Pred::Const(true) => return vec![],
        Pred::And(v) => v.iter().collect(),
        other => vec![other],
    };
    let (impure, pure): (Vec<&Pred>, Vec<&Pred>) =
        conjuncts.into_iter().partition(|c| needs_lookup(c));
    let mut stages = vec![];
    if !pure.is_empty() {
        let m = if pure.len() == 1 {
            match_doc(pure[0])
        } else {
            doc! { "$and": pure.iter().map(|c| match_doc(c)).collect::<Vec<_>>() }
        };
        stages.push(doc! { "$match": m });
    }
    if !impure.is_empty() {
        let mut rels = vec![];
        for c in &impure {
            collect_lookups(c, &mut rels);
        }
        for r in &rels {
            stages.push(rel_lookup_stage(r));
        }
        let m = if impure.len() == 1 {
            match_doc(impure[0])
        } else {
            doc! { "$and": impure.iter().map(|c| match_doc(c)).collect::<Vec<_>>() }
        };
        stages.push(doc! { "$match": m });
        stages.push(doc! { "$unset": rels.iter().map(|r| tmp_rel(r.id)).collect::<Vec<_>>() });
    }
    stages
}

/// Guard against `$lookup` matching documents whose join field is missing
/// when the local field is missing (MongoDB treats both as null).
fn not_null(field: &str) -> Document {
    doc! { "$match": { field: { "$ne": Bson::Null } } }
}

fn lookup(hop: &Hop, pipeline: Vec<Document>, as_: &str) -> Document {
    doc! { "$lookup": {
        "from": &hop.to,
        "localField": &hop.from_field,
        "foreignField": &hop.to_field,
        "pipeline": pipeline,
        "as": as_,
    } }
}

/// `$lookup` stage producing a non-empty array in the temp field when a
/// related document matching `r.inner` exists.
fn rel_lookup_stage(r: &RelPred) -> Document {
    let hops = &r.relation.path;
    let last = hops.last().expect("relation has at least one hop");
    let mut pipeline = vec![not_null(&last.to_field)];
    pipeline.extend(filter_stages(&r.inner));
    pipeline.push(doc! { "$limit": 1 });
    pipeline.push(doc! { "$project": { "_id": 1 } });
    for i in (1..hops.len()).rev() {
        let j = format!("{TMP}j");
        pipeline = vec![
            not_null(&hops[i - 1].to_field),
            lookup(&hops[i], pipeline, &j),
            doc! { "$match": { format!("{j}.0"): { "$exists": true } } },
            doc! { "$limit": 1 },
            doc! { "$project": { "_id": 1 } },
        ];
    }
    lookup(&hops[0], pipeline, &tmp_rel(r.id))
}

pub fn sort_doc(sort: &[SortKey]) -> Document {
    let mut d = Document::new();
    for k in sort {
        d.insert(k.field.clone(), if k.desc { -1 } else { 1 });
    }
    d
}

/// Build an inclusion projection for `select` plus `keep` (output fields that
/// must survive, e.g. include aliases) and `required` (fields needed only for
/// joining). Returns the projection and the required fields to strip later.
pub fn projection(
    select: &Option<Vec<String>>,
    keep: &[String],
    required: &[String],
) -> (Option<Document>, Vec<String>) {
    let Some(select) = select else {
        return (None, vec![]);
    };
    fn covered(p: &str, set: &[String]) -> bool {
        set.iter()
            .any(|s| s == p || (p.starts_with(s.as_str()) && p.as_bytes()[s.len()] == b'.'))
    }
    fn add(fields: &mut Vec<String>, p: &str) {
        if !covered(p, fields) {
            // Drop narrower paths already present to avoid path collisions.
            fields.retain(|f| !(f.starts_with(p) && f.as_bytes().get(p.len()) == Some(&b'.')));
            fields.push(p.to_string());
        }
    }
    let mut fields: Vec<String> = vec!["_id".into()];
    for p in select.iter().chain(keep) {
        add(&mut fields, p);
    }
    let mut strip = vec![];
    for p in required {
        if !covered(p, &fields) {
            strip.push(p.clone());
            add(&mut fields, p);
        }
    }
    let mut d = Document::new();
    for f in fields {
        d.insert(f, 1);
    }
    (Some(d), strip)
}

/// Stages that run a lookup-strategy include, nesting its own includes.
pub fn include_lookup_stages(inc: &IncludePlan) -> Vec<Document> {
    let hops = &inc.relation.path;
    let last = hops.last().expect("relation has at least one hop");
    let mut inner = vec![not_null(&last.to_field)];
    inner.extend(filter_stages(&inc.filter));
    for i in (1..hops.len()).rev() {
        let t = format!("{TMP}t");
        inner = vec![
            not_null(&hops[i - 1].to_field),
            lookup(&hops[i], inner, &t),
            doc! { "$unwind": format!("${t}") },
            doc! { "$replaceRoot": { "newRoot": format!("${t}") } },
        ];
    }
    if hops.len() > 1 {
        // Duplicate junction rows must not duplicate targets.
        inner.push(doc! { "$group": { "_id": "$_id", "d": { "$first": "$$ROOT" } } });
        inner.push(doc! { "$replaceRoot": { "newRoot": "$d" } });
    }
    if !inc.sort.is_empty() {
        inner.push(doc! { "$sort": sort_doc(&inc.sort) });
    }
    if inc.is_one() {
        inner.push(doc! { "$limit": 1 });
    } else if let Some(l) = inc.limit {
        inner.push(doc! { "$limit": l as i64 });
    }
    let mut aliases = vec![];
    for child in &inc.includes {
        inner.extend(include_lookup_stages(child));
        aliases.push(child.alias.clone());
    }
    if let (Some(p), _) = projection(&inc.select, &aliases, &[]) {
        inner.push(doc! { "$project": p });
    }
    let mut stages = vec![lookup(&hops[0], inner, &inc.alias)];
    if inc.is_one() {
        let a = format!("${}", inc.alias);
        stages.push(doc! { "$set": { &inc.alias: { "$ifNull": [ { "$arrayElemAt": [a, 0] }, Bson::Null ] } } });
    }
    stages
}

/// Fields a batch include needs from its parents.
pub fn batch_parent_fields(includes: &[IncludePlan]) -> Vec<String> {
    includes
        .iter()
        .filter(|i| i.strategy == JoinStrategy::Batch)
        .map(|i| i.relation.path[0].from_field.clone())
        .collect()
}

fn lookup_aliases(includes: &[IncludePlan]) -> Vec<String> {
    includes
        .iter()
        .filter(|i| i.strategy == JoinStrategy::Lookup)
        .map(|i| i.alias.clone())
        .collect()
}

fn lookup_include_stages(includes: &[IncludePlan]) -> Vec<Document> {
    includes
        .iter()
        .filter(|i| i.strategy == JoinStrategy::Lookup)
        .flat_map(include_lookup_stages)
        .collect()
}

/// A compiled pipeline plus fields to strip from its output after
/// batch includes have been stitched in.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    pub pipeline: Vec<Document>,
    pub strip: Vec<String>,
}

/// The root pipeline of a resolved plan.
pub fn root_pipeline(plan: &QueryPlan) -> Compiled {
    let mut pipeline = filter_stages(&plan.filter);
    if !plan.sort.is_empty() {
        pipeline.push(doc! { "$sort": sort_doc(&plan.sort) });
    }
    if let Some(s) = plan.skip {
        pipeline.push(doc! { "$skip": s as i64 });
    }
    if let Some(l) = plan.limit {
        pipeline.push(doc! { "$limit": l as i64 });
    }
    pipeline.extend(lookup_include_stages(&plan.includes));
    let (proj, strip) = projection(
        &plan.select,
        &lookup_aliases(&plan.includes),
        &batch_parent_fields(&plan.includes),
    );
    if let Some(p) = proj {
        pipeline.push(doc! { "$project": p });
    }
    Compiled { pipeline, strip }
}

/// Count pipeline for a resolved filter.
pub fn count_pipeline(filter: &Pred) -> Vec<Document> {
    let mut p = filter_stages(filter);
    p.push(doc! { "$count": "n" });
    p
}

/// Pipeline fetching the children of a single-hop batch include whose join
/// key is in `keys`. Applies the per-parent limit by keeping the top `limit`
/// per key (the top `limit` of a union of key groups is always contained in
/// the union of each group's top `limit`).
pub fn batch_pipeline(inc: &IncludePlan, keys: Vec<Bson>) -> Compiled {
    let hop = inc
        .relation
        .path
        .last()
        .expect("relation has at least one hop");
    let to_field = &hop.to_field;
    let mut pipeline = vec![doc! { "$match": { to_field.as_str(): { "$in": keys } } }];
    pipeline.extend(filter_stages(&inc.filter));
    let sort = sort_doc(&inc.sort);
    if !sort.is_empty() {
        pipeline.push(doc! { "$sort": sort.clone() });
    }
    if let (false, Some(limit)) = (inc.is_one(), inc.limit) {
        let k = format!("{TMP}k");
        pipeline.push(doc! { "$set": { &k: format!("${to_field}") } });
        if hop.to_array {
            pipeline.push(doc! { "$unwind": format!("${k}") });
        }
        pipeline.push(doc! { "$group": { "_id": format!("${k}"), "d": { "$push": "$$ROOT" } } });
        pipeline.push(doc! { "$project": { "d": { "$slice": ["$d", limit as i64] } } });
        pipeline.push(doc! { "$unwind": "$d" });
        pipeline.push(doc! { "$replaceRoot": { "newRoot": "$d" } });
        pipeline.push(doc! { "$unset": &k });
        if !sort.is_empty() {
            pipeline.push(doc! { "$sort": sort });
        }
    }
    finish_child(inc, pipeline, to_field)
}

fn finish_child(inc: &IncludePlan, mut pipeline: Vec<Document>, join_field: &str) -> Compiled {
    pipeline.extend(lookup_include_stages(&inc.includes));
    let mut required = vec![join_field.to_string()];
    required.extend(batch_parent_fields(&inc.includes));
    let (proj, strip) = projection(&inc.select, &lookup_aliases(&inc.includes), &required);
    if let Some(p) = proj {
        pipeline.push(doc! { "$project": p });
    }
    Compiled { pipeline, strip }
}

/// First step of a junction (two-hop) batch include: map parent keys to
/// target keys. Output documents are `{ a: <parent key>, b: <target key> }`.
pub fn junction_pipeline(inc: &IncludePlan, keys: Vec<Bson>) -> Vec<Document> {
    let (h1, h2) = (&inc.relation.path[0], &inc.relation.path[1]);
    vec![
        doc! { "$match": { h1.to_field.as_str(): { "$in": keys }, h2.from_field.as_str(): { "$ne": Bson::Null } } },
        doc! { "$project": { "_id": 0, "a": format!("${}", h1.to_field), "b": format!("${}", h2.from_field) } },
    ]
}

/// Second step of a junction batch include: fetch the targets.
/// The per-parent limit is applied in memory by the caller.
pub fn junction_target_pipeline(inc: &IncludePlan, target_keys: Vec<Bson>) -> Compiled {
    let h2 = &inc.relation.path[1];
    let mut pipeline = vec![doc! { "$match": { h2.to_field.as_str(): { "$in": target_keys } } }];
    pipeline.extend(filter_stages(&inc.filter));
    if !inc.sort.is_empty() {
        pipeline.push(doc! { "$sort": sort_doc(&inc.sort) });
    }
    finish_child(inc, pipeline, &h2.to_field)
}

/// One step of a semi-join: run `stages` (prefixed by an `$in` match on
/// `match_field` with the previous step's keys, if any) on `collection` and
/// collect distinct values of `key_field`.
#[derive(Debug, Clone, PartialEq)]
pub struct SemiJoinStep {
    pub collection: String,
    pub match_field: Option<String>,
    pub stages: Vec<Document>,
    pub key_field: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemiJoinPlan {
    pub steps: Vec<SemiJoinStep>,
    /// Field on the parent collection to test against the final keys.
    pub parent_field: String,
}

pub fn semi_join_plan(r: &RelPred) -> SemiJoinPlan {
    let hops = &r.relation.path;
    let last = hops.last().expect("relation has at least one hop");
    let mut steps = vec![SemiJoinStep {
        collection: last.to.clone(),
        match_field: None,
        stages: filter_stages(&r.inner),
        key_field: last.to_field.clone(),
    }];
    for i in (1..hops.len()).rev() {
        steps.push(SemiJoinStep {
            collection: hops[i - 1].to.clone(),
            match_field: Some(hops[i].from_field.clone()),
            stages: vec![],
            key_field: hops[i - 1].to_field.clone(),
        });
    }
    SemiJoinPlan {
        steps,
        parent_field: hops[0].from_field.clone(),
    }
}

/// Full pipeline for a semi-join step, collecting at most `cap + 1` distinct keys.
pub fn semi_join_step_pipeline(
    step: &SemiJoinStep,
    prev_keys: Option<Vec<Bson>>,
    cap: usize,
) -> Vec<Document> {
    let mut p = vec![];
    if let (Some(f), Some(keys)) = (&step.match_field, prev_keys) {
        p.push(doc! { "$match": { f.as_str(): { "$in": keys } } });
    }
    p.extend(step.stages.iter().cloned());
    p.push(doc! { "$project": { "_id": 0, "k": format!("${}", step.key_field) } });
    p.push(doc! { "$unwind": "$k" });
    p.push(doc! { "$match": { "k": { "$ne": Bson::Null } } });
    p.push(doc! { "$group": { "_id": "$k" } });
    p.push(doc! { "$limit": (cap as i64) + 1 });
    p
}

/// Leaf replacing a resolved semi-join.
pub fn semi_join_leaf(parent_field: &str, keys: Vec<Bson>) -> Pred {
    if keys.is_empty() {
        return Pred::Const(false);
    }
    Pred::Leaf(doc! { parent_field: { "$in": keys } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::catalog;
    use crate::plan::{NoStats, Planner, PlannerConfig};
    use crate::query::{Expr, Include, Query};

    fn plan(q: &Query) -> QueryPlan {
        let cat = catalog();
        let cfg = PlannerConfig::default();
        Planner::new(&cat, &NoStats, &cfg).plan(q).unwrap()
    }

    fn force_lookup(p: &mut Pred) {
        match p {
            Pred::And(v) | Pred::Or(v) => v.iter_mut().for_each(force_lookup),
            Pred::Not(x) => force_lookup(x),
            Pred::Rel(r) => {
                if r.strategy != RelStrategy::FkExists {
                    r.strategy = RelStrategy::Lookup;
                }
                force_lookup(&mut r.inner);
            }
            _ => {}
        }
    }

    #[test]
    fn pure_filter_is_one_match() {
        let p = plan(&Query {
            filter: Some(Expr::eq("title", "a")),
            ..Query::new("posts")
        });
        assert_eq!(
            filter_stages(&p.filter),
            vec![doc! { "$match": { "title": { "$eq": "a" } } }]
        );
    }

    #[test]
    fn lookup_predicates_come_after_pure_ones() {
        let mut p = plan(&Query {
            filter: Some(Expr::eq("title", "a").and(Expr::eq("author.name", "bob"))),
            ..Query::new("posts")
        });
        force_lookup(&mut p.filter);
        let stages = filter_stages(&p.filter);
        assert_eq!(stages.len(), 4);
        assert_eq!(stages[0], doc! { "$match": { "title": { "$eq": "a" } } });
        assert_eq!(
            stages[1],
            doc! { "$lookup": {
                "from": "users", "localField": "author_id", "foreignField": "_id",
                "pipeline": [
                    { "$match": { "_id": { "$ne": null } } },
                    { "$match": { "name": { "$eq": "bob" } } },
                    { "$limit": 1 },
                    { "$project": { "_id": 1 } },
                ],
                "as": "__mr_r0",
            } }
        );
        assert_eq!(
            stages[2],
            doc! { "$match": { "__mr_r0.0": { "$exists": true } } }
        );
        assert_eq!(stages[3], doc! { "$unset": ["__mr_r0"] });
    }

    #[test]
    fn fk_exists_compiles_to_field_check() {
        let p = plan(&Query {
            filter: Some(Expr::Exists {
                path: "author".into(),
                exists: false,
            }),
            ..Query::new("posts")
        });
        assert_eq!(
            filter_stages(&p.filter),
            vec![doc! { "$match": { "$nor": [ { "author_id": { "$ne": null } } ] } }]
        );
    }

    #[test]
    fn root_pipeline_with_lookup_and_batch_includes() {
        let p = plan(&Query {
            select: Some(vec!["title".into()]),
            include: vec![
                Include {
                    strategy: Some(JoinStrategy::Lookup),
                    select: Some(vec!["name".into()]),
                    ..Include::new("author")
                },
                Include {
                    strategy: Some(JoinStrategy::Batch),
                    ..Include::new("tags")
                },
            ],
            limit: Some(5),
            ..Query::new("posts")
        });
        let c = root_pipeline(&p);
        assert_eq!(c.strip, vec!["tag_ids".to_string()]);
        assert_eq!(c.pipeline[0], doc! { "$limit": 5_i64 });
        assert_eq!(
            c.pipeline[1],
            doc! { "$lookup": {
                "from": "users", "localField": "author_id", "foreignField": "_id",
                "pipeline": [
                    { "$match": { "_id": { "$ne": null } } },
                    { "$limit": 1 },
                    { "$project": { "_id": 1, "name": 1 } },
                ],
                "as": "author",
            } }
        );
        assert_eq!(
            c.pipeline[2],
            doc! { "$set": { "author": { "$ifNull": [ { "$arrayElemAt": ["$author", 0] }, null ] } } }
        );
        assert_eq!(
            c.pipeline[3],
            doc! { "$project": { "_id": 1, "title": 1, "author": 1, "tag_ids": 1 } }
        );
    }

    #[test]
    fn batch_pipeline_with_per_parent_limit() {
        let p = plan(&Query {
            include: vec![Include {
                strategy: Some(JoinStrategy::Batch),
                limit: Some(2),
                sort: vec![SortKey::desc("at")],
                select: Some(vec!["body".into()]),
                ..Include::new("comments")
            }],
            ..Query::new("posts")
        });
        let c = batch_pipeline(&p.includes[0], vec![Bson::Int32(1)]);
        assert_eq!(c.strip, vec!["post_id".to_string()]);
        let ops: Vec<_> = c
            .pipeline
            .iter()
            .map(|s| s.keys().next().unwrap().clone())
            .collect();
        assert_eq!(
            ops,
            vec![
                "$match",
                "$sort",
                "$set",
                "$group",
                "$project",
                "$unwind",
                "$replaceRoot",
                "$unset",
                "$sort",
                "$project"
            ]
        );
    }

    #[test]
    fn projection_handles_collisions_and_required() {
        let (p, strip) = projection(
            &Some(vec!["a.b".into(), "a".into()]),
            &[],
            &["a.c".into(), "k".into()],
        );
        assert_eq!(p.unwrap(), doc! { "_id": 1, "a": 1, "k": 1 });
        assert_eq!(strip, vec!["k".to_string()]);
        assert_eq!(projection(&None, &[], &["k".into()]), (None, vec![]));
    }

    #[test]
    fn semi_join_steps() {
        let p = plan(&Query {
            filter: Some(Expr::any("tags", Some(Expr::eq("name", "rust")))),
            ..Query::new("posts")
        });
        let Pred::Rel(r) = &p.filter else { panic!() };
        let sj = semi_join_plan(r);
        assert_eq!(sj.parent_field, "tag_ids");
        assert_eq!(sj.steps.len(), 1);
        let pipe = semi_join_step_pipeline(&sj.steps[0], None, 10);
        assert_eq!(pipe.last().unwrap(), &doc! { "$limit": 11_i64 });
    }
}
