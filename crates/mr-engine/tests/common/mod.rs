#![allow(dead_code)]

use bson::{Document, doc, oid::ObjectId};
use mr_engine::mr_core::query::Query;
use mr_engine::mr_core::schema::*;
use mr_engine::{Engine, EngineConfig};

/// Connect to the test deployment (`MR_TEST_MONGODB_URI`, a replica set) with
/// a fresh database, or `None` to skip when it is not configured.
pub async fn engine() -> Option<TestDb> {
    engine_with(|_| {}).await
}

pub async fn engine_with(f: impl FnOnce(&mut EngineConfig)) -> Option<TestDb> {
    let Ok(uri) = std::env::var("MR_TEST_MONGODB_URI") else {
        eprintln!("MR_TEST_MONGODB_URI not set; skipping");
        return None;
    };
    let name = format!("mr_test_{}", ObjectId::new().to_hex());
    let mut cfg = EngineConfig::new(&name);
    f(&mut cfg);
    let engine = Engine::connect(&uri, cfg).await.expect("connect");
    Some(TestDb { engine, name, uri })
}

pub struct TestDb {
    pub engine: Engine,
    pub name: String,
    uri: String,
}

impl Drop for TestDb {
    fn drop(&mut self) {
        // Drop the test database from a separate runtime (the test's runtime
        // may be shutting down).
        let (uri, name) = (self.uri.clone(), self.name.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Ok(c) = mongodb::Client::with_uri_str(&uri).await {
                    let _ = c.database(&name).drop().await;
                }
            });
        })
        .join();
    }
}

pub fn blog_schema() -> Schema {
    Schema {
        collections: vec![
            CollectionDef {
                name: "users".into(),
                fields: vec![
                    FieldDef::new("email", FieldType::String)
                        .required()
                        .unique(),
                    FieldDef::new("name", FieldType::String),
                    FieldDef::new("country", FieldType::String),
                ],
                relations: vec![
                    RelationDef::has_many("posts", "posts", "author_id")
                        .on_delete(OnDelete::Cascade),
                    RelationDef::has_one("profile", "profiles", "user_id"),
                    RelationDef::has_many("comments", "comments", "user_id")
                        .on_delete(OnDelete::SetNull),
                ],
            },
            CollectionDef {
                name: "profiles".into(),
                fields: vec![FieldDef::new("bio", FieldType::String)],
                relations: vec![
                    RelationDef::belongs_to("user", "users", "user_id")
                        .on_delete(OnDelete::Cascade),
                ],
            },
            CollectionDef {
                name: "posts".into(),
                fields: vec![
                    FieldDef::new("title", FieldType::String).required(),
                    FieldDef::new("likes", FieldType::Long),
                    FieldDef::new("author_id", FieldType::ObjectId),
                ],
                relations: vec![
                    RelationDef::belongs_to("author", "users", "author_id")
                        .embed(&["name"], "author_snapshot"),
                    RelationDef::has_many("comments", "comments", "post_id")
                        .on_delete(OnDelete::Cascade),
                    RelationDef::many_to_many("tags", "tags", Via::Local("tag_ids".into()))
                        .on_delete(OnDelete::SetNull)
                        .embed(&["name"], "tag_snapshots"),
                ],
            },
            CollectionDef {
                name: "comments".into(),
                fields: vec![
                    FieldDef::new("body", FieldType::String),
                    FieldDef::new("user_id", FieldType::ObjectId).nullable(),
                ],
                relations: vec![
                    RelationDef::belongs_to("post", "posts", "post_id"),
                    RelationDef::belongs_to("user", "users", "user_id"),
                ],
            },
            CollectionDef {
                name: "tags".into(),
                fields: vec![FieldDef::new("name", FieldType::String).required().unique()],
                relations: vec![RelationDef::many_to_many(
                    "posts",
                    "posts",
                    Via::Remote("tag_ids".into()),
                )],
            },
            CollectionDef {
                name: "categories".into(),
                fields: vec![FieldDef::new("name", FieldType::String)],
                relations: vec![
                    RelationDef::belongs_to("parent", "categories", "parent_id"),
                    RelationDef::has_many("children", "categories", "parent_id"),
                ],
            },
            CollectionDef {
                name: "students".into(),
                fields: vec![FieldDef::new("name", FieldType::String)],
                relations: vec![RelationDef::many_to_many(
                    "courses",
                    "courses",
                    Via::Junction(Junction {
                        collection: "enrollments".into(),
                        local_key: "student_id".into(),
                        target_key: "course_id".into(),
                    }),
                )],
            },
            CollectionDef {
                name: "courses".into(),
                fields: vec![FieldDef::new("title", FieldType::String)],
                relations: vec![RelationDef::many_to_many(
                    "students",
                    "students",
                    Via::Junction(Junction {
                        collection: "enrollments".into(),
                        local_key: "course_id".into(),
                        target_key: "student_id".into(),
                    }),
                )],
            },
            CollectionDef::new("enrollments"),
        ],
    }
}

pub struct Seed {
    pub alice: ObjectId,
    pub bob: ObjectId,
    pub carol: ObjectId,
    pub rust: ObjectId,
    pub mongo: ObjectId,
    pub p1: ObjectId,
    pub p2: ObjectId,
    pub p3: ObjectId,
}

/// Seed a small blog dataset.
pub async fn seed(e: &Engine) -> Seed {
    e.apply_schema(blog_schema(), false)
        .await
        .expect("apply schema");
    let (alice, bob, carol) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
    e.insert(
        "users",
        vec![
            doc! { "_id": alice, "email": "alice@x.io", "name": "Alice", "country": "US" },
            doc! { "_id": bob, "email": "bob@x.io", "name": "Bob", "country": "DE" },
            doc! { "_id": carol, "email": "carol@x.io", "name": "Carol", "country": "US" },
        ],
    )
    .await
    .unwrap();
    e.insert(
        "profiles",
        vec![doc! { "user_id": alice, "bio": "rustacean" }],
    )
    .await
    .unwrap();
    let (rust, mongo) = (ObjectId::new(), ObjectId::new());
    e.insert(
        "tags",
        vec![
            doc! { "_id": rust, "name": "rust" },
            doc! { "_id": mongo, "name": "mongo" },
        ],
    )
    .await
    .unwrap();
    let (p1, p2, p3) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
    e.insert(
        "posts",
        vec![
            doc! { "_id": p1, "title": "Ownership", "author_id": alice, "likes": 10, "tag_ids": [rust] },
            doc! { "_id": p2, "title": "Aggregation", "author_id": alice, "likes": 5, "tag_ids": [mongo, rust] },
            doc! { "_id": p3, "title": "Hello", "author_id": bob, "likes": 1 },
        ],
    )
    .await
    .unwrap();
    e.insert(
        "comments",
        vec![
            doc! { "post_id": p1, "user_id": bob, "body": "nice", "n": 1 },
            doc! { "post_id": p1, "user_id": carol, "body": "great", "n": 2 },
            doc! { "post_id": p1, "user_id": bob, "body": "again", "n": 3 },
            doc! { "post_id": p2, "user_id": carol, "body": "ok", "n": 4 },
        ],
    )
    .await
    .unwrap();
    Seed {
        alice,
        bob,
        carol,
        rust,
        mongo,
        p1,
        p2,
        p3,
    }
}

pub fn titles(docs: &[Document]) -> Vec<String> {
    docs.iter()
        .map(|d| d.get_str("title").unwrap().to_string())
        .collect()
}

pub fn q(collection: &str) -> Query {
    Query::new(collection)
}
