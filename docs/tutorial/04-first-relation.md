# Chapter 4: Your first relation

> **In this chapter:** add a `pets` collection, connect each pet to its
> owner, and fetch owners and pets *together* with a single request.

Before you start, in your second terminal: `cd ~/petclinic`, and re-run the
`export AUTH=...` and `mr() {...}` lines from chapter 3 if this is a new
terminal.

## The idea

Each pet stores its owner's `_id` in an `owner_id` field, which is a foreign
key (see chapter 1):

```
owners                      pets
┌────────────┐              ┌──────────────────────────┐
│ _id: "ana" │◀─────────────│ _id: "rex"               │
│ name: Ana  │      ┌───────│ owner_id: "ana"          │
└────────────┘      │       └──────────────────────────┘
                    │       ┌──────────────────────────┐
                    └───────│ _id: "misty"             │
                            │ owner_id: "ana"          │
                            └──────────────────────────┘
```

One owner, many pets: a **one-to-many** relation.

## Step 1: Add pets to the schema

The schema always describes **everything**, so this new `schema.json`
repeats `owners` and adds `pets`. Replace the file:

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
          { "name": "owner", "kind": "belongs_to", "target": "owners", "foreign_key": "owner_id" }
        ]
      }
    ]
  }
}
EOF
```

There are two new `relations` entries, one on each side:

- On **pets**: `owner` is a `belongs_to` relation. *A pet belongs to an
  owner, and the pet holds the key* in its `owner_id` field.
- On **owners**: `pets` is a `has_many` relation. *An owner has many pets,
  and the pets hold the key* in their `owner_id` field.

Both describe **the same connection**, seen from opposite ends. Declaring
both sides is optional, but it lets you navigate in both directions: from a
pet to its owner, and from an owner to their pets. The relation `name`
(`owner`, `pets`) is what you'll use in queries.

> 💡 **Types must match.** Our owners' `_id`s are strings (`"ana"`), so
> `owner_id` is declared as `string`. If your `_id`s were generated
> ObjectIds, you'd declare the foreign key as `object_id`. A foreign key
> must have the same type as the `_id` it points to, or it won't match.

Apply it:

```sh
curl -s -X PUT localhost:8080/v1/schema -H "$AUTH" --data @schema.json
```

```json
{"changed":true,"created_collections":["pets"],"created_indexes":["pets.mr_fk_owner_id"],
 "dropped_indexes":[],"dry_run":false,"validators_updated":["owners","pets"],"version":2}
```

The schema is now at version 2. The server also created an index on
`pets.owner_id`, so finding "all pets of Ana" stays fast however many pets
you have.

## Step 2: Add pets

```sh
mr write '{ "ops": [ { "op": "insert", "collection": "pets", "documents": [
  { "_id": "rex",     "name": "Rex",     "species": "dog", "age": 4, "owner_id": "ana" },
  { "_id": "misty",   "name": "Misty",   "species": "cat", "age": 2, "owner_id": "ana" },
  { "_id": "pickles", "name": "Pickles", "species": "cat", "age": 7, "owner_id": "ben" }
] } ] }'
```

```json
{"deleted":0,"inserted_ids":["rex","misty","pickles"],"matched":0,"modified":0}
```

Ana has two pets, Ben has one, and Chloe has none.

## Step 3: The server checks your references

Try adding a pet for an owner who doesn't exist (note the typo, `anna`):

```sh
mr write '{ "ops": [ { "op": "insert", "collection": "pets", "documents": [
  { "name": "Ghost", "species": "dog", "owner_id": "anna" } ] } ] }'
