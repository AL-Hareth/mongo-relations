# Getting started

This guide takes you from nothing to a working mongo-relations server in about
15 minutes. You will model a small library (authors, books, reviews and
genres), load data, run relational queries, and watch referential integrity
and denormalized copies at work.

Every command and response below comes from a real run against MongoDB 7.0.
Your generated `_id` values will differ.

**Contents**

1. [Prerequisites](#1-prerequisites)
2. [Start MongoDB](#2-start-mongodb)
3. [Build and configure the server](#3-build-and-configure-the-server)
4. [Define a schema](#4-define-a-schema)
5. [Write data](#5-write-data)
6. [Query with relations](#6-query-with-relations)
7. [Update and delete](#7-update-and-delete)
8. [Use the Rust client](#8-use-the-rust-client)
9. [Use other languages (gRPC)](#9-use-other-languages-grpc)
10. [Troubleshooting](#10-troubleshooting)

---

## 1. Prerequisites

- **Rust 1.85 or newer** (`rustup update stable`).
- **MongoDB 5.0 or newer, running as a replica set.** The quickest way to get
  one is Docker, used below.
- **curl**. **jq** is optional; it only pretty-prints responses.

> **Why a replica set?** Strict foreign keys, cascades and multi-operation
> writes run in MongoDB transactions, and embed sync uses change streams. Both
> need a replica set. A single node is enough. A standalone `mongod` works
> only for loose relations and single writes; the server warns at startup.

## 2. Start MongoDB

Start a single-node replica set and initiate it:

```sh
docker run -d --name mongo -p 27017:27017 mongo:7 --replSet rs0 --bind_ip_all

docker exec mongo mongosh --quiet --eval \
  'rs.initiate({_id: "rs0", members: [{_id: 0, host: "127.0.0.1:27017"}]})'
```

Wait a few seconds, then confirm the node is the primary:

```sh
docker exec mongo mongosh --quiet --eval 'db.hello().isWritablePrimary'
# true
```

## 3. Build and configure the server

Clone and build:

```sh
git clone https://github.com/AL-Hareth/mongo-relations
cd mongo-relations
cargo build --release
```

The binary is at `target/release/mongo-relations`.

### Create an API key

Every request is authenticated with an API key. The config file stores only
the key's SHA-256 hash, never the key itself. Choose a key and hash it:

```sh
./target/release/mongo-relations hash-key dev-secret
# 298754db2dbab6ec62605ceb0379eb7ee376580359449efe0caa3aa06cd56736
```

### Write `config.toml`

```toml
[mongodb]
uri = "mongodb://127.0.0.1:27017/?replicaSet=rs0"
database = "library"

[server]
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8080"

[[auth.keys]]
name = "dev"
sha256 = "298754db2dbab6ec62605ceb0379eb7ee376580359449efe0caa3aa06cd56736"
role = "admin"   # read | read_write | admin
```

[`config.example.toml`](../config.example.toml) lists every option. The
connection settings can also be overridden with environment variables
(`MR_MONGODB_URI`, `MR_DATABASE`, `MR_GRPC_ADDR`, `MR_HTTP_ADDR`).

### Start it

```sh
./target/release/mongo-relations --config config.toml check-config
# configuration OK

./target/release/mongo-relations --config config.toml
```

```
INFO mongo_relations: connected mongodb=7.0.45 transactions=true schema_version=0
INFO mongo_relations: listening grpc=127.0.0.1:50051 http=127.0.0.1:8080
```

Leave it running. In a second terminal, check its health and save the auth
header for the rest of the guide:

```sh
curl -s localhost:8080/health
# {"mongodb":{"reachable":true,"transactions":true,"version":"7.0.45"},
#  "schema_version":0,"status":"ok","version":"0.1.0"}

export AUTH="authorization: Bearer dev-secret"
```

`transactions: true` confirms MongoDB is a replica set.

## 4. Define a schema

The schema tells mongo-relations about your collections, their typed fields,
and the relations between them. Here is the model:

```
authors 1 ──< books >── many ── genres
                 │
                 1
                 └──< reviews
```

Save this as `schema.json`:

```json
{
  "schema": {
    "collections": [
      {
        "name": "authors",
        "fields": [
          { "name": "name", "type": "string", "required": true },
          { "name": "country", "type": "string" }
        ],
        "relations": [
          { "name": "books", "kind": "has_many", "target": "books", "foreign_key": "author_id" }
        ]
      },
      {
        "name": "books",
        "fields": [
          { "name": "title", "type": "string", "required": true },
          { "name": "year", "type": "int" },
          { "name": "author_id", "type": "object_id", "required": true },
          { "name": "genre_ids", "type": { "array": "object_id" } }
        ],
        "relations": [
          { "name": "author", "kind": "belongs_to", "target": "authors", "foreign_key": "author_id",
            "on_delete": "restrict",
            "embed": { "fields": ["name"], "into": "author_info" } },
          { "name": "reviews", "kind": "has_many", "target": "reviews", "foreign_key": "book_id",
            "on_delete": "cascade" },
          { "name": "genres", "kind": "many_to_many", "target": "genres",
            "via": { "local": "genre_ids" }, "on_delete": "set_null" }
        ]
      },
      {
        "name": "reviews",
        "fields": [
          { "name": "stars", "type": "int", "required": true },
          { "name": "text", "type": "string" }
        ],
        "relations": [
          { "name": "book", "kind": "belongs_to", "target": "books", "foreign_key": "book_id" }
        ]
      },
      {
        "name": "genres",
        "fields": [ { "name": "name", "type": "string", "required": true, "unique": true } ],
        "relations": [
          { "name": "books", "kind": "many_to_many", "target": "books", "via": { "remote": "genre_ids" } }
        ]
      }
    ]
  }
}
```

### What this says

**Fields.** Each declared field has a type and can be `required`, `nullable`
or `unique`. The types are `object_id`, `string`, `int`, `long`, `double`,
`decimal`, `number`, `bool`, `date`, `binary`, `uuid` and `any`, plus
`{ "array": <type> }` and `{ "object": [<fields>] }`. Fields you don't declare
are allowed and stored untouched.

**Relations.** Each relation has a name, which you use in queries, and a kind:

| Kind | Where the key lives | Here |
|---|---|---|
| `belongs_to` | `foreign_key` on **this** collection | `books.author` → `books.author_id` |
| `has_one` / `has_many` | `foreign_key` on the **target** | `authors.books` → `books.author_id` |
| `many_to_many` | `via.local`: id array here; `via.remote`: id array on the target; `via.junction`: a link collection | `books.genres` → `books.genre_ids` |

`books.author` and `authors.books` describe the **same** foreign key from its
two sides. The server merges them into one constraint. Settings given on only
one side (such as `on_delete` here) apply to both sides, and settings that
conflict are rejected.

**Integrity.** Relations are `strict` by default. Writes must reference rows
that exist, and `on_delete` decides what happens when a referenced row is
deleted:

- `restrict` (the default): refuse the delete.
- `cascade`: delete the referencing rows too.
- `set_null`: set a scalar foreign key to `null`, or remove the key from an
  array foreign key.
- `no_action`: do nothing.

Set `"integrity": "loose"` on a relation to use it only for joins, with no
checks at all.

**Embeds.** `embed` keeps a copy of the author's `name` inside each book, in
`author_info`. Reads then need no join, and the server keeps the copy up to
date.

> **Declare your foreign-key fields.** Declared `object_id` fields accept
> either `{"$oid": "..."}` or a plain 24-character hex string, and store an
> ObjectId either way. An **undeclared** field keeps whatever JSON type you
> send. If `genre_ids` weren't declared, `["6500...0101"]` would be stored as
> a string and wouldn't match the genre's ObjectId, so the insert would fail
> with a foreign-key error.

### Apply it

Preview the changes first with a dry run, which changes nothing:

```sh
jq '.dry_run = true' schema.json | curl -s -X PUT localhost:8080/v1/schema -H "$AUTH" --data @-
```

Then apply for real:

```sh
curl -s -X PUT localhost:8080/v1/schema -H "$AUTH" --data @schema.json
```

```json
{
  "version": 1,
  "changed": true,
  "dry_run": false,
  "created_collections": ["authors", "books", "reviews", "genres"],
  "created_indexes": ["books.mr_fk_author_id", "books.mr_fk_genre_ids",
                      "books.mr_fk_author_info._id", "reviews.mr_fk_book_id",
                      "genres.mr_uniq_name"],
  "dropped_indexes": [],
  "validators_updated": ["authors", "books", "reviews", "genres"]
}
```

The server created the collections, indexed every foreign key and unique
field, and installed a MongoDB `$jsonSchema` validator on each collection.
Applying is idempotent: run the same schema again and you get
`"changed": false`. To change the model later, edit the file and apply it
again; each change creates a new version:

```sh
curl -s localhost:8080/v1/schema/history -H "$AUTH"
# {"versions":[{"applied_at":"2026-10-17T11:41:48.41Z","version":1}]}
```

## 5. Write data

All operations in one `/v1/write` request run **atomically**: either all of
them succeed or none do. Save this as `seed.json`:

```json
{ "ops": [
  { "op": "insert", "collection": "authors", "documents": [
    { "_id": { "$oid": "650000000000000000000001" }, "name": "Ursula K. Le Guin", "country": "US" },
    { "_id": { "$oid": "650000000000000000000002" }, "name": "Terry Pratchett",   "country": "UK" },
    { "_id": { "$oid": "650000000000000000000003" }, "name": "Octavia E. Butler", "country": "US" }
  ] },
  { "op": "insert", "collection": "genres", "documents": [
    { "_id": { "$oid": "650000000000000000000101" }, "name": "science fiction" },
    { "_id": { "$oid": "650000000000000000000102" }, "name": "fantasy" }
  ] },
  { "op": "insert", "collection": "books", "documents": [
    { "_id": { "$oid": "650000000000000000000201" }, "title": "The Left Hand of Darkness", "year": 1969,
      "author_id": "650000000000000000000001", "genre_ids": ["650000000000000000000101"] },
    { "_id": { "$oid": "650000000000000000000202" }, "title": "A Wizard of Earthsea", "year": 1968,
      "author_id": "650000000000000000000001", "genre_ids": ["650000000000000000000102"] },
    { "_id": { "$oid": "650000000000000000000203" }, "title": "Guards! Guards!", "year": 1989,
      "author_id": "650000000000000000000002", "genre_ids": ["650000000000000000000102"],
      "edition": { "binding": "paperback", "pages": 416 } }
  ] },
  { "op": "insert", "collection": "reviews", "documents": [
    { "book_id": { "$oid": "650000000000000000000201" }, "stars": 5, "text": "Changed how I think." },
    { "book_id": { "$oid": "650000000000000000000201" }, "stars": 4, "text": "Slow start, huge payoff." },
    { "book_id": { "$oid": "650000000000000000000203" }, "stars": 5, "text": "Hilarious." }
  ] }
] }
```

```sh
curl -s -X POST localhost:8080/v1/write -H "$AUTH" --data @seed.json
```

```json
{"inserted_ids":[{"$oid":"650000000000000000000001"}, ...], "matched":0, "modified":0, "deleted":0}
```

Documents are written as **relaxed Extended JSON**, MongoDB's JSON format for
BSON types: `{"$oid": ...}` for ObjectIds and `{"$date": ...}` for dates.
Documents without an `_id` get a generated ObjectId.

Here is what MongoDB actually stored for one book:

```js
{
  _id: ObjectId('650000000000000000000203'),
  author_id: ObjectId('650000000000000000000002'),   // hex string coerced to ObjectId
  edition: { binding: 'paperback', pages: 416 },     // undeclared field, kept as-is
  genre_ids: [ ObjectId('650000000000000000000102') ],
  title: 'Guards! Guards!',
  year: 1989,
  author_info: {                                     // embed, filled by the server
    _id: ObjectId('650000000000000000000002'),
    name: 'Terry Pratchett'
  }
}
```

### When writes go wrong

A book that references an author who doesn't exist is rejected with
**409 Conflict**:

```sh
curl -s -X POST localhost:8080/v1/write -H "$AUTH" -d '{ "ops": [
  { "op": "insert", "collection": "books",
    "documents": [ { "title": "Ghost Book", "author_id": "650000000000000000000999" } ] } ] }'
```

```json
{"error":{"kind":"failed_precondition",
  "message":"foreign key violation: `books.author_id` references missing `authors` key(s) [ObjectId(\"650000000000000000000999\")]"}}
```

A document that breaks the declared field types is rejected with
**400 Bad Request**:

```json
{"error":{"kind":"invalid_argument",
  "message":"document validation failed: `title`: is required; `year`: expected int, got string"}}
```

The field types also protect your data from writes that bypass
mongo-relations, because MongoDB itself enforces the `$jsonSchema`
validator:

```sh
docker exec mongo mongosh --quiet library --eval 'db.books.insertOne({title: 42})'
# MongoServerError: Document failed validation
```

## 6. Query with relations

Send queries to `POST /v1/query`. A query names a `collection` and may add a
`filter`, `select`, `include`, `sort`, `skip` and `limit`.

### Joins with `include`

Fetch books with their author and genres:

```sh
curl -s -X POST localhost:8080/v1/query -H "$AUTH" -d '{
  "collection": "books",
  "select": ["title", "year"],
  "include": [
    { "relation": "author", "select": ["name"] },
    { "relation": "genres", "select": ["name"] }
  ],
  "sort": [ { "field": "year" } ]
}'
```

```json
[
  {
    "_id": { "$oid": "650000000000000000000202" },
    "title": "A Wizard of Earthsea",
    "year": 1968,
    "author": { "_id": { "$oid": "650000000000000000000001" }, "name": "Ursula K. Le Guin" },
    "genres": [ { "_id": { "$oid": "650000000000000000000102" }, "name": "fantasy" } ]
  },
  ...
]
```

A to-one relation (`author`) becomes an object, or `null` if there is no
match. A to-many relation (`genres`) becomes an array. `author_id` is
missing from the output because you didn't select it: the server fetched it
to do the join, then removed it. `_id` is always returned.

### Nested includes with a limit per parent

For each author, return their newest book and that book's reviews:

```sh
curl -s -X POST localhost:8080/v1/query -H "$AUTH" -d '{
  "collection": "authors",
  "select": ["name"],
  "include": [ {
    "relation": "books",
    "select": ["title", "year"],
    "sort": [ { "field": "year", "desc": true } ],
    "limit": 1,
    "include": [ { "relation": "reviews", "select": ["stars"] } ]
  } ],
  "sort": [ { "field": "name" } ]
}'
```

```json
{"name":"Octavia E. Butler","books":[]}
{"name":"Terry Pratchett","books":[{"title":"Guards! Guards!","year":1989,"reviews":[{"stars":5}]}]}
{"name":"Ursula K. Le Guin","books":[{"title":"The Left Hand of Darkness","year":1969,"reviews":[{"stars":5},{"stars":4}]}]}
```

(`_id`s are left out here for brevity.) The `limit` applies **per author**,
not to the whole result.

### Inner joins with `required`

By default an include behaves like a left join: parents without a match are
still returned. Add `"required": true` to keep only parents that have a
match, like an SQL inner join. This returns only books with at least one
5-star review:

```sh
curl -s -X POST localhost:8080/v1/query -H "$AUTH" -d '{
  "collection": "books",
  "select": ["title"],
  "include": [ { "relation": "reviews", "required": true,
                 "filter": { "cmp": { "path": "stars", "op": "gte", "value": 5 } },
                 "select": ["text"] } ]
}'
```

```json
{"title":"The Left Hand of Darkness","reviews":[{"text":"Changed how I think."}]}
{"title":"Guards! Guards!","reviews":[{"text":"Hilarious."}]}
```

### Filtering across relations

A filter path can cross a **to-one** relation. This finds books by US
authors:

```sh
curl -s -X POST localhost:8080/v1/query -H "$AUTH" -d '{
  "collection": "books",
  "filter": { "cmp": { "path": "author.country", "op": "eq", "value": "US" } },
  "select": ["title"]
}'
# [{"title":"The Left Hand of Darkness"}, {"title":"A Wizard of Earthsea"}]
```

For **to-many** relations, use `any` (SQL `EXISTS`) or `all`. These can be
nested. This finds authors with at least one book that has a 5-star review:

```sh
curl -s -X POST localhost:8080/v1/query -H "$AUTH" -d '{
  "collection": "authors",
  "filter": { "any": { "relation": "books",
    "filter": { "any": { "relation": "reviews",
      "filter": { "cmp": { "path": "stars", "op": "eq", "value": 5 } } } } } },
  "select": ["name"]
}'
# [{"name":"Ursula K. Le Guin"}, {"name":"Terry Pratchett"}]
```

Authors with **no** books:

```sh
curl -s -X POST localhost:8080/v1/query -H "$AUTH" -d '{
  "collection": "authors",
  "filter": { "is_null": { "path": "books" } },
  "select": ["name"]
}'
# [{"name":"Octavia E. Butler"}]
```

`{"not": {"any": {"relation": "books"}}}` returns the same result.

Books whose reviews are **all** 5 stars:

```sh
curl -s -X POST localhost:8080/v1/query -H "$AUTH" -d '{
  "collection": "books",
  "filter": { "all": { "relation": "reviews",
    "filter": { "cmp": { "path": "stars", "op": "eq", "value": 5 } } } },
  "select": ["title"]
}'
# [{"title":"A Wizard of Earthsea"}, {"title":"Guards! Guards!"}]
```

*A Wizard of Earthsea* has no reviews at all, so "all of its reviews are 5
stars" is trivially true. SQL behaves the same way.

### Filter reference

| Expression | Example |
|---|---|
| Comparison | `{"cmp": {"path": "year", "op": "gte", "value": 1970}}` — `op` is one of `eq ne gt gte lt lte` |
| Set membership | `{"in": {"path": "country", "values": ["US", "UK"]}}`, and the same shape with `nin` |
| Presence | `{"exists": {"path": "edition", "exists": true}}`, `{"is_null": {"path": "year"}}` |
| Pattern | `{"regex": {"path": "title", "pattern": "^the", "options": "i"}}` |
| Relations | `{"any": {"relation": "reviews", "filter": ...}}`, `{"all": {"relation": "reviews", "filter": ...}}` |
| Logic | `{"and": [...]}`, `{"or": [...]}`, `{"not": ...}` |
| Raw MongoDB | `{"raw": {"edition.pages": {"$gt": 400}}}` — for document-specific operators |

### Count, stream and explain

Count books by US authors:

```sh
curl -s -X POST localhost:8080/v1/count -H "$AUTH" -d '{
  "collection": "books",
  "filter": { "cmp": { "path": "author.country", "op": "eq", "value": "US" } } }'
# {"count":2}
```

For large results, ask for **NDJSON** (one document per line), which streams
results as they are fetched:

```sh
curl -s -X POST localhost:8080/v1/query -H "$AUTH" -H 'accept: application/x-ndjson' \
  -d '{ "collection": "genres", "select": ["name"] }'
# {"_id":{"$oid":"650000000000000000000101"},"name":"science fiction"}
# {"_id":{"$oid":"650000000000000000000102"},"name":"fantasy"}
```

`/v1/explain` shows the plan the planner chose and the MongoDB pipeline it
generated:

```sh
curl -s -X POST localhost:8080/v1/explain -H "$AUTH" -d '{
  "collection": "books",
  "filter": { "cmp": { "path": "author.country", "op": "eq", "value": "US" } },
  "include": [ { "relation": "reviews" } ],
  "limit": 10 }'
```

```json
{
  "plan": {
    "filter": { "exists": "author", "target": "authors",
                "strategy": "semi_join(max_keys=10000)", "where": { "country": { "$eq": "US" } } },
    "include": [ { "relation": "reviews", "strategy": "lookup", "hops": 1 } ]
  },
  "pipeline": [
    { "$match": { "author_id": { "$in": [ { "$oid": "...0003" }, { "$oid": "...0001" } ] } } },
    { "$limit": 10 },
    { "$lookup": { "from": "reviews", "localField": "_id", "foreignField": "book_id", ... } }
  ]
}
```

The `author.country` filter was resolved first, into a plain indexed
`author_id $in [...]` match (a **semi-join**). For a small page of results,
the reviews join runs as a `$lookup` in the same pipeline. For larger result
sets, the planner switches to **batch** strategies: one extra `$in` query per
join, never one query per row. To force a strategy for one include, add
`"strategy": "lookup"` or `"strategy": "batch"` to it.

## 7. Update and delete

### Updates

Updates use MongoDB's update operators (`$set`, `$inc`, `$push`, ...), and
their `filter` takes the same expressions as queries:

```sh
curl -s -X POST localhost:8080/v1/write -H "$AUTH" -d '{ "ops": [ {
  "op": "update", "collection": "authors",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": { "$oid": "650000000000000000000002" } } },
  "update": { "$set": { "name": "Sir Terry Pratchett" } } } ] }'
# {"matched":1,"modified":1,"deleted":0}
```

Within about a second, the copy embedded in his book has followed:

```js
db.books.findOne({ title: "Guards! Guards!" }, { author_info: 1 })
// { _id: ..., author_info: { _id: ObjectId('650000000000000000000002'), name: 'Sir Terry Pratchett' } }
```

A background worker watches MongoDB change streams and updates the copies.
It also catches changes made directly in MongoDB, not only those made
through mongo-relations. Copies are therefore *eventually* consistent, and
are refreshed immediately when a write changes a book's `author_id`. If the
worker was offline for a long time, `POST /v1/admin/rebuild-embeds`
recomputes every copy.

Foreign keys are checked on updates too, and the managed copy can't be
written directly:

```json
// $set author_id to a missing author → 409
{"error":{"kind":"failed_precondition","message":"foreign key violation: `books.author_id` references missing `authors` key(s) [...]"}}

// $set author_info.name → 400
{"error":{"kind":"invalid_argument","message":"invalid write: `author_info` is maintained by relation `author` and cannot be written"}}
```

### Deletes and `on_delete`

**`restrict`**: Le Guin still has books, so she can't be deleted.

```sh
curl -s -X POST localhost:8080/v1/write -H "$AUTH" -d '{ "ops": [ {
  "op": "delete", "collection": "authors",
  "filter": { "cmp": { "path": "name", "op": "eq", "value": "Ursula K. Le Guin" } } } ] }'
```

```json
{"error":{"kind":"failed_precondition",
  "message":"cannot delete `authors` document(s): still referenced by `books.author_id` (on_delete: restrict)"}}
```

**`cascade`**: deleting a book deletes its reviews.

```sh
curl -s -X POST localhost:8080/v1/write -H "$AUTH" -d '{ "ops": [ {
  "op": "delete", "collection": "books",
  "filter": { "cmp": { "path": "title", "op": "eq", "value": "The Left Hand of Darkness" } } } ] }'
# {"deleted":1,"cascaded":[{"collection":"reviews","deleted":2,"nulled":0}], ...}
```

**`set_null`**: deleting a genre removes it from every book's `genre_ids`.

```sh
curl -s -X POST localhost:8080/v1/write -H "$AUTH" -d '{ "ops": [ {
  "op": "delete", "collection": "genres",
  "filter": { "cmp": { "path": "name", "op": "eq", "value": "fantasy" } } } ] }'
# {"deleted":1,"cascaded":[{"collection":"books","deleted":0,"nulled":2}], ...}
```

Each of these runs in a single transaction. If any step fails, nothing
changes. Concurrent writes can't sneak an orphan in either: for example, a
book can't be inserted for an author who is being deleted at the same moment.

## 8. Use the Rust client

The `mr-client` crate talks gRPC and offers a typed builder API. It isn't on
crates.io yet, so add it from git:

```toml
[dependencies]
mr-client = { git = "https://github.com/AL-Hareth/mongo-relations" }
bson = "3"
tokio = { version = "1", features = ["full"] }
```

```rust
use mr_client::{Client, any, field, has, rel};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::connect("http://127.0.0.1:50051")
        .await?
        .with_api_key("dev-secret");

    // Books by UK authors, with the author's name.
    let books = client
        .query("books")
        .filter(field("author.country").eq("UK"))
        .include(rel("author").select(["name"]))
        .select(["title", "year"])
        .sort_asc("year")
        .fetch_all()
        .await?;
    for b in &books {
        println!("{} ({})", b.get_str("title")?, b.get_document("author")?.get_str("name")?);
    }

    // Authors with at least one book, plus their newest book.
    let authors = client
        .query("authors")
        .filter(has("books"))
        .include(rel("books").sort_desc("year").limit(1))
        .fetch_all()
        .await?;
    println!("{} author(s) with books", authors.len());

    // Filters combine with `&`, `|` and `!`.
    let n = client
        .count("books", Some(any("genres", field("name").eq("fantasy")) & !field("year").lt(1970)))
        .await?;
    println!("{n} matching book(s)");

    // Errors carry the same machine-readable kind as the HTTP API.
    let err = client
        .insert("books", vec![bson::doc! { "title": "Ghost", "author_id": bson::oid::ObjectId::new() }])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), Some("failed_precondition"));
    Ok(())
}
```

```
Guards! Guards! (Sir Terry Pratchett)
2 author(s) with books
...
```

Results arrive as `bson::Document`s. Use `.stream()` instead of
`.fetch_all()` to process large results one document at a time, and
`.first()`, `.count()` or `.explain()` for the other operations.

## 9. Use other languages (gRPC)

The gRPC API is defined in
[`crates/mr-proto/proto/mongo_relations/v1/relations.proto`](../crates/mr-proto/proto/mongo_relations/v1/relations.proto).
Generate a client with your language's protobuf tooling. Two conventions to
know:

- **Documents are raw BSON bytes.** This applies to inserted documents,
  result rows, update specs and raw filters. Use your MongoDB driver's BSON
  library to encode and decode them.
- **Single values are wrapped** in a BSON document `{ "v": <value> }`,
  because BSON can't encode a bare value. This applies to comparison values,
  `in` lists and inserted ids.

Authenticate with the metadata header `authorization: Bearer <key>`. Error
responses carry the error class in the `mr-error-kind` metadata key. If you
don't need gRPC, the HTTP API from this guide works from any language.

## 10. Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `transactions: false` in `/health`, or writes fail with `unsupported: this write needs a transaction` | MongoDB isn't a replica set. Start it with `--replSet` and run `rs.initiate(...)` ([step 2](#2-start-mongodb)). |
| `401 unauthenticated` | The `authorization: Bearer <key>` header is missing, or the key doesn't match any configured hash. Re-run `hash-key`. |
| `403 permission_denied` | The key's role is too low. Writes need `read_write`; schema changes need `admin`. |
| Foreign-key error although the target exists | Usually a type mismatch: the key was sent as a string but the target `_id` is an ObjectId. Declare the field as `object_id` (or `{"array": "object_id"}`), or send `{"$oid": "..."}`. |
| `relation ... references ..., which must be _id or a declared unique scalar field` | A relation's `references` must point at `_id` or at a field declared `"unique": true`. |
| `conflicting on_delete` when applying a schema | Both sides of a relation set different values. Set it on one side only, or make them match. |
| Embedded copies are stale | The sync worker was offline. Run `curl -X POST localhost:8080/v1/admin/rebuild-embeds -H "$AUTH"`. |
| `Document failed validation` from a direct MongoDB write | Working as intended: the `$jsonSchema` validator protects declared fields. |

Log verbosity follows `RUST_LOG`, for example
`RUST_LOG=mr_engine=debug,info`.

## Next steps

- [README](../README.md) has the full HTTP endpoint table and semantics.
- [ARCHITECTURE.md](ARCHITECTURE.md) explains how the planner, integrity
  enforcement, intent locks and embed sync work, and records the design
  decisions.
- Try a junction collection for many-to-many relations that carry their own
  data, such as enrollments with a grade:
  `"via": { "junction": { "collection": "enrollments", "local_key": "student_id", "target_key": "course_id" } }`.
