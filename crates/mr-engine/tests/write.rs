mod common;

use std::time::Duration;

use bson::{Bson, doc, oid::ObjectId};
use common::*;
use mr_engine::mr_core::query::Expr;
use mr_engine::mr_core::write::WriteOp;
use mr_engine::{Error, ErrorKind};

#[tokio::test]
async fn schema_apply_is_idempotent_and_installs_validators() {
    let Some(t) = engine().await else { return };
    let e = &t.engine;
    let r1 = e.apply_schema(blog_schema(), false).await.unwrap();
    assert!(r1.changed);
    assert_eq!(r1.version, 1);
    assert!(
        r1.created_indexes
            .iter()
            .any(|i| i == "posts.mr_fk_author_id")
    );
    assert!(
        r1.created_indexes
            .iter()
            .any(|i| i == "users.mr_uniq_email")
    );

    let r2 = e.apply_schema(blog_schema(), false).await.unwrap();
    assert!(!r2.changed);
    assert!(r2.created_indexes.is_empty());
    assert_eq!(e.schema_history(10).await.unwrap().len(), 1);

    // Writes that bypass the layer still hit the $jsonSchema validator...
    let raw = e.database().collection::<bson::Document>("users");
    let err = raw.insert_one(doc! { "email": 42 }).await.unwrap_err();
    assert!(err.to_string().contains("validation"), "{err}");
    // ...but undeclared fields stay free-form.
    raw.insert_one(doc! { "email": "x@y.z", "anything": { "goes": [1, "a"] } })
        .await
        .unwrap();

    // Removing a relation drops its index on the next apply.
    let mut s = blog_schema();
    s.collections
        .retain(|c| !["students", "courses", "enrollments"].contains(&c.name.as_str()));
    let r3 = e.apply_schema(s, false).await.unwrap();
    assert_eq!(r3.version, 2);
    assert!(
        r3.dropped_indexes
            .iter()
            .any(|i| i.starts_with("enrollments."))
    );
}

