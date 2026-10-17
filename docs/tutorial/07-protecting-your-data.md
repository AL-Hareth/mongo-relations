# Chapter 7: Protecting your data

> **In this chapter:** decide what happens to related documents when
> something is deleted, update documents, and group changes so they succeed
> or fail together.

If this is a new terminal: `cd ~/petclinic`, then re-run the `export AUTH=...`
and `mr() {...}` lines from chapter 3.

## Three answers to "what about the dependents?"

In chapter 4 the server refused to delete Ana because Rex and Misty pointed
at her. Refusing is one possible answer, but not always the right one. For
each relation you can choose what happens to the documents that point at a
deleted one with **`on_delete`**:

| `on_delete` | When the referenced document is deleted… | Clinic example |
|---|---|---|
| `restrict` | …refuse the delete. **(The default.)** | A vet with visit history can't be deleted. |
| `cascade` | …delete the dependents too. | When an owner leaves, their pets' records go too. |
| `set_null` | …keep the dependents, but clear their reference. | If a pet's preferred vet leaves, the pet simply has no preference. |

> 💡 **Behind the scenes:** one relation kind already uses `cascade` by
> default. Junction rows (our visits) are deleted along with the pet or vet
> they link, because a visit with no pet makes no sense. You can override
> that, as you're about to do for vets.

## Step 1: Choose behaviours in the schema

Make these four changes to `schema.json`:

1. On `owners`, the `pets` relation gets `"on_delete": "cascade"`.
2. On `pets`, add a field `preferred_vet_id` (a `string`, and `nullable`,
   meaning it may hold `null`).
3. On `pets`, add a relation `preferred_vet` (`belongs_to` vets, through
   `preferred_vet_id`) with `"on_delete": "set_null"`.
4. On `visits`, the `vet` relation gets `"on_delete": "restrict"`, so visit
   history protects vets.

Here is the complete file with those changes:

```sh
cat > schema.json <<'EOF'
{
  "schema": {
    "collections": [
      {
        "name": "owners",
        "fields": [
          { "name": "name",  "type": "string", "required": true },
          { "name": "city",  "type": "string" },
          { "name": "email", "type": "string", "unique": true }
        ],
        "relations": [
          { "name": "pets", "kind": "has_many", "target": "pets", "foreign_key": "owner_id",
            "on_delete": "cascade" }
        ]
      },
      {
        "name": "pets",
        "fields": [
          { "name": "name",     "type": "string", "required": true },
          { "name": "species",  "type": "string", "required": true },
          { "name": "age",      "type": "int" },
          { "name": "owner_id", "type": "string", "required": true },
          { "name": "preferred_vet_id", "type": "string", "nullable": true }
        ],
        "relations": [
          { "name": "owner",  "kind": "belongs_to", "target": "owners", "foreign_key": "owner_id" },
          { "name": "visits", "kind": "has_many",   "target": "visits", "foreign_key": "pet_id" },
          { "name": "vets",   "kind": "many_to_many", "target": "vets",
            "via": { "junction": { "collection": "visits", "local_key": "pet_id", "target_key": "vet_id" } } },
          { "name": "preferred_vet", "kind": "belongs_to", "target": "vets",
            "foreign_key": "preferred_vet_id", "on_delete": "set_null" }
        ]
      },
      {
        "name": "vets",
        "fields": [
          { "name": "name", "type": "string", "required": true }
        ],
        "relations": [
          { "name": "patients", "kind": "many_to_many", "target": "pets",
            "via": { "junction": { "collection": "visits", "local_key": "vet_id", "target_key": "pet_id" } } }
        ]
      },
      {
        "name": "visits",
        "fields": [
          { "name": "date",   "type": "date",   "required": true },
          { "name": "reason", "type": "string" },
          { "name": "pet_id", "type": "string", "required": true },
          { "name": "vet_id", "type": "string", "required": true }
        ],
        "relations": [
          { "name": "pet", "kind": "belongs_to", "target": "pets", "foreign_key": "pet_id" },
          { "name": "vet", "kind": "belongs_to", "target": "vets", "foreign_key": "vet_id",
            "on_delete": "restrict" }
        ]
      }
    ]
  }
}
EOF
```

