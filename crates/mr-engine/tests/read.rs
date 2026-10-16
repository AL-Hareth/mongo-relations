mod common;

use bson::{Bson, Document, doc};
use common::*;
use mr_engine::mr_core::query::{Expr, Include, JoinStrategy, SortKey};

const BOTH: [JoinStrategy; 2] = [JoinStrategy::Lookup, JoinStrategy::Batch];

fn inc(rel: &str, s: JoinStrategy) -> Include {
    Include {
        strategy: Some(s),
        ..Include::new(rel)
    }
}

#[tokio::test]
async fn belongs_to_and_has_one_includes() {
    let Some(t) = engine().await else { return };
    let s = seed(&t.engine).await;
    for strategy in BOTH {
        let docs = t
            .engine
            .query_all(&mr_engine::mr_core::query::Query {
                sort: vec![SortKey::asc("title")],
                include: vec![Include {
                    select: Some(vec!["name".into()]),
                    include: vec![inc("profile", strategy)],
                    ..inc("author", strategy)
                }],
                ..q("posts")
            })
            .await
            .unwrap();
        assert_eq!(
            titles(&docs),
            vec!["Aggregation", "Hello", "Ownership"],
            "{strategy:?}"
        );
        let author = docs[0].get_document("author").unwrap();
        assert_eq!(author.get_str("name").unwrap(), "Alice");
        assert!(author.get("email").is_none(), "select limits fields");
        assert_eq!(
            author
                .get_document("profile")
                .unwrap()
                .get_str("bio")
                .unwrap(),
            "rustacean"
        );
        let bob = docs[1].get_document("author").unwrap();
        assert_eq!(
            bob.get("profile"),
            Some(&Bson::Null),
            "missing has_one is null"
        );
        assert_eq!(docs[0].get_object_id("author_id").unwrap(), s.alice);
    }
}

#[tokio::test]
async fn has_many_with_per_parent_sort_and_limit() {
    let Some(t) = engine().await else { return };
    let s = seed(&t.engine).await;
    for strategy in BOTH {
        let docs = t
            .engine
            .query_all(&mr_engine::mr_core::query::Query {
                filter: Some(Expr::is_in("_id", [s.p1, s.p2, s.p3])),
                sort: vec![SortKey::desc("likes")],
                include: vec![Include {
                    sort: vec![SortKey::desc("n")],
                    limit: Some(2),
                    select: Some(vec!["body".into()]),
                    ..inc("comments", strategy)
                }],
                ..q("posts")
            })
            .await
            .unwrap();
        let bodies = |d: &Document| -> Vec<String> {
            d.get_array("comments")
                .unwrap()
                .iter()
                .map(|c| {
                    c.as_document()
                        .unwrap()
                        .get_str("body")
                        .unwrap()
                        .to_string()
                })
                .collect()
        };
        assert_eq!(bodies(&docs[0]), vec!["again", "great"], "{strategy:?}");
        assert_eq!(bodies(&docs[1]), vec!["ok"]);
        assert!(bodies(&docs[2]).is_empty());
        // The join key was only fetched for stitching and is stripped.
        let c = docs[0].get_array("comments").unwrap()[0]
            .as_document()
            .unwrap();
        assert!(c.get("post_id").is_none(), "{strategy:?}: {c}");
    }
}

#[tokio::test]
async fn many_to_many_local_and_remote() {
    let Some(t) = engine().await else { return };
    let s = seed(&t.engine).await;
    for strategy in BOTH {
        let posts = t
            .engine
            .query_all(&mr_engine::mr_core::query::Query {
                filter: Some(Expr::eq("_id", s.p2)),
                include: vec![Include {
                    sort: vec![SortKey::asc("name")],
                    ..inc("tags", strategy)
                }],
                ..q("posts")
            })
            .await
            .unwrap();
        let names: Vec<_> = posts[0]
            .get_array("tags")
            .unwrap()
            .iter()
            .map(|t| t.as_document().unwrap().get_str("name").unwrap())
            .collect();
        assert_eq!(names, vec!["mongo", "rust"], "{strategy:?}");

        let tags = t
            .engine
            .query_all(&mr_engine::mr_core::query::Query {
                filter: Some(Expr::eq("_id", s.rust)),
                include: vec![Include {
                    sort: vec![SortKey::asc("title")],
                    ..inc("posts", strategy)
                }],
                ..q("tags")
            })
            .await
            .unwrap();
        let posts: Vec<Document> = tags[0]
            .get_array("posts")
            .unwrap()
            .iter()
            .map(|p| p.as_document().unwrap().clone())
            .collect();
        assert_eq!(
            titles(&posts),
            vec!["Aggregation", "Ownership"],
            "{strategy:?}"
        );
    }
}

