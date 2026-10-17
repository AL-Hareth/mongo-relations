# Chapter 8: Copies that update themselves

> **In this chapter:** keep a copy of each owner's name and city *inside*
> every pet, so the most common reads need no join at all, and let the
> server keep those copies correct for you.

If this is a new terminal: `cd ~/petclinic`, then re-run the `export AUTH=...`
and `mr() {...}` lines from chapter 3.

## Back to chapter 1's dilemma

Chapter 1 compared two ways to connect a pet to its owner:

- **Copying** the owner into the pet: reads are fast and simple, but the
  copies go stale when the owner changes.
- **Referencing** the owner by id: there's one source of truth, but reads
  need a join.

mongo-relations lets you have **both**. The reference (`owner_id`) stays the
source of truth, and you can ask for a copy of selected fields to be stored
next to it. This is called an **embed**. The server keeps the copy up to
date whenever the original changes.

## Step 1: Ask for an embed

Add an `embed` to the `owner` relation on `pets`. Open `schema.json` in your
editor and change the `owner` relation of `pets` from:

```json
{ "name": "owner",  "kind": "belongs_to", "target": "owners", "foreign_key": "owner_id" },
```

to:

```json
{ "name": "owner",  "kind": "belongs_to", "target": "owners", "foreign_key": "owner_id",
  "embed": { "fields": ["name", "city"], "into": "owner_info" } },
```

This means: *copy the owner's `name` and `city` into a field called
`owner_info` on each pet.*

<details>
<summary>Or replace the whole file with this complete version</summary>

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
          { "name": "owner",  "kind": "belongs_to", "target": "owners", "foreign_key": "owner_id",
            "embed": { "fields": ["name", "city"], "into": "owner_info" } },
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
</details>

Apply it:

```sh
curl -s -X PUT localhost:8080/v1/schema -H "$AUTH" --data @schema.json
```

```json
{"changed":true,"created_collections":[],"created_indexes":["pets.mr_fk_owner_info._id"],
 "dropped_indexes":[],"dry_run":false,"validators_updated":["owners","pets","vets","visits"],"version":5}
```

## Step 2: Fill in copies for existing pets

The schema change doesn't touch pets that already exist:

```sh
mr query '{ "collection": "pets", "select": ["name", "owner_info"] }'
```

```json
[{"_id":"rex","name":"Rex"},{"_id":"misty","name":"Misty"},{"_id":"bolt","name":"Bolt"}]
```

Ask the server to (re)build every copy. This is an admin operation with its
own endpoint:

```sh
curl -s -X POST localhost:8080/v1/admin/rebuild-embeds -H "$AUTH"
```

```json
{"updated":3}
```

```sh
mr query '{ "collection": "pets", "select": ["name", "owner_info"] }'
```

```json
[{"_id":"rex","name":"Rex","owner_info":{"_id":"ana","city":"Lisbon","name":"Ana"}},
 {"_id":"misty","name":"Misty","owner_info":{"_id":"ana","city":"Lisbon","name":"Ana"}},
 {"_id":"bolt","name":"Bolt","owner_info":{"_id":"dina","city":"Braga","name":"Dina"}}]
```

Each pet now carries its owner's name and city, plus the owner's `_id`,
which the server uses to keep track of the copy. Note there's **no
`include`** in that query: the data is right there in the pet document.

## Step 3: New pets get their copy immediately

Chloe finally gets a pet:

```sh
mr write '{ "ops": [ { "op": "insert", "collection": "pets", "documents": [
  { "_id": "noodle", "name": "Noodle", "species": "hamster", "owner_id": "chloe" } ] } ] }'
```

```json
{"deleted":0,"inserted_ids":["noodle"],"matched":0,"modified":0}
```

```sh
mr query '{ "collection": "pets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "noodle" } },
  "select": ["name", "owner_info"] }'
```

```json
[{"_id":"noodle","name":"Noodle","owner_info":{"_id":"chloe","city":"Lisbon","name":"Chloe"}}]
```

