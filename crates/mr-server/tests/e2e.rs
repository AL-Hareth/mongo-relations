//! End-to-end: boot the server and exercise both front-ends.
//! Requires `MR_TEST_MONGODB_URI` (a replica set); skipped otherwise.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bson::{doc, oid::ObjectId};
use http_body_util::BodyExt;
use mr_client::schema::*;
use mr_client::{Client, field, has, rel};
use mr_engine::Engine;
use mr_server::auth::{Role, hash_key};
use mr_server::config::{ApiKey, AuthConfig, Config, EngineTuning, MongoConfig, ServerConfig};
use mr_server::{AppState, Server, http};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Fixture {
    state: AppState,
    grpc: String,
    db: String,
    uri: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let (uri, db) = (self.uri.clone(), self.db.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Ok(c) = mongodb::Client::with_uri_str(&uri).await {
                    let _ = c.database(&db).drop().await;
                }
            });
        })
        .join();
    }
}

async fn fixture() -> Option<Fixture> {
    let Ok(uri) = std::env::var("MR_TEST_MONGODB_URI") else {
        eprintln!("MR_TEST_MONGODB_URI not set; skipping");
        return None;
    };
    let db = format!("mr_e2e_{}", ObjectId::new().to_hex());
    let config = Config {
        mongodb: MongoConfig {
            uri: uri.clone(),
            database: db.clone(),
            metadata_database: None,
        },
        server: ServerConfig::default(),
        auth: AuthConfig {
            disabled: false,
            keys: vec![
                ApiKey {
                    name: "admin".into(),
                    sha256: hash_key("admin-key"),
                    role: Role::Admin,
                },
                ApiKey {
                    name: "app".into(),
                    sha256: hash_key("app-key"),
                    role: Role::ReadWrite,
                },
                ApiKey {
                    name: "ro".into(),
                    sha256: hash_key("ro-key"),
                    role: Role::Read,
                },
            ],
        },
        engine: EngineTuning::default(),
    };
    let engine = Engine::connect(&uri, config.engine_config()).await.unwrap();
    let state = AppState::new(engine, &config);
    let server = Server::bind(
        state.clone(),
        "127.0.0.1:0".parse().unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let grpc = format!("http://{}", server.grpc_addr());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(server.serve(async {
        let _ = rx.await;
    }));
    Some(Fixture {
        state,
        grpc,
        db,
        uri,
        _shutdown: tx,
    })
}

fn schema() -> Schema {
    Schema {
        collections: vec![
            CollectionDef {
                name: "authors".into(),
                fields: vec![FieldDef::new("name", FieldType::String).required()],
                relations: vec![
                    RelationDef::has_many("books", "books", "author_id")
                        .on_delete(OnDelete::Cascade),
                ],
            },
            CollectionDef {
                name: "books".into(),
                fields: vec![
                    FieldDef::new("title", FieldType::String).required(),
                    FieldDef::new("year", FieldType::Int),
                    FieldDef::new("author_id", FieldType::ObjectId),
                ],
                relations: vec![RelationDef::belongs_to("author", "authors", "author_id")],
            },
        ],
    }
}

#[tokio::test]
async fn grpc_end_to_end() {
    let Some(f) = fixture().await else { return };
    let anon = Client::connect(&f.grpc).await.unwrap();
    let ro = anon.clone().with_api_key("ro-key");
    let app = anon.clone().with_api_key("app-key");
    let admin = anon.clone().with_api_key("admin-key");

    let err = anon.server_info().await.unwrap_err();
    assert_eq!(err.kind(), Some("unauthenticated"));
    let err = app.apply_schema(&schema(), false).await.unwrap_err();
    assert_eq!(err.kind(), Some("permission_denied"));

    let rep = admin.apply_schema(&schema(), false).await.unwrap();
    assert_eq!(rep.version, 1);
    assert_eq!(ro.schema().await.unwrap(), (1, schema()));
    assert!(ro.server_info().await.unwrap().transactions);

    let le_guin = ObjectId::new();
    let err = ro
        .insert("authors", vec![doc! { "name": "x" }])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some("permission_denied"));
    app.insert(
        "authors",
        vec![
            doc! { "_id": le_guin, "name": "Ursula K. Le Guin" },
            doc! { "name": "Nobody" },
        ],
    )
    .await
    .unwrap();
    app.insert(
        "books",
        vec![
            doc! { "title": "A Wizard of Earthsea", "year": 1968, "author_id": le_guin },
            doc! { "title": "The Dispossessed", "year": 1974, "author_id": le_guin },
        ],
    )
    .await
    .unwrap();

    let err = app
        .insert(
            "books",
            vec![doc! { "title": "?", "author_id": ObjectId::new() }],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some("failed_precondition"), "{err}");
    let err = app
        .insert("books", vec![doc! { "year": 1 }])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some("invalid_argument"), "{err}");

    let authors = ro
        .query("authors")
        .filter(has("books"))
        .include(rel("books").sort_desc("year").limit(1).select(["title"]))
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(authors.len(), 1);
    let latest = authors[0].get_array("books").unwrap()[0]
        .as_document()
        .unwrap();
    assert_eq!(latest.get_str("title").unwrap(), "The Dispossessed");

    let n = ro
        .query("books")
        .filter(field("author.name").regex("^Ursula") & field("year").lt(1970))
        .count()
        .await
        .unwrap();
    assert_eq!(n, 1);
    let first = ro
        .query("books")
        .sort_asc("year")
        .include(rel("author"))
        .first()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first
            .get_document("author")
            .unwrap()
            .get_str("name")
            .unwrap(),
        "Ursula K. Le Guin"
    );
    let plan = ro
        .query("books")
        .filter(field("author.name").eq("x"))
        .explain()
        .await
        .unwrap();
    assert!(plan.contains_key("pipeline"));

    let r = app
        .delete("authors", Some(field("_id").eq(le_guin)))
        .await
        .unwrap();
    assert_eq!(r.cascaded[0].deleted, 2);
    assert_eq!(ro.count("books", None).await.unwrap(), 0);
}