#[tokio::test]
async fn insert_validates_coerces_and_checks_foreign_keys() {
    let Some(t) = engine().await else { return };
    let s = seed(&t.engine).await;
    let e = &t.engine;

    let err = e
        .insert("posts", vec![doc! { "title": 1 }])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidArgument, "{err}");

    let ghost = ObjectId::new();
    let err = e
        .insert("posts", vec![doc! { "title": "x", "author_id": ghost }])
        .await
        .unwrap_err();
    match &err {
        Error::ForeignKey {
            field,
            target,
            missing,
            ..
        } => {
            assert_eq!((field.as_str(), target.as_str()), ("author_id", "users"));
            assert_eq!(missing, &vec![Bson::ObjectId(ghost)]);
        }
        other => panic!("{other}"),
    }
    assert_eq!(err.kind(), ErrorKind::FailedPrecondition);

    // Array FKs are checked element-wise.
    let err = e
        .insert(
            "posts",
            vec![doc! { "title": "x", "tag_ids": [s.rust, ghost] }],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ForeignKey { .. }), "{err}");

    // Hex strings are coerced into declared object_id fields; ints into longs.
    let r = e
        .insert(
            "posts",
            vec![doc! { "title": "ok", "author_id": s.bob.to_hex(), "likes": 3 }],
        )
        .await
        .unwrap();
    let id = r.inserted_ids[0].clone();
    let doc = e
        .database()
        .collection::<bson::Document>("posts")
        .find_one(doc! { "_id": id })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.get_object_id("author_id").unwrap(), s.bob);
    assert!(matches!(doc.get("likes"), Some(Bson::Int64(3))));

    // Self references may be satisfied within the same insert.
    let (root, leaf) = (ObjectId::new(), ObjectId::new());
    e.insert(
        "categories",
        vec![
            doc! { "_id": leaf, "name": "leaf", "parent_id": root },
            doc! { "_id": root, "name": "root" },
        ],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn delete_applies_cascade_set_null_and_restrict() {
    let Some(t) = engine().await else { return };
    let s = seed(&t.engine).await;
    let e = &t.engine;

    // Restrict: a category with children cannot be deleted.
    let (root, leaf) = (ObjectId::new(), ObjectId::new());
    e.insert(
        "categories",
        vec![
            doc! { "_id": root },
            doc! { "_id": leaf, "parent_id": root },
        ],
    )
    .await
    .unwrap();
    let err = e
        .delete("categories", Some(Expr::eq("_id", root)))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Restrict { .. }), "{err}");
    // Deleting both together is fine (the referencing row goes too).
    let r = e
        .delete("categories", Some(Expr::is_in("_id", [root, leaf])))
        .await
        .unwrap();
    assert_eq!(r.deleted, 2);

    // Deleting a tag pulls it out of posts.tag_ids (set_null on an array FK).
    let r = e
        .delete("tags", Some(Expr::eq("name", "rust")))
        .await
        .unwrap();
    assert_eq!(r.deleted, 1);
    let posts = e.database().collection::<bson::Document>("posts");
    let p2 = posts.find_one(doc! { "_id": s.p2 }).await.unwrap().unwrap();
    assert_eq!(
        p2.get_array("tag_ids").unwrap(),
        &vec![Bson::ObjectId(s.mongo)]
    );

    // Deleting Alice cascades to her posts, their comments and her profile;
    // comments she did not write but that were on her posts go with the posts.
    let r = e
        .delete("users", Some(Expr::eq("_id", s.alice)))
        .await
        .unwrap();
    assert_eq!(r.deleted, 1);
    let count = |c: &str| {
        let c = e.database().collection::<bson::Document>(c);
        async move { c.count_documents(doc! {}).await.unwrap() }
    };
    assert_eq!(count("posts").await, 1);
    assert_eq!(count("comments").await, 0);
    assert_eq!(count("profiles").await, 0);
    let by = |c: &str| {
        r.cascaded
            .iter()
            .find(|x| x.collection == c)
            .unwrap()
            .deleted
    };
    assert_eq!(by("posts"), 2);
    assert_eq!(by("comments"), 4);
    assert_eq!(by("profiles"), 1);

    // Deleting Carol nulls comments.user_id (none left here, so re-seed one).
    e.insert(
        "comments",
        vec![doc! { "post_id": s.p3, "user_id": s.carol, "body": "hi" }],
    )
    .await
    .unwrap();
    let r = e
        .delete("users", Some(Expr::eq("_id", s.carol)))
        .await
        .unwrap();
    assert_eq!(r.cascaded[0].nulled, 1);
    let c = e
        .database()
        .collection::<bson::Document>("comments")
        .find_one(doc! {})
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.get("user_id"), Some(&Bson::Null));
}

#[tokio::test]
async fn update_checks_new_foreign_keys_and_rolls_back() {
    let Some(t) = engine().await else { return };
    let s = seed(&t.engine).await;
    let e = &t.engine;
    let ghost = ObjectId::new();

    let err = e
        .update(
            "posts",
            Some(Expr::eq("author_id", s.alice)),
            doc! { "$set": { "author_id": ghost } },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ForeignKey { .. }), "{err}");
    // Nothing changed.
    assert_eq!(
        e.count("posts", Some(&Expr::eq("author_id", s.alice)))
            .await
            .unwrap(),
        2
    );

    let err = e
        .update("posts", None, doc! { "$push": { "tag_ids": ghost } })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ForeignKey { .. }), "{err}");

    // A valid FK change also refreshes the embedded snapshot synchronously.
    let r = e
        .update(
            "posts",
            Some(Expr::eq("_id", s.p3)),
            doc! { "$set": { "author_id": s.carol } },
        )
        .await
        .unwrap();
    assert_eq!(r.modified, 1);
    let p3 = e
        .database()
        .collection::<bson::Document>("posts")
        .find_one(doc! { "_id": s.p3 })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        p3.get_document("author_snapshot")
            .unwrap()
            .get_str("name")
            .unwrap(),
        "Carol"
    );

    // Snapshots cannot be written directly.
    let err = e
        .update(
            "posts",
            None,
            doc! { "$set": { "author_snapshot.name": "x" } },
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidArgument);
}

