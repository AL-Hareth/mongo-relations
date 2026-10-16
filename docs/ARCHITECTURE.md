# mongo-relations — Architecture

mongo-relations is a standalone server that sits in front of MongoDB and adds
SQL-style relations (foreign keys, joins, referential integrity, cascades) on
top of collections, while every collection keeps its document flexibility.

This document records the architectural decisions agreed for the project and
the design that follows from them. Decisions marked **[D#]** were chosen by
the project owner; changing one should be discussed first.

## Decision log

| #   | Topic                    | Decision |
|-----|--------------------------|----------|
| D1  | Form factor              | Standalone server/proxy |
| D2  | Schema location          | Stored in MongoDB (versioned system collection) |
| D3  | Query interface          | Typed builder API over a query IR; SQL parser later on the same IR |
| D4  | Referential integrity    | Configurable per relation: `strict` or `loose` |
| D5  | Client protocol          | gRPC + HTTP/JSON gateway |
| D6  | Physical relation storage| References (FK fields / id arrays) + optional denormalized embed |
| D7  | Field strictness         | Declared fields are typed and validated; undeclared fields pass through |
| D8  | First milestone          | Read path + full integrity (FK checks, cascades, transactions, embed sync) |
| D9  | Join execution           | Cost-based planner picks `$lookup` vs batched fetch per join |
| D10 | Embed sync               | Asynchronous, driven by change streams |
| D11 | Gateway                  | One binary, two listeners (tonic gRPC + axum HTTP) over one service core |
| D12 | Authentication           | API keys / bearer tokens |
| D13 | Wire document encoding   | Raw BSON bytes in gRPC; relaxed Extended JSON over HTTP |
| D14 | Filter IR                | Typed expression tree with relation paths; raw Mongo filter as escape hatch |
| D15 | DB-side validation       | Layer validation **and** a MongoDB `$jsonSchema` validator (extras allowed) |
| D16 | Relation model           | Explicit SQL-style kinds: `belongs_to`, `has_one`, `has_many`, `many_to_many` |
| D17 | FK concurrency control   | Intent-lock documents with TTL in `_mr_locks` |

## Workspace layout

```
crates/
  mr-core     Pure logic, no I/O: schema model + validation, query IR,
              planner, Mongo compiler, write IR, document validation.
  mr-engine   MongoDB I/O: catalog store, query executor, write path with
              integrity, intent locks, embed-sync worker, stats cache.
  mr-proto    Protobuf definitions (tonic/prost) + proto <-> core conversion.
  mr-server   The binary: config, auth, gRPC service, HTTP gateway.
  mr-client   Rust client SDK with the typed builder API (gRPC).
```

`mr-core` stays free of I/O so the planner and compiler can be unit-tested
quickly and deterministically. Everything that talks to MongoDB lives in
`mr-engine`. Both front-ends in `mr-server` call the same `Engine`.

## Schema catalog (D2, D7, D15, D16)

The schema is a list of collection definitions. Each one has declared fields
and named relations:

```jsonc
{
  "collections": [
    {
      "name": "users",
      "fields": [
        { "name": "email", "type": "string", "required": true, "unique": true },
        { "name": "name",  "type": "string" }
      ],
      "relations": [
        { "name": "posts", "kind": "has_many", "target": "posts", "foreign_key": "author_id",
          "on_delete": "cascade" }
      ]
    },
    {
      "name": "posts",
      "fields": [ { "name": "title", "type": "string", "required": true } ],
      "relations": [
        { "name": "author", "kind": "belongs_to", "target": "users", "foreign_key": "author_id",
          "embed": { "fields": ["name"], "into": "author_snapshot" } },
        { "name": "tags", "kind": "many_to_many", "target": "tags", "via": { "local": "tag_ids" } }
      ]
    }
  ]
}
```

### Relation kinds

| Kind           | Where the key lives                            | Cardinality |
|----------------|------------------------------------------------|-------------|
| `belongs_to`   | `foreign_key` on **this** collection           | to-one      |
| `has_one`      | `foreign_key` on the **target** collection     | to-one      |
| `has_many`     | `foreign_key` on the **target** collection     | to-many     |
| `many_to_many` | `via.local`: id array on this collection<br>`via.remote`: id array on the target<br>`via.junction`: rows in a junction collection | to-many |

`references` (default `_id`) names the field the key points at.

Internally every relation compiles to:

* a **join path**: one or two equality hops (`from.field = to.field`). Two hops
  are used only for junction tables. The executor and compiler use join paths
  only, never the relation kind.
* zero or more **constraints**: physical FK facts (owner collection and field,
  scalar or array, target collection and field, integrity, `on_delete`).

Both sides of a relation may be declared (for example `posts.author`
`belongs_to` and `users.posts` `has_many`). They describe the **same**
physical FK, so the catalog merges them into one constraint. Settings declared
on only one side are taken from that side. Settings that conflict between the
two sides fail validation. This keeps the familiar SQL kinds without letting
the two declarations drift apart.

Defaults: `integrity: strict`, `on_delete: restrict` (SQL semantics). A FK
field named by a relation gets an index automatically.

### Field strictness

Declared fields are typed (`object_id`, `string`, `int`, `long`, `double`,
`decimal`, `bool`, `date`, `binary`, `uuid`, `object`, `array<T>`, `any`) and
can be `required`, `unique` and `nullable`. Undeclared fields are stored
untouched. On every apply, a `$jsonSchema` validator covering only the
declared fields is installed with `additionalProperties` allowed, so writes
that bypass the layer cannot corrupt declared fields either.

### Storage and versioning

The schema lives in `<metadata_db>._mr_schema`, one document per version
(`_id` = version number). Applying a schema means: diff the desired schema
against the current one, validate it, create indexes and validators, then
insert version `n+1`. The unique `_id` gives optimistic concurrency between
server instances. Each instance caches the resolved schema and follows
`_mr_schema` through a change stream, so every instance picks up new versions.

## Query IR and planner (D3, D9, D14)

```
Query   { collection, filter?: Expr, select?, include: [Include], sort, skip?, limit? }
Include { relation, alias?, filter?, select?, sort, limit?, required, include: [Include], strategy? }
Expr    = And | Or | Not | Cmp(path op value) | In | Nin | Exists | Regex | IsNull
        | Any(relation, Expr) | All(relation, Expr) | Raw(document)
```

A path such as `author.country` crosses to-one relations. A filter that needs
a to-many relation uses `Any`/`All` (SQL `EXISTS`). Before planning, the IR is
normalised:

1. Relation paths are rewritten into quantifiers:
   `author.country = "US"` becomes `Any(author, country = "US")`.
2. `All(r, f)` becomes `Not(Any(r, Not f))`.
3. A `required` include (inner join) adds `Any(relation, include.filter)` to
   the parent filter, so pagination stays correct.

### Strategies

For each relational predicate (`Any`):

* **Semi-join** (adaptive): run the inner query against the related collection
  first, collect the matching join keys, and substitute `field $in [keys]`
  (or `$nin` under `Not`). The pre-query is capped at
  `semi_join_max_keys + 1`. If the cap is exceeded, the planner falls back to
  `$lookup`. The resulting pure filter can use the parent's indexes.
* **Lookup**: compile a `$lookup` (using `localField`/`foreignField` plus a
  `pipeline`, limited to one match) and test that the result is non-empty.

For each include:

* **Lookup**: one aggregation round trip. Nested includes compile into the
  sub-pipeline.
* **Batch**: after a page of parents is fetched, collect their keys and run one
  query per include level (`target_field $in keys`), then stitch the results in
  memory. Per-parent limits use `$group` + `$slice`. Total queries equal the
  number of include nodes, with no N+1.

The cost model uses cached collection statistics: estimated count, whether the
collection is sharded, and indexes. It weighs round trips against per-parent
`$lookup` cost. `$lookup` is never chosen for sharded targets. Once a node uses
`$lookup`, all of its descendants do too. A client can force a strategy per
include with `strategy`.

### Execution

Root results are read from a cursor in chunks of `chunk_size` documents. Each
chunk has its batch includes resolved and is then streamed to the client
(gRPC server streaming / NDJSON over HTTP), so memory stays bounded for large
result sets. Fields needed only for joining are fetched and stripped before
output.

## Write path and integrity (D4, D8, D17)

Write operations: `insert`, `update` (filter + update document), `delete`
(filter). A `batch` runs several operations atomically.

A write runs in a multi-document transaction when it involves integrity work
(a strict FK check, cascade, or set-null) or is a multi-op batch. Otherwise it
runs as a plain write. Transient transaction errors are retried with
exponential backoff and jitter.

* **Insert**: validate declared fields. For each strict outbound constraint,
  take intent locks on the referenced keys and verify that they exist. Fill
  embed snapshots. Then insert.
* **Update**: when the update touches FK paths or embedded snapshots, collect
  the matching `_id`s, apply the update, read the touched fields back, then
  lock and verify the new keys and refresh snapshots. If the update changes a
  field that strict constraints reference, `ON UPDATE RESTRICT` applies.
* **Delete**: for every strict inbound constraint, lock the keys being
  deleted, then apply `on_delete`: `restrict` fails if any references exist,
  `cascade` deletes them recursively (with a cycle guard), `set_null` sets a
  scalar FK to `null` or `$pull`s the key from an array FK, `no_action` does
  nothing.

`loose` constraints get no checks and no actions. They only drive joins.

### Intent locks (D17)

Snapshot isolation alone allows write skew: inserting a post for user U and
deleting U can both commit. To prevent this, both the referencing write and
the deleting write upsert the same lock document,
`_mr_locks { _id: "<collection>|<key>", at }`, inside their transactions.
MongoDB detects the write-write conflict and one transaction retries. A TTL
index on `at` removes lock documents after a while. User documents are never
modified for locking.

## Embed sync (D6, D10)

An embed keeps a copy of selected target fields on the referencing document:
an object for a `belongs_to`, or an array of `{_id, ...}` for a local-array
`many_to_many`. The layer fills the copy synchronously on insert, and on any
update that changes the FK. A background worker then keeps the copies
up to date:

* It watches each target collection's change stream (`fullDocument:
  updateLookup`). When an update touches an embedded field, it rewrites the
  copies with `updateMany` (using `arrayFilters` for array embeds). When a
  target is deleted, it unsets the copy or `$pull`s it from the array.
* It watches the owning collections for FK changes made **outside** the layer
  and refreshes those copies.
* Resume tokens are saved in `_mr_sync_state`, so the worker continues where it
  left off after a restart.

Embeds are eventually consistent. Because they come from change streams, they
also follow writes that bypass the layer.

## Server (D1, D5, D11, D12, D13)

One binary with two listeners:

* gRPC (tonic). Documents are raw BSON bytes. Query results are server
  streams of BSON document batches.
* HTTP/JSON (axum). The request/response shapes mirror the proto messages
  (snake_case), and documents and values are relaxed Extended JSON. Query
  results are a JSON array, or NDJSON when `Accept: application/x-ndjson` is
  sent.

Auth: `Authorization: Bearer <key>` (HTTP) or the `authorization` metadata key
(gRPC). Keys are configured as SHA-256 hashes, so plaintext keys are never
stored. A key can be read-only, read-write, or admin (admin is needed for
schema changes).

## Requirements

MongoDB 5.0+ running as a replica set or sharded cluster. Transactions and
change streams need this, and `$lookup` with `localField` + `pipeline` needs
5.0. A single-node replica set is enough for development.
