# Chapter 6: Many-to-many: vets and visits

> **In this chapter:** add vets, record visits, and connect pets and vets
> to each other through those visits: a **many-to-many** relation. You'll
> also store dates.

If this is a new terminal: `cd ~/petclinic`, then re-run the `export AUTH=...`
and `mr() {...}` lines from chapter 3.

## The idea

A pet can see several vets, and a vet sees many pets. Neither side can hold
"the" other's id, because there are many. The classic solution is a third
collection whose documents each link **one pet** to **one vet**. In a clinic
that collection is natural: it's the list of **visits**.

```
 pets             visits                          vets
 ┌──────┐   ┌──────────────────────────────┐   ┌──────────┐
 │ rex  │◀──│ pet_id: rex   vet_id: patel  │──▶│ dr-patel │
 │      │◀──│ pet_id: rex   vet_id: kim    │──▶│ dr-kim   │
 │misty │◀──│ pet_id: misty vet_id: patel  │   └──────────┘
 └──────┘   └──────────────────────────────┘
```

A collection that links two others like this is called a **junction
collection**. It can hold its own data too: each visit has a date and a
reason.

## Step 1: Extend the schema

Replace `schema.json` with this version. It adds `vets` and `visits`, and
two new relations on `pets`:

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
          { "name": "pets", "kind": "has_many", "target": "pets", "foreign_key": "owner_id" }
        ]
      },
      {
        "name": "pets",
        "fields": [
          { "name": "name",     "type": "string", "required": true },
          { "name": "species",  "type": "string", "required": true },
          { "name": "age",      "type": "int" },
          { "name": "owner_id", "type": "string", "required": true }
        ],
        "relations": [
          { "name": "owner",  "kind": "belongs_to", "target": "owners", "foreign_key": "owner_id" },
          { "name": "visits", "kind": "has_many",   "target": "visits", "foreign_key": "pet_id" },
          { "name": "vets",   "kind": "many_to_many", "target": "vets",
            "via": { "junction": { "collection": "visits", "local_key": "pet_id", "target_key": "vet_id" } } }
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
          { "name": "vet", "kind": "belongs_to", "target": "vets", "foreign_key": "vet_id" }
        ]
      }
    ]
  }
}
EOF
```

What's new:

- **`visits`** has two foreign keys, `pet_id` and `vet_id`, and two
  `belongs_to` relations: each visit has one pet and one vet. Its `date`
  field has the new type **`date`**.
- **`pets.vets`** is a `many_to_many` relation. Its `via.junction` part
  says: *go through the `visits` collection; a visit's `pet_id` points at
  this pet (`local_key`), and its `vet_id` points at the vet
  (`target_key`)*.
- **`vets.patients`** is the same connection seen from the vet's side, so
  `local_key` and `target_key` are swapped.
- **`pets.visits`** is a plain `has_many`. Sometimes you want the vets
  directly, and sometimes the visits themselves, with their dates and
  reasons.

Apply it:

```sh
curl -s -X PUT localhost:8080/v1/schema -H "$AUTH" --data @schema.json
```

```json
{"changed":true,"created_collections":["vets","visits"],"created_indexes":["visits.mr_fk_pet_id","visits.mr_fk_vet_id"],
 "dropped_indexes":[],"dry_run":false,"validators_updated":["owners","pets","vets","visits"],"version":3}
