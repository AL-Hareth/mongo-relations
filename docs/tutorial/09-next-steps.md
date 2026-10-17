# Chapter 9: Where to go next

🎉 **You made it!** You've built a small but complete relational data model
on top of MongoDB. Here's what you can now do:

- describe collections, fields and relations in a schema, and evolve it
  version by version;
- connect collections one-to-many (`belongs_to` / `has_many`) and
  many-to-many (through a junction collection);
- fetch related data with `include`, including nested includes with sorting
  and per-parent limits;
- filter through relations with dotted paths, `any`, `all` and `is_null`;
- rely on the server to reject broken references, and choose `restrict`,
  `cascade` or `set_null` for deletes;
- group changes into all-or-nothing requests;
- keep fast, self-updating copies with embeds.

## Cheat sheet

### Endpoints

| Request | Purpose |
|---|---|
| `GET /health` | Is everything up? (no key needed) |
| `PUT /v1/schema` | Apply a schema |
| `GET /v1/schema` | Show the current schema |
| `GET /v1/schema/history` | List schema versions |
| `POST /v1/query` | Find documents |
| `POST /v1/count` | Count documents |
| `POST /v1/explain` | Show how a query will run |
| `POST /v1/write` | Insert / update / delete (all or nothing) |
| `POST /v1/admin/rebuild-embeds` | Refill all embedded copies |

### Relation kinds

```jsonc
{ "name": "owner",  "kind": "belongs_to", "target": "owners", "foreign_key": "owner_id" }
{ "name": "pets",   "kind": "has_many",   "target": "pets",   "foreign_key": "owner_id" }
{ "name": "chip",   "kind": "has_one",    "target": "chips",  "foreign_key": "pet_id" }
{ "name": "vets",   "kind": "many_to_many", "target": "vets",
  "via": { "junction": { "collection": "visits", "local_key": "pet_id", "target_key": "vet_id" } } }
{ "name": "tags",   "kind": "many_to_many", "target": "tags", "via": { "local": "tag_ids" } }
```

`has_one` is like `has_many`, but returns a single document (or `null`)
instead of a list. A `many_to_many` can also skip the junction collection and
store a **list of ids** directly in a document: `"via": { "local": "tag_ids" }`
for a list kept on this side, or `"via": { "remote": "pet_ids" }` for a list
kept on the other side.

Options on any relation: `"on_delete"` (`restrict`, `cascade`, `set_null`,
`no_action`), `"embed"` (on `belongs_to` and local-list `many_to_many`), and
`"integrity": "loose"` to use a relation only for joins, without any checks.

### Query shape

```json
{
  "collection": "pets",
  "filter":  { … },
  "select":  ["name", "age"],
  "include": [ { "relation": "owner", "select": ["name"], "filter": { … },
                 "sort": [ … ], "limit": 5, "required": true, "include": [ … ] } ],
  "sort":    [ { "field": "age", "desc": true } ],
  "skip": 0,
  "limit": 20
}
```

`"required": true` on an include keeps only parents that have a match: Ana
and Ben would be listed, but not pet-less Chloe.

### Filters

```jsonc
{ "cmp":    { "path": "age", "op": "gte", "value": 3 } }        // eq ne gt gte lt lte
{ "in":     { "path": "species", "values": ["cat", "dog"] } }   // also "nin"
{ "exists": { "path": "email", "exists": true } }
{ "is_null":{ "path": "preferred_vet_id" } }
{ "regex":  { "path": "name", "pattern": "^r", "options": "i" } }
{ "cmp":    { "path": "owner.city", "op": "eq", "value": "Lisbon" } }  // through a to-one relation
{ "any":    { "relation": "pets", "filter": { … } } }
{ "all":    { "relation": "pets", "filter": { … } } }
{ "and": [ … ] }   { "or": [ … ] }   { "not": { … } }
```

## Keep going

- **Use it from a program.** Everything you did with `curl` works from any
  language that can send HTTP. For Rust there's a client library with a
  typed query builder; see section 8 of the
  [getting-started guide](../getting-started.md#8-use-the-rust-client).
- **Production setup.** Give each application its own API key with the
  smallest role it needs (`read` or `read_write`); keep `admin` for schema
  changes. All the configuration options are listed in
  [`config.example.toml`](../../config.example.toml).
- **Look inside.** `POST /v1/explain` shows the plan for any query, and
  [ARCHITECTURE.md](../ARCHITECTURE.md) explains how the planner, the
  integrity checks and the embed worker work.
- **Reference.** The [README](../../README.md) lists every endpoint and the
  exact semantics.

## Clean up

When you're done experimenting:

```sh
# Ctrl+C in the server's terminal, then:
docker stop mongo        # stop MongoDB (your data is kept)
docker rm -f mongo       # …or remove it and its data entirely
rm -r ~/petclinic        # remove the tutorial files
```

Thanks for following along, and happy building! 🐾

---

← [Chapter 8](08-embedded-copies.md) · [Back to the start](README.md)