async fn call(
    f: &Fixture,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        req = req.header(header::AUTHORIZATION, format!("Bearer {k}"));
    }
    let body = match body {
        Some(b) => Body::from(serde_json::to_vec(&b).unwrap()),
        None => Body::empty(),
    };
    let res = http::router(f.state.clone())
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

#[tokio::test]
async fn http_end_to_end() {
    let Some(f) = fixture().await else { return };

    let (s, v) = call(&f, "GET", "/health", None, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["mongodb"]["transactions"], true);

    let (s, _) = call(&f, "GET", "/v1/schema", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let schema_json = serde_json::to_value(schema()).unwrap();
    let (s, v) = call(
        &f,
        "PUT",
        "/v1/schema",
        Some("admin-key"),
        Some(json!({ "schema": schema_json, "dry_run": true })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["dry_run"], true);
    let (s, v) = call(
        &f,
        "PUT",
        "/v1/schema",
        Some("admin-key"),
        Some(json!({ "schema": schema_json })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["version"], 1);

    let id = ObjectId::new().to_hex();
    let ops = json!({ "ops": [
        { "op": "insert", "collection": "authors", "documents": [ { "_id": { "$oid": id }, "name": "Octavia Butler" } ] },
        { "op": "insert", "collection": "books", "documents": [
            { "title": "Kindred", "year": 1979, "author_id": { "$oid": id } },
            { "title": "Parable of the Sower", "year": 1993, "author_id": id }
        ] }
    ]});
    let (s, v) = call(&f, "POST", "/v1/write", Some("app-key"), Some(ops)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["inserted_ids"].as_array().unwrap().len(), 3);

    let q = json!({
        "collection": "books",
        "filter": { "cmp": { "path": "author.name", "op": "eq", "value": "Octavia Butler" } },
        "include": [ { "relation": "author", "select": ["name"] } ],
        "sort": [ { "field": "year" } ],
        "select": ["title"]
    });
    let (s, v) = call(&f, "POST", "/v1/query", Some("ro-key"), Some(q)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["title"], "Kindred");
    assert_eq!(rows[0]["author"]["name"], "Octavia Butler");
    assert_eq!(
        rows[0]["_id"]["$oid"].as_str().unwrap().len(),
        24,
        "ids are relaxed EJSON"
    );
    assert!(
        rows[0].get("author_id").is_none(),
        "join key stripped by select"
    );

    let (s, v) = call(
        &f,
        "POST",
        "/v1/write",
        Some("app-key"),
        Some(json!({ "ops": [ { "op": "insert", "collection": "books",
            "documents": [ { "title": "x", "author_id": { "$oid": ObjectId::new().to_hex() } } ] } ] })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["kind"], "failed_precondition");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("foreign key")
    );

    let (s, v) = call(
        &f,
        "POST",
        "/v1/query",
        Some("ro-key"),
        Some(json!({ "nope": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    let (s, v) = call(
        &f,
        "POST",
        "/v1/count",
        Some("ro-key"),
        Some(json!({ "collection": "books" })),
    )
    .await;
    assert_eq!((s, v["count"].as_u64()), (StatusCode::OK, Some(2)));

    // NDJSON streaming.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/query")
        .header(header::AUTHORIZATION, "Bearer ro-key")
        .header(header::ACCEPT, "application/x-ndjson")
        .body(Body::from(
            serde_json::to_vec(&json!({ "collection": "books" })).unwrap(),
        ))
        .unwrap();
    let res = http::router(f.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/x-ndjson");
    let text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert_eq!(text.lines().count(), 2);
}