```

## Step 2: Add vets and visits

You can send several operations in one request. Dates are written as
`{ "$date": "<ISO 8601 date and time>" }`, where the trailing `Z` means UTC:

```sh
mr write '{ "ops": [
  { "op": "insert", "collection": "vets", "documents": [
    { "_id": "dr-patel", "name": "Dr. Patel" },
    { "_id": "dr-kim",   "name": "Dr. Kim" }
  ] },
  { "op": "insert", "collection": "visits", "documents": [
    { "pet_id": "rex",     "vet_id": "dr-patel", "date": { "$date": "2026-01-10T09:00:00Z" }, "reason": "vaccines" },
    { "pet_id": "misty",   "vet_id": "dr-patel", "date": { "$date": "2026-02-03T14:30:00Z" }, "reason": "check-up" },
    { "pet_id": "pickles", "vet_id": "dr-kim",   "date": { "$date": "2026-02-20T11:00:00Z" }, "reason": "limping" },
    { "pet_id": "rex",     "vet_id": "dr-kim",   "date": { "$date": "2026-03-15T16:00:00Z" }, "reason": "ate a sock" }
  ] }
] }'
```

```json
{"deleted":0,"inserted_ids":["dr-patel","dr-kim",{"$oid":"6ad382f4aa89e65fa6005d9b"},{"$oid":"6ad382f4aa89e65fa6005d9c"},
 {"$oid":"6ad382f4aa89e65fa6005d9d"},{"$oid":"6ad382f4aa89e65fa6005d9e"}],"matched":0,"modified":0}
```

The visits got generated ids, which is fine: you'll never need to type
them.

A plain string isn't accepted as a date:

```sh
mr write '{ "ops": [ { "op": "insert", "collection": "visits", "documents": [
  { "pet_id": "rex", "vet_id": "dr-kim", "date": "2026-04-01" } ] } ] }'
```

```json
{"error":{"kind":"invalid_argument","message":"document validation failed: `date`: expected date, got string"}}
```

## Step 3: Each pet's vets

Include a many-to-many relation exactly like any other:

```sh
mr query '{
  "collection": "pets",
  "select": ["name"],
  "include": [ { "relation": "vets", "select": ["name"], "sort": [ { "field": "name" } ] } ]
}'
```

```json
[{"_id":"rex","name":"Rex","vets":[{"_id":"dr-kim","name":"Dr. Kim"},{"_id":"dr-patel","name":"Dr. Patel"}]},
 {"_id":"misty","name":"Misty","vets":[{"_id":"dr-patel","name":"Dr. Patel"}]},
 {"_id":"pickles","name":"Pickles","vets":[{"_id":"dr-kim","name":"Dr. Kim"}]}]
```

The server walked from each pet, through `visits`, to `vets`. Rex has seen
both vets. The `sort` inside the include orders each pet's list of vets.

> 💡 **Tip:** without a `sort`, results come back in whatever order is
> fastest for MongoDB. If the order matters to you, always ask for it.

## Step 4: Includes inside includes

Includes can be nested. Here are Dr. Patel's patients, with each patient's
owner:

```sh
mr query '{
  "collection": "vets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "dr-patel" } },
  "select": ["name"],
  "include": [ {
    "relation": "patients",
    "select": ["name"],
    "sort": [ { "field": "name" } ],
    "include": [ { "relation": "owner", "select": ["name", "city"] } ]
  } ]
}'
```

```json
[{"_id":"dr-patel","name":"Dr. Patel","patients":[
   {"_id":"misty","name":"Misty","owner":{"_id":"ana","city":"Lisbon","name":"Ana"}},
   {"_id":"rex","name":"Rex","owner":{"_id":"ana","city":"Lisbon","name":"Ana"}}]}]