```

```json
{"error":{"kind":"failed_precondition","message":"foreign key violation: `pets.owner_id` references missing `owners` key(s) [\"anna\"]"}}
```

Refused! This is **referential integrity** at work: a pet can only point at
an owner who really exists. In plain MongoDB, Ghost would have been saved
with a dangling reference.

## Step 4: Fetch pets *with* their owners

Now the payoff. Add an `include` to a query to fetch related documents
along with the main ones. This is a **join**:

```sh
mr query '{
  "collection": "pets",
  "include": [ { "relation": "owner" } ]
}'
```

```json
[
  { "_id": "rex", "age": 4, "name": "Rex",
    "owner": { "_id": "ana", "city": "Lisbon", "email": "ana@example.com", "name": "Ana" },
    "owner_id": "ana", "species": "dog" },
  { "_id": "misty", "age": 2, "name": "Misty",
    "owner": { "_id": "ana", "city": "Lisbon", "email": "ana@example.com", "name": "Ana" },
    "owner_id": "ana", "species": "cat" },
  { "_id": "pickles", "age": 7, "name": "Pickles",
    "owner": { "_id": "ben", "city": "Porto", "email": "ben@example.com", "name": "Ben" },
    "owner_id": "ben", "species": "cat" }
]
```

Each pet now carries an `owner` field holding the full owner document. In
MongoDB itself nothing changed: Ana's details are still stored once. The
server stitched them together just for this answer.

## Step 5: The other direction: owners with their pets

```sh
mr query '{
  "collection": "owners",
  "select": ["name"],
  "include": [ { "relation": "pets", "select": ["name", "species"] } ]
}'
```

```json
[
  { "_id": "ana",   "name": "Ana",   "pets": [ { "_id": "rex", "name": "Rex", "species": "dog" },
                                             { "_id": "misty", "name": "Misty", "species": "cat" } ] },
  { "_id": "ben",   "name": "Ben",   "pets": [ { "_id": "pickles", "name": "Pickles", "species": "cat" } ] },
  { "_id": "chloe", "name": "Chloe", "pets": [] }
]
```

Things to notice:

- **`owner` was a single object, but `pets` is a list.** A pet has one
  owner, but an owner can have many pets.
- **Chloe is included with an empty list.** An include never hides parent
  documents. (Chapter 5 shows how to find only owners *with* pets.)
- **`select` works inside an include too**, so each pet shows only its name
  and species.

## Step 6: A sneak peek at protection

What if Ana leaves the clinic? Try deleting her:

```sh
mr write '{ "ops": [ { "op": "delete", "collection": "owners",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "ana" } } } ] }'
```

```json
{"error":{"kind":"failed_precondition","message":"cannot delete `owners` document(s): still referenced by `pets.owner_id` (on_delete: restrict)"}}
```

The server refuses, because deleting Ana would leave Rex and Misty pointing
at nobody. This default behaviour is called `restrict`. In chapter 7 you'll
learn to choose other behaviours, such as deleting the pets along with
their owner.

## What you learned

- A **relation** connects two collections through a **foreign key** field.
- `belongs_to` goes on the side that holds the key; `has_many` goes on the
  other side. Together they describe one connection.
- The server **refuses references to documents that don't exist**, and by
  default **refuses to delete documents that are still referenced**.
- `include` fetches related documents in the same request. To-one relations
  give an object (or `null`); to-many relations give a list.

## Try it yourself

<details>
<summary>1. List each pet's name and age, youngest first, with only its owner's name.</summary>

```sh
mr query '{
  "collection": "pets",
  "select": ["name", "age"],
  "sort": [ { "field": "age" } ],
  "include": [ { "relation": "owner", "select": ["name"] } ]
}'
```

```json
[{"_id":"misty","age":2,"name":"Misty","owner":{"_id":"ana","name":"Ana"}},
 {"_id":"rex","age":4,"name":"Rex","owner":{"_id":"ana","name":"Ana"}},
 {"_id":"pickles","age":7,"name":"Pickles","owner":{"_id":"ben","name":"Ben"}}]
```

`owner_id` doesn't appear, because you didn't select it. The server used it
for the join and then left it out.
</details>

<details>
<summary>2. Show Ben with his pets, but call the list <code>animals</code> instead of <code>pets</code>.</summary>

An include can be renamed with `alias`:

```sh
mr query '{
  "collection": "owners",
  "filter": { "cmp": { "path": "_id", "op": "eq", "value": "ben" } },
  "select": ["name"],
  "include": [ { "relation": "pets", "alias": "animals", "select": ["name"] } ]
}'
```

```json
[{"_id":"ben","animals":[{"_id":"pickles","name":"Pickles"}],"name":"Ben"}]
```
</details>

---

← [Chapter 3](03-first-collection.md) · Next: [Chapter 5: Asking questions across relations →](05-filtering-across-relations.md)