Notice that `on_delete` for owners → pets is written only on the owners
side. The two sides describe one connection, so the setting applies to
both. (If you wrote *different* values on the two sides, the server would
reject the schema as contradictory.)

Apply it:

```sh
curl -s -X PUT localhost:8080/v1/schema -H "$AUTH" --data @schema.json
```

```json
{"changed":true,"created_collections":[],"created_indexes":["pets.mr_fk_preferred_vet_id"],
 "dropped_indexes":[],"dry_run":false,"validators_updated":["owners","pets","vets","visits"],"version":4}
```

## Step 2: `restrict`: vets with history stay

Dr. Kim has visits on record:

```sh
mr write '{ "ops": [ { "op": "delete", "collection": "vets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "dr-kim" } } } ] }'
```

```json
{"error":{"kind":"failed_precondition","message":"cannot delete `vets` document(s): still referenced by `visits.vet_id` (on_delete: restrict)"}}
```

## Step 3: Updating documents

To demonstrate `set_null` you need a pet with a preferred vet, and that
means **changing** an existing pet. An `update` operation has a `filter`
(which documents to change) and an `update` (what to change). `$set`
assigns field values:

```sh
mr write '{ "ops": [
  { "op": "insert", "collection": "vets", "documents": [ { "_id": "dr-lopez", "name": "Dr. Lopez" } ] },
  { "op": "update", "collection": "pets",
    "filter": { "cmp": { "path": "_id", "op": "eq", "value": "misty" } },
    "update": { "$set": { "preferred_vet_id": "dr-lopez" } } }
] }'
```

```json
{"deleted":0,"inserted_ids":["dr-lopez"],"matched":1,"modified":1}
```

One request added a new vet **and** made her Misty's preferred vet
(`matched: 1`, `modified: 1`). Check it with an include:

```sh
mr query '{
  "collection": "pets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "misty" } },
  "select": ["name"],
  "include": [ { "relation": "preferred_vet", "select": ["name"] } ]
}'
```

```json
[{"_id":"misty","name":"Misty","preferred_vet":{"_id":"dr-lopez","name":"Dr. Lopez"}}]
```

Other update operators you'll find useful:

| Operator | Example | Effect |
|---|---|---|
| `$set` | `{ "$set": { "age": 5 } }` | set fields |
| `$unset` | `{ "$unset": { "age": "" } }` | remove fields |
| `$inc` | `{ "$inc": { "age": 1 } }` | add to a number |
| `$push` | `{ "$push": { "tags": "senior" } }` | append to a list |

## Step 4: `set_null`: the preference disappears, the pet stays

Dr. Lopez has no visits, so nothing restricts deleting her:

```sh
mr write '{ "ops": [ { "op": "delete", "collection": "vets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "dr-lopez" } } } ] }'
```

```json
{"cascaded":[{"collection":"pets","deleted":0,"nulled":1}],"deleted":1,"matched":0,"modified":0}
```

`cascaded` reports the side effects: in `pets`, 1 reference was cleared
(`nulled`). Misty is still here, with no preferred vet:

```sh
mr query '{ "collection": "pets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "misty" } },
  "select": ["name", "preferred_vet_id"] }'
```

```json
[{"_id":"misty","name":"Misty","preferred_vet_id":null}]
```

## Step 5: `cascade`: Ben leaves the clinic

Ben owns Pickles, and Pickles had a visit:

```sh
mr write '{ "ops": [ { "op": "delete", "collection": "owners",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "ben" } } } ] }'
```

```json
{"cascaded":[{"collection":"visits","deleted":1,"nulled":0},{"collection":"pets","deleted":1,"nulled":0}],
 "deleted":1,"matched":0,"modified":0}
```

