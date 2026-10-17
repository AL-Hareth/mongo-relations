mod common;

use std::time::Duration;

use bson::{Bson, Document, doc};
use common::*;
use mr_engine::mr_core::query::Expr;

/// Poll until `f` returns true or the timeout elapses.
async fn eventually<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embeds_follow_targets() {
    let Some(t) = engine().await else { return };
    let s = seed(&t.engine).await;
    let e = t.engine.clone();
    let posts = e.database().collection::<Document>("posts");

    // Filled synchronously on insert.
    let p1 = posts.find_one(doc! { "_id": s.p1 }).await.unwrap().unwrap();
    assert_eq!(
        p1.get_document("author_snapshot").unwrap(),
        &doc! { "_id": s.alice, "name": "Alice" }
    );
    let tags = p1.get_array("tag_snapshots").unwrap();
    assert_eq!(
        tags,
        &vec![Bson::Document(doc! { "_id": s.rust, "name": "rust" })]
    );

    let bg = e.spawn_background();
    // Give the watchers a moment to open their change streams.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Renaming the author rewrites every copy — even when written directly.
    e.database()
        .collection::<Document>("users")
        .update_one(
            doc! { "_id": s.alice },
            doc! { "$set": { "name": "Alicia" } },
        )
        .await
        .unwrap();
    eventually("author rename to propagate", || async {
        posts
            .count_documents(doc! { "author_snapshot.name": "Alicia" })
            .await
            .unwrap()
            == 2
    })
    .await;

    // Array embeds update in place.
    e.update(
        "tags",
        Some(Expr::eq("_id", s.rust)),
        doc! { "$set": { "name": "rustlang" } },
    )
    .await
    .unwrap();
    eventually("tag rename to propagate", || async {
        posts
            .count_documents(doc! { "tag_snapshots.name": "rustlang" })
            .await
            .unwrap()
            == 2
    })
    .await;

    // A FK changed behind the layer's back is picked up from the owner side.
    posts
        .update_one(
            doc! { "_id": s.p3 },
            doc! { "$set": { "author_id": s.carol } },
        )
        .await
        .unwrap();
    eventually("owner FK change to refresh the snapshot", || async {
        let p3 = posts.find_one(doc! { "_id": s.p3 }).await.unwrap().unwrap();
        p3.get_document("author_snapshot")
            .is_ok_and(|a| a.get_str("name").is_ok_and(|n| n == "Carol"))
    })
    .await;

    // Loose deletes outside the layer remove copies.
    e.database()
        .collection::<Document>("tags")
        .delete_one(doc! { "_id": s.mongo })
        .await
        .unwrap();
    eventually("deleted tag to leave snapshots", || async {
        posts
            .count_documents(doc! { "tag_snapshots._id": s.mongo })
            .await
            .unwrap()
            == 0
    })
    .await;

    bg.shutdown().await;

    // Resume tokens were saved, so changes made while stopped are processed
    // after a restart.
    e.database()
        .collection::<Document>("users")
        .update_one(doc! { "_id": s.alice }, doc! { "$set": { "name": "Ally" } })
        .await
        .unwrap();
    let bg = e.spawn_background();
    eventually("offline change to be replayed", || async {
        posts
            .count_documents(doc! { "author_snapshot.name": "Ally" })
            .await
            .unwrap()
            == 2
    })
    .await;
    bg.shutdown().await;
}

#[tokio::test]
async fn rebuild_repairs_stale_snapshots() {
    let Some(t) = engine().await else { return };
    let s = seed(&t.engine).await;
    let e = &t.engine;
    let users = e.database().collection::<Document>("users");
    users
        .update_one(doc! { "_id": s.bob }, doc! { "$set": { "name": "Robert" } })
        .await
        .unwrap();
    let n = e.rebuild_embeds().await.unwrap();
    assert_eq!(n, 1);
    let p3 = e
        .database()
        .collection::<Document>("posts")
        .find_one(doc! { "_id": s.p3 })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        p3.get_document("author_snapshot")
            .unwrap()
            .get_str("name")
            .unwrap(),
        "Robert"
    );
}

/// A change made right after applying a schema that adds an embed must not
/// be lost while the sync worker is still (re)starting its watchers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changes_right_after_adding_an_embed_are_not_lost() {
    let Some(t) = engine().await else { return };
    let e = t.engine.clone();
    let mut without = blog_schema();
    for c in &mut without.collections {
        for r in &mut c.relations {
            r.embed = None;
        }
    }
    e.apply_schema(without, false).await.unwrap();
    let alice = bson::oid::ObjectId::new();
    e.insert(
        "users",
        vec![doc! { "_id": alice, "email": "a@x.io", "name": "Alice" }],
    )
    .await
    .unwrap();
    e.insert("posts", vec![doc! { "title": "t", "author_id": alice }])
        .await
        .unwrap();
    let bg = e.spawn_background();

    e.apply_schema(blog_schema(), false).await.unwrap();
    // No pause: the watcher for `users` may not be running yet.
    e.database()
        .collection::<Document>("users")
        .update_one(doc! { "_id": alice }, doc! { "$set": { "name": "Alicia" } })
        .await
        .unwrap();
    let posts = e.database().collection::<Document>("posts");
    eventually("the early rename to propagate", || async {
        posts
            .count_documents(doc! { "author_snapshot.name": "Alicia" })
            .await
            .unwrap()
            == 1
    })
    .await;
    bg.shutdown().await;
}