```

Three collections in one request: vets → pets → owners.

## Step 5: The visit log

Query the junction collection directly to see visits with their details,
newest first:

```sh
mr query '{
  "collection": "visits",
  "select": ["date", "reason"],
  "sort": [ { "field": "date", "desc": true } ],
  "include": [
    { "relation": "pet", "select": ["name"] },
    { "relation": "vet", "select": ["name"] }
  ]
}'
```

```json
[{"_id":{"$oid":"6ad382f4aa89e65fa6005d9e"},"date":{"$date":"2026-03-15T16:00:00Z"},"pet":{"_id":"rex","name":"Rex"},"reason":"ate a sock","vet":{"_id":"dr-kim","name":"Dr. Kim"}},
 {"_id":{"$oid":"6ad382f4aa89e65fa6005d9d"},"date":{"$date":"2026-02-20T11:00:00Z"},"pet":{"_id":"pickles","name":"Pickles"},"reason":"limping","vet":{"_id":"dr-kim","name":"Dr. Kim"}},
 {"_id":{"$oid":"6ad382f4aa89e65fa6005d9c"},"date":{"$date":"2026-02-03T14:30:00Z"},"pet":{"_id":"misty","name":"Misty"},"reason":"check-up","vet":{"_id":"dr-patel","name":"Dr. Patel"}},
 {"_id":{"$oid":"6ad382f4aa89e65fa6005d9b"},"date":{"$date":"2026-01-10T09:00:00Z"},"pet":{"_id":"rex","name":"Rex"},"reason":"vaccines","vet":{"_id":"dr-patel","name":"Dr. Patel"}}]
```

## Step 6: Each pet's most recent visit

An include on a to-many relation can have its own `sort` and `limit`. The
limit applies **to each pet separately**:

```sh
mr query '{
  "collection": "pets",
  "select": ["name"],
  "include": [ {
    "relation": "visits",
    "select": ["date", "reason"],
    "sort": [ { "field": "date", "desc": true } ],
    "limit": 1
  } ]
}'
```

```json
[{"_id":"rex","name":"Rex","visits":[{"_id":{"$oid":"6ad382f4aa89e65fa6005d9e"},"date":{"$date":"2026-03-15T16:00:00Z"},"reason":"ate a sock"}]},
 {"_id":"misty","name":"Misty","visits":[{"_id":{"$oid":"6ad382f4aa89e65fa6005d9c"},"date":{"$date":"2026-02-03T14:30:00Z"},"reason":"check-up"}]},
 {"_id":"pickles","name":"Pickles","visits":[{"_id":{"$oid":"6ad382f4aa89e65fa6005d9d"},"date":{"$date":"2026-02-20T11:00:00Z"},"reason":"limping"}]}]
```

Rex has two visits, but only his latest one (the sock incident 🧦) is shown.

## What you learned

- A **many-to-many** relation goes through a **junction collection** whose
  documents each link one document from each side.
- `"via": { "junction": { … } }` names the junction collection and which of
  its fields points to each side.
- The junction collection is an ordinary collection: it can have its own
  fields (`date`, `reason`) and relations, and you can query it directly.
- Includes can be **nested**, and to-many includes accept `sort` and a
  per-parent `limit`.
- Dates are written `{ "$date": "2026-01-10T09:00:00Z" }`.

## Try it yourself

<details>
<summary>1. Which vets have treated at least one cat?</summary>

Use `any` on the `patients` relation (chapter 5):

```sh
mr query '{
  "collection": "vets",
  "filter": { "any": { "relation": "patients",
              "filter": { "cmp": { "path": "species", "op": "eq", "value": "cat" } } } },
  "select": ["name"]
}'
```

```json
[{"_id":"dr-kim","name":"Dr. Kim"},{"_id":"dr-patel","name":"Dr. Patel"}]
```
</details>

<details>
<summary>2. List the reasons for all visits in February 2026.</summary>

Compare dates with `gte` and `lt`, combined with `and`:

```sh
mr query '{
  "collection": "visits",
  "filter": { "and": [
    { "cmp": { "path": "date", "op": "gte", "value": { "$date": "2026-02-01T00:00:00Z" } } },
    { "cmp": { "path": "date", "op": "lt",  "value": { "$date": "2026-03-01T00:00:00Z" } } }
  ] },
  "select": ["reason"]
}'
```

```json
[{"_id":{"$oid":"6ad382f4aa89e65fa6005d9c"},"reason":"check-up"},{"_id":{"$oid":"6ad382f4aa89e65fa6005d9d"},"reason":"limping"}]
```
</details>

---

← [Chapter 5](05-filtering-across-relations.md) · Next: [Chapter 7: Protecting your data →](07-protecting-your-data.md)