A chain reaction: deleting Ben deleted Pickles (owners → pets is
`cascade`), and deleting Pickles deleted Pickles' visit (junction rows
cascade). Count what's left:

```sh
mr count '{ "collection": "pets" }'
mr count '{ "collection": "visits" }'
```

```json
{"count":2}
{"count":3}
```

Nothing dangling remains: no pet without an owner, no visit without a pet.

## Step 6: Updates are checked too

Moving Rex to an owner who doesn't exist is refused, just like inserting
him with one would be:

```sh
mr write '{ "ops": [ { "op": "update", "collection": "pets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "rex" } },
  "update": { "$set": { "owner_id": "zoe" } } } ] }'
```

```json
{"error":{"kind":"failed_precondition","message":"foreign key violation: `pets.owner_id` references missing `owners` key(s) [\"zoe\"]"}}
```

## Step 7: All or nothing

A new client, Dina, arrives with her dog Bolt. Register both in one
request, but with a typo in Bolt's `owner_id`:

```sh
mr write '{ "ops": [
  { "op": "insert", "collection": "owners", "documents": [ { "_id": "dina", "name": "Dina", "city": "Braga" } ] },
  { "op": "insert", "collection": "pets",   "documents": [ { "name": "Bolt", "species": "dog", "owner_id": "dyna" } ] }
] }'
```

```json
{"error":{"kind":"failed_precondition","message":"foreign key violation: `pets.owner_id` references missing `owners` key(s) [\"dyna\"]"}}
```

The first operation was fine, so was Dina saved anyway?

```sh
mr count '{ "collection": "owners", "filter": { "cmp": { "path": "_id", "op": "eq", "value": "dina" } } }'
```

```json
{"count":0}
```

No. **All operations in one request succeed together or not at all.** This
is called a **transaction**, and it means you never end up with half a
registration. Fix the typo and try again:

```sh
mr write '{ "ops": [
  { "op": "insert", "collection": "owners", "documents": [ { "_id": "dina", "name": "Dina", "city": "Braga" } ] },
  { "op": "insert", "collection": "pets",   "documents": [ { "_id": "bolt", "name": "Bolt", "species": "dog", "owner_id": "dina" } ] }
] }'
```

```json
{"deleted":0,"inserted_ids":["dina","bolt"],"matched":0,"modified":0}
```

> 💡 **Behind the scenes:** the server also protects you from *timing*
> accidents. If one person deletes an owner at the exact moment another
> adds a pet for that owner, one of the two requests is made to wait and
> retry, so an orphaned pet can't slip through.

## What you learned

- `on_delete` decides what happens to dependents: `restrict` (the default)
  refuses, `cascade` deletes them too, and `set_null` clears their
  reference.
- Cascades can chain across several collections, and the response lists
  every side effect under `cascaded`.
- `update` operations use MongoDB's operators (`$set`, `$inc`, …), and
  their references are checked just like inserts.
- All operations in one `/v1/write` request form a **transaction**: all or
  nothing.

## Try it yourself

<details>
<summary>1. <i>Without running it</i>: what would happen if you deleted Rex?</summary>

Rex would be deleted, along with **both** of his visits. Junction rows
cascade by default, and only the visit → **vet** link was changed to
`restrict`; the visit → **pet** link still cascades. The vets themselves
would be untouched. (Don't actually run it: the next chapter still uses
Rex!)
</details>

<details>
<summary>2. Rex just had a birthday. Increase his age by one with <code>$inc</code>.</summary>

```sh
mr write '{ "ops": [ { "op": "update", "collection": "pets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "rex" } },
  "update": { "$inc": { "age": 1 } } } ] }'
```

```json
{"deleted":0,"matched":1,"modified":1}
```

`mr query '{ "collection": "pets", "filter": { "cmp": { "path": "_id", "op": "eq", "value": "rex" } }, "select": ["age"] }'`
now shows `"age":5`.
</details>

---

← [Chapter 6](06-many-to-many.md) · Next: [Chapter 8: Copies that update themselves →](08-embedded-copies.md)
