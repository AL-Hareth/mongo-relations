# mongo-relations

SQL-style relations on top of MongoDB — foreign keys, joins, referential
integrity and cascades — without giving up document flexibility.

`mongo-relations` is a standalone server written in Rust that sits in front of
MongoDB. You declare collections and relations once; the server then plans
joins, enforces integrity transactionally, and keeps denormalized copies in
sync, while undeclared fields stay completely free-form.

```
 clients ──gRPC (BSON)──┐
                        ├──▶ mongo-relations ──▶ MongoDB (replica set)
 curl ──HTTP (EJSON)────┘    planner · integrity · embed sync
```

## Features

- **Relations**: `belongs_to`, `has_one`, `has_many` and `many_to_many`. Many-to-many can be stored as an id array on either side or in a junction collection.
- **Joins (includes)**: nested, with a per-parent filter, sort, limit and select, and optional inner-join semantics (`required`).
- **Relational filters**: paths across to-one relations (`author.country = "US"`) and `any`/`all` quantifiers over to-many relations (SQL `EXISTS`).
- **Cost-based planner**: per join it chooses between a `$lookup` in the pipeline and a batched `$in` fetch (no N+1). Relational filters use an adaptive semi-join and fall back to `$lookup` when the key set is large. `explain` shows the plan it chose.
- **Integrity, set per relation (`strict` or `loose`)**: FK existence checks plus `on_delete` (`restrict`, `cascade`, `set_null`, `no_action`), all inside a MongoDB transaction. Concurrent writes can't leave orphans: the intent locks are tested to catch exactly that race.
- **Denormalized embeds**: a relation can keep a copy of selected target fields on each document. A change-stream worker keeps the copies up to date, including after writes that bypass the server.
- **Flexible documents**: declared fields are typed, coerced and validated. MongoDB itself also enforces them through a generated `$jsonSchema`. Undeclared fields pass through untouched.
- **Two protocols, one server**: gRPC with raw BSON documents, and HTTP/JSON with relaxed Extended JSON (NDJSON streaming optional).
- **API-key auth** with three roles: `read`, `read_write` and `admin`.

## Quick start

Requirements: Rust 1.85+ and MongoDB 5.0+ running as a replica set. A
single-node replica set works fine for development:

```sh
docker run -d --name mongo -p 27017:27017 mongo:7 --replSet rs0 --bind_ip_all
docker exec mongo mongosh --eval 'rs.initiate({_id:"rs0",members:[{_id:0,host:"127.0.0.1:27017"}]})'

cp config.example.toml config.toml          # edit uri/database; set your key hash:
cargo run --bin mongo-relations -- hash-key my-secret
cargo run --release --bin mongo-relations -- --config config.toml
```

### Declare a schema

```sh
curl -X PUT localhost:8080/v1/schema -H 'authorization: Bearer my-secret' -d '{
  "schema": { "collections": [
    { "name": "users",
      "fields": [ { "name": "email", "type": "string", "required": true, "unique": true } ],
      "relations": [ { "name": "posts", "kind": "has_many", "target": "posts",
                       "foreign_key": "author_id", "on_delete": "cascade" } ] },
    { "name": "posts",
      "fields": [ { "name": "title", "type": "string", "required": true },
                  { "name": "author_id", "type": "object_id" } ],
      "relations": [
        { "name": "author", "kind": "belongs_to", "target": "users", "foreign_key": "author_id",
          "embed": { "fields": ["email"], "into": "author_snapshot" } },
        { "name": "tags", "kind": "many_to_many", "target": "tags", "via": { "local": "tag_ids" },
          "on_delete": "set_null" } ] },
    { "name": "tags", "fields": [ { "name": "name", "type": "string", "unique": true } ] }
  ] } }'
```

Applying is declarative and idempotent. Each change creates a new schema
version (see `GET /v1/schema/history`). Both sides of a relation may be
declared; they describe the same foreign key and must agree.

### Query with joins (HTTP)

```sh
curl -X POST localhost:8080/v1/query -H 'authorization: Bearer my-secret' -d '{
  "collection": "posts",
  "filter": { "and": [
    { "cmp": { "path": "author.email", "op": "eq", "value": "ada@example.com" } },
    { "any": { "relation": "tags", "filter": { "cmp": { "path": "name", "op": "eq", "value": "rust" } } } }
  ] },
  "include": [ { "relation": "author", "select": ["email"] },
               { "relation": "tags", "sort": [ { "field": "name" } ] } ],
  "sort": [ { "field": "_id", "desc": true } ],
  "limit": 20
}'
```