You only sent `owner_id`; the server filled in `owner_info` while saving.

## Step 4: Watch a copy update itself

Ana moves to Coimbra. Change **only the owner**:

```sh
mr write '{ "ops": [ { "op": "update", "collection": "owners",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "ana" } },
  "update": { "$set": { "city": "Coimbra" } } } ] }'
```

```json
{"deleted":0,"matched":1,"modified":1}
```

Now look at her pets:

```sh
mr query '{ "collection": "pets",
  "filter": { "cmp": { "path": "owner_id", "op": "eq", "value": "ana" } },
  "select": ["name", "owner_info"] }'
```

```json
[{"_id":"rex","name":"Rex","owner_info":{"_id":"ana","city":"Coimbra","name":"Ana"}},
 {"_id":"misty","name":"Misty","owner_info":{"_id":"ana","city":"Coimbra","name":"Ana"}}]
```

Both copies say Coimbra. A background worker in the server noticed the
change and updated every copy.

> 💡 **Behind the scenes:** the worker uses MongoDB **change streams**, a
> feed of every change to a collection (one reason you needed a replica
> set). Because it watches MongoDB itself, it catches changes made by *any*
> program, not only those sent through mongo-relations. Updating copies
> takes a moment, usually well under a second. If you query instantly
> after a change you might briefly see the old value. This is called
> **eventual consistency**.

## Step 5: Copies are read-only

Copies belong to the server. Trying to edit one directly is refused, since
it would make the copy disagree with the original:

```sh
mr write '{ "ops": [ { "op": "update", "collection": "pets",
  "update": { "$set": { "owner_info.city": "Paris" } } } ] }'
```

```json
{"error":{"kind":"invalid_argument","message":"invalid write: `owner_info` is maintained by relation `owner` and cannot be written"}}
```

To change what the copy says, change the owner. To give a pet a different
owner, change `owner_id`, and the copy follows straight away:

```sh
mr write '{ "ops": [ { "op": "update", "collection": "pets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "noodle" } },
  "update": { "$set": { "owner_id": "dina" } } } ] }'
```

```sh
mr query '{ "collection": "pets",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "noodle" } },
  "select": ["name", "owner_info"] }'
```

```json
[{"_id":"noodle","name":"Noodle","owner_info":{"_id":"dina","city":"Braga","name":"Dina"}}]
```

## When should you use an embed?

| Use an `include` (chapter 4) when… | Add an `embed` when… |
|---|---|
| you need many or all of the related fields | you need just a few fields, very often |
| the related data changes often | the related data rarely changes |
| you always need the very latest value | a moment's delay is acceptable |

A good rule of thumb: start with includes, and add an embed when a
particular screen or report reads the same few fields over and over.

## What you learned

- An **embed** stores a copy of selected fields from a related document,
  so reads need no join.
- The reference stays the source of truth. Copies are filled on insert,
  refreshed when the reference changes, and updated in the background when
  the original document changes.
- Copies are **read-only** and **eventually consistent**.
- `POST /v1/admin/rebuild-embeds` fills or repairs every copy, for example
  after adding an embed to existing data.

## Try it yourself

<details>
<summary>1. Find pets whose owner lives in Coimbra, using the copy instead of a relation path.</summary>

`owner_info` is an ordinary field inside each pet, so filter on it directly:

```sh
mr query '{ "collection": "pets",
  "filter": { "cmp": { "path": "owner_info.city", "op": "eq", "value": "Coimbra" } },
  "select": ["name"] }'
```

```json
[{"_id":"rex","name":"Rex"},{"_id":"misty","name":"Misty"}]
```

Compare with chapter 5's `owner.city`, which looked the owners up. This
version reads only the `pets` collection.
</details>

---

← [Chapter 7](07-protecting-your-data.md) · Next: [Chapter 9: Where to go next →](09-next-steps.md)