#[tokio::test]
async fn batches_are_atomic() {
    let Some(t) = engine().await else { return };
    let s = seed(&t.engine).await;
    let e = &t.engine;
    let err = e
        .write(vec![
            WriteOp::Insert {
                collection: "tags".into(),
                documents: vec![doc! { "name": "new" }],
            },
            WriteOp::Insert {
                collection: "posts".into(),
                documents: vec![doc! { "title": "x", "author_id": ObjectId::new() }],
            },
        ])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ForeignKey { .. }));
    assert_eq!(
        e.count("tags", Some(&Expr::eq("name", "new")))
            .await
            .unwrap(),
        0
    );

    let dave = ObjectId::new();
    let r = e
        .write(vec![
            WriteOp::Insert {
                collection: "users".into(),
                documents: vec![doc! { "_id": dave, "email": "d@x.io" }],
            },
            WriteOp::Insert {
                collection: "posts".into(),
                documents: vec![doc! { "title": "y", "author_id": dave }],
            },
            WriteOp::Delete {
                collection: "posts".into(),
                filter: Some(Expr::eq("_id", s.p3)),
                multi: true,
            },
        ])
        .await
        .unwrap();
    assert_eq!(r.inserted_ids.len(), 2);
    assert_eq!(r.deleted, 1);
}

/// Concurrent "insert a post for U" and "delete U" must never leave an orphan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn intent_locks_prevent_orphans_under_concurrency() {
    let Some(t) = engine_with(|c| c.max_txn_attempts = 50).await else {
        return;
    };
    let e = t.engine.clone();
    e.apply_schema(blog_schema(), false).await.unwrap();
    // Restrict instead of cascade makes the race observable as an orphan.
    let mut schema = blog_schema();
    schema.collections[0].relations[0].on_delete =
        Some(mr_engine::mr_core::schema::OnDelete::Restrict);
    e.apply_schema(schema, false).await.unwrap();

    for round in 0..20 {
        let uid = ObjectId::new();
        e.insert(
            "users",
            vec![doc! { "_id": uid, "email": format!("u{round}@x.io") }],
        )
        .await
        .unwrap();
        let (e1, e2) = (e.clone(), e.clone());
        let ins = tokio::spawn(async move {
            e1.insert("posts", vec![doc! { "title": "p", "author_id": uid }])
                .await
        });
        let del = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(round % 3)).await;
            e2.delete("users", Some(Expr::eq("_id", uid))).await
        });
        let (ins, del) = (ins.await.unwrap(), del.await.unwrap());
        let user_exists = e.count("users", Some(&Expr::eq("_id", uid))).await.unwrap() == 1;
        let post_exists = e
            .count("posts", Some(&Expr::eq("author_id", uid)))
            .await
            .unwrap()
            == 1;
        assert!(
            !(post_exists && !user_exists),
            "round {round}: orphan post (ins={ins:?}, del={del:?})"
        );
        assert_eq!(ins.is_ok(), post_exists);
        assert_eq!(del.is_ok(), !user_exists);
    }
}

/// Optional unique fields behave like SQL UNIQUE: many documents may lack a
/// value, duplicates of real values are rejected with a readable error, and
/// re-applying a schema upgrades an index whose definition changed.
#[tokio::test]
async fn optional_unique_fields_allow_missing_values() {
    use mr_engine::mr_core::schema::{CollectionDef, FieldDef, FieldType, Schema};
    let Some(t) = engine().await else { return };
    let e = &t.engine;
    let schema = Schema {
        collections: vec![CollectionDef {
            name: "people".into(),
            fields: vec![FieldDef::new("email", FieldType::String).unique()],
            relations: vec![],
        }],
    };
    // Simulate an index created before partial unique indexes existed.
    e.database()
        .collection::<bson::Document>("people")
        .create_index(
            mongodb::IndexModel::builder()
                .keys(doc! { "email": 1 })
                .options(
                    mongodb::options::IndexOptions::builder()
                        .name("mr_uniq_email".to_string())
                        .unique(true)
                        .build(),
                )
                .build(),
        )
        .await
        .unwrap();
    let r = e.apply_schema(schema, false).await.unwrap();
    assert_eq!(r.dropped_indexes, vec!["people.mr_uniq_email"]);
    assert_eq!(r.created_indexes, vec!["people.mr_uniq_email"]);

    e.insert(
        "people",
        vec![doc! { "n": 1 }, doc! { "n": 2 }, doc! { "email": "a@x.io" }],
    )
    .await
    .unwrap();
    let err = e
        .insert("people", vec![doc! { "email": "a@x.io" }])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::FailedPrecondition);
    assert_eq!(
        err.to_string(),
        "unique constraint violated: `people` already has a document with \
         { email: \"a@x.io\" } (index `mr_uniq_email`)"
    );
}