#[tokio::test]
async fn junction_many_to_many() {
    let Some(t) = engine().await else { return };
    let e = &t.engine;
    e.apply_schema(blog_schema(), false).await.unwrap();
    let ids = e
        .insert(
            "students",
            vec![doc! { "name": "Ann" }, doc! { "name": "Ben" }],
        )
        .await
        .unwrap()
        .inserted_ids;
    let cids = e
        .insert(
            "courses",
            vec![doc! { "title": "Math" }, doc! { "title": "Art" }],
        )
        .await
        .unwrap()
        .inserted_ids;
    e.insert(
        "enrollments",
        vec![
            doc! { "student_id": &ids[0], "course_id": &cids[0] },
            doc! { "student_id": &ids[0], "course_id": &cids[1] },
            doc! { "student_id": &ids[0], "course_id": &cids[1] }, // duplicate link
            doc! { "student_id": &ids[1], "course_id": &cids[1] },
        ],
    )
    .await
    .unwrap();
    for strategy in BOTH {
        let docs = e
            .query_all(&mr_engine::mr_core::query::Query {
                sort: vec![SortKey::asc("name")],
                include: vec![Include {
                    sort: vec![SortKey::asc("title")],
                    ..inc("courses", strategy)
                }],
                ..q("students")
            })
            .await
            .unwrap();
        let courses = |d: &Document| -> Vec<String> {
            d.get_array("courses")
                .unwrap()
                .iter()
                .map(|c| {
                    c.as_document()
                        .unwrap()
                        .get_str("title")
                        .unwrap()
                        .to_string()
                })
                .collect()
        };
        assert_eq!(courses(&docs[0]), vec!["Art", "Math"], "{strategy:?}");
        assert_eq!(courses(&docs[1]), vec!["Art"], "{strategy:?}");
    }
    // Filter through the junction.
    let n = e
        .count(
            "students",
            Some(&Expr::any("courses", Some(Expr::eq("title", "Math")))),
        )
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn relation_path_filters_semi_join_and_lookup_agree() {
    for max_keys in [10_000, 0] {
        let Some(t) = engine_with(|c| c.planner.semi_join_max_keys = max_keys).await else {
            return;
        };
        let s = seed(&t.engine).await;
        let e = &t.engine;
        let found = |docs: Vec<Document>| titles(&docs);
        let sorted = |f: Expr| mr_engine::mr_core::query::Query {
            filter: Some(f),
            sort: vec![SortKey::asc("title")],
            ..q("posts")
        };

        // To-one path.
        let docs = e
            .query_all(&sorted(Expr::eq("author.country", "US")))
            .await
            .unwrap();
        assert_eq!(
            found(docs),
            vec!["Aggregation", "Ownership"],
            "max_keys={max_keys}"
        );

        // Negation keeps SQL semantics: authors that exist and are not in US.
        let docs = e
            .query_all(&sorted(Expr::ne("author.country", "US")))
            .await
            .unwrap();
        assert_eq!(found(docs), vec!["Hello"]);

        // EXISTS over to-many, combined with OR.
        let f = Expr::any("comments", Some(Expr::eq("body", "ok"))).or(Expr::eq("title", "Hello"));
        let docs = e.query_all(&sorted(f)).await.unwrap();
        assert_eq!(found(docs), vec!["Aggregation", "Hello"]);

        // ALL is vacuously true without related documents.
        let f = Expr::all("comments", Expr::eq("user_id", s.carol));
        let docs = e.query_all(&sorted(f)).await.unwrap();
        assert_eq!(found(docs), vec!["Aggregation", "Hello"]);

        // Nested: posts whose comments were written by someone in DE.
        let f = Expr::any("comments", Some(Expr::eq("user.country", "DE")));
        let docs = e.query_all(&sorted(f)).await.unwrap();
        assert_eq!(found(docs), vec!["Ownership"]);

        // Many-to-many path through an array FK.
        let f = Expr::any("tags", Some(Expr::eq("name", "mongo")));
        assert_eq!(
            found(e.query_all(&sorted(f)).await.unwrap()),
            vec!["Aggregation"]
        );

        // Users without posts.
        let n = e
            .count("users", Some(&Expr::any("posts", None).not()))
            .await
            .unwrap();
        assert_eq!(n, 1);
    }
}

#[tokio::test]
async fn required_include_is_an_inner_join_and_respects_limit() {
    let Some(t) = engine().await else { return };
    seed(&t.engine).await;
    for strategy in BOTH {
        let docs = t
            .engine
            .query_all(&mr_engine::mr_core::query::Query {
                sort: vec![SortKey::asc("title")],
                limit: Some(1),
                include: vec![Include {
                    required: true,
                    filter: Some(Expr::eq("body", "ok")),
                    ..inc("comments", strategy)
                }],
                ..q("posts")
            })
            .await
            .unwrap();
        assert_eq!(titles(&docs), vec!["Aggregation"], "{strategy:?}");
        assert_eq!(docs[0].get_array("comments").unwrap().len(), 1);
    }
}

#[tokio::test]
async fn select_strips_join_keys_and_streams_in_chunks() {
    let Some(t) = engine_with(|c| c.planner.chunk_size = 2).await else {
        return;
    };
    seed(&t.engine).await;
    use futures::TryStreamExt;
    let stream = t
        .engine
        .query(&mr_engine::mr_core::query::Query {
            select: Some(vec!["title".into()]),
            sort: vec![SortKey::asc("title")],
            include: vec![inc("author", JoinStrategy::Batch)],
            ..q("posts")
        })
        .await
        .unwrap();
    let chunks: Vec<Vec<Document>> = stream.try_collect().await.unwrap();
    assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 1]);
    let first = &chunks[0][0];
    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        vec!["_id", "title", "author"]
    );
}

#[tokio::test]
async fn explain_reports_strategies() {
    let Some(t) = engine().await else { return };
    seed(&t.engine).await;
    let plan = t
        .engine
        .explain(&mr_engine::mr_core::query::Query {
            filter: Some(Expr::eq("author.country", "US")),
            include: vec![Include::new("comments")],
            limit: Some(5),
            ..q("posts")
        })
        .await
        .unwrap();
    let filter = plan
        .get_document("plan")
        .unwrap()
        .get_document("filter")
        .unwrap();
    assert_eq!(filter.get_str("exists").unwrap(), "author");
    assert!(filter.get_str("strategy").unwrap().starts_with("semi_join"));
    let first_stage = plan.get_array("pipeline").unwrap()[0]
        .as_document()
        .unwrap();
    assert!(
        first_stage
            .get_document("$match")
            .unwrap()
            .contains_key("author_id"),
        "{first_stage}"
    );
}