### Query with joins (Rust client)

```rust
use mr_client::{Client, any, field, rel};

let client = Client::connect("http://127.0.0.1:50051").await?.with_api_key("my-secret");
let posts = client
    .query("posts")
    .filter(field("author.email").eq("ada@example.com") & any("tags", field("name").eq("rust")))
    .include(rel("author").select(["email"]))
    .include(rel("tags").sort_asc("name"))
    .sort_desc("_id")
    .limit(20)
    .fetch_all()
    .await?;
```

### Writes

```sh
curl -X POST localhost:8080/v1/write -H 'authorization: Bearer my-secret' -d '{ "ops": [
  { "op": "insert", "collection": "users", "documents": [ { "_id": { "$oid": "65a1b2c3d4e5f60718293a4b" }, "email": "ada@example.com" } ] },
  { "op": "insert", "collection": "posts", "documents": [ { "title": "Hello", "author_id": "65a1b2c3d4e5f60718293a4b" } ] }
] }'
```

All operations in one request are atomic. Inserting a post with an unknown
`author_id` returns `409` with `kind: failed_precondition`. Deleting the user
cascades to their posts, and the response reports the cascaded counts.

## HTTP API

| Method | Path                       | Role         | Body / result |
|--------|----------------------------|--------------|---------------|
| GET    | `/health`                  | —            | MongoDB reachability, schema version |
| POST   | `/v1/query`                | read         | `Query` → JSON array (or NDJSON with `Accept: application/x-ndjson`) |
| POST   | `/v1/count`                | read         | `{collection, filter?}` → `{count}` |
| POST   | `/v1/explain`              | read         | `Query` → logical plan + pipeline |
| POST   | `/v1/write`                | read_write   | `{ops: [...]}` → counts, inserted ids, cascades |
| GET    | `/v1/schema`               | read         | current schema + version |
| PUT    | `/v1/schema`               | admin        | `{schema, dry_run?}` → apply report |
| GET    | `/v1/schema/history`       | read         | stored versions |
| POST   | `/v1/admin/rebuild-embeds` | admin        | recompute all snapshots |

The gRPC API is defined in [`crates/mr-proto/proto/mongo_relations/v1/relations.proto`](crates/mr-proto/proto/mongo_relations/v1/relations.proto).

## Semantics worth knowing

- **Filters across relations** use SQL inner-join semantics:
  `author.country != "US"` matches posts whose author exists and is not in
  the US. `all(rel, f)` is vacuously true when there are no related
  documents.
- **Strict relations** need transactions, so they need a replica set. Their
  guarantees cover writes made through the server. Direct writes are still
  type-checked by the `$jsonSchema` validator, but FK existence is not
  checked for them.
- **`ON UPDATE`** is always `RESTRICT` for strict relations whose referenced
  key is not `_id`. (`_id` is immutable in MongoDB anyway.)
- **Embeds** are eventually consistent: they are filled synchronously on
  insert and FK change, and the change-stream worker propagates target
  changes afterwards. `rebuild-embeds` repairs everything, for example after
  the worker was down longer than the oplog window.
- Operators that execute server-side JavaScript (`$where`, `$function`,
  `$accumulator`) are rejected in raw filters.

## Project layout

```
crates/mr-core    pure logic: schema catalog, query IR, planner, compiler, validation
crates/mr-engine  MongoDB I/O: catalog store, executor, integrity writes, locks, embed sync
crates/mr-proto   protobuf definitions + conversions
crates/mr-server  the binary: config, auth, gRPC + HTTP front-ends
crates/mr-client  Rust client with the typed builder API
docs/ARCHITECTURE.md  design and decision log
```

## Development

```sh
cargo test                                   # unit tests; integration tests skip without MongoDB
MR_TEST_MONGODB_URI="mongodb://127.0.0.1:27017/?replicaSet=rs0" cargo test   # everything
cargo clippy --all-targets && cargo fmt --check
```

## Roadmap

- SQL front-end compiled to the same query IR
- Aggregations (group by, relation counts) and sorting by related fields
- Per-collection permissions; client SDKs for other languages
- Online integrity audit for data written outside the server

## License

Apache-2.0
