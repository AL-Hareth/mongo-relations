# Chapter 5: Asking questions across relations

> **In this chapter:** filter documents by facts stored in *related*
> documents. For example: "pets whose owner lives in Lisbon", "owners who
> have a cat", "owners with no pets". No schema changes; you'll only query.

If this is a new terminal: `cd ~/petclinic`, then re-run the `export AUTH=...`
and `mr() {...}` lines from chapter 3.

## Step 1: Follow a relation in a filter path

Pets don't store their owner's city; owners do. But you can reach it
through the `owner` relation by writing a **path** with a dot,
`owner.city`:

```sh
mr query '{
  "collection": "pets",
  "filter": { "cmp": { "path": "owner.city", "op": "eq", "value": "Lisbon" } },
  "select": ["name"]
}'
```

```json
[{"_id":"rex","name":"Rex"},{"_id":"misty","name":"Misty"}]
```

Read `owner.city` as "the `city` of this pet's `owner`". It works because
`owner` is a relation name on `pets`, so the server knows to look in the
`owners` collection.

> 💡 **Behind the scenes:** the server first finds the owners in Lisbon
> (`["ana", "chloe"]`), then looks for pets whose `owner_id` is one of
> those, using the index it created in chapter 4. You can see this plan for
> yourself with `mr explain '<same query>'`.

## Step 2: "Owners who have at least one cat"

A dotted path only works when there's **one** related document, like a
pet's single owner. An owner can have **many** pets, so "the species of
Ana's pet" is ambiguous. Which pet?

Try it and the server explains:

```sh
mr query '{ "collection": "owners",
  "filter": { "cmp": { "path": "pets.species", "op": "eq", "value": "cat" } } }'
```

```json
{"error":{"kind":"invalid_argument","message":"invalid query: `pets` is a to-many relation; filter it with `any` or `all`"}}
```

For to-many relations, say what you mean with **`any`**: "at least one
related document matches":

```sh
mr query '{
  "collection": "owners",
  "filter": { "any": {
    "relation": "pets",
    "filter": { "cmp": { "path": "species", "op": "eq", "value": "cat" } }
  } },
  "select": ["name"]
}'
```

```json
[{"_id":"ana","name":"Ana"},{"_id":"ben","name":"Ben"}]
```

Ana has Misty and Ben has Pickles. The inner `filter` is about **pets**, so
its paths (`species`) are pet fields.

## Step 3: Who has pets? Who doesn't?

Leave out the inner filter and `any` simply means "has at least one":

```sh
mr query '{ "collection": "owners", "filter": { "any": { "relation": "pets" } }, "select": ["name"] }'
```

```json
[{"_id":"ana","name":"Ana"},{"_id":"ben","name":"Ben"}]
```

And `is_null` on a relation means "has none":

```sh
mr query '{ "collection": "owners", "filter": { "is_null": { "path": "pets" } }, "select": ["name"] }'
```

```json
[{"_id":"chloe","name":"Chloe"}]
```

## Step 4: "Every pet is a cat" with `all`

`all` means "every related document matches":

```sh
mr query '{
  "collection": "owners",
  "filter": { "all": {
    "relation": "pets",
    "filter": { "cmp": { "path": "species", "op": "eq", "value": "cat" } }
  } },
  "select": ["name"]
}'
```

```json
[{"_id":"ben","name":"Ben"},{"_id":"chloe","name":"Chloe"}]
```

Ben's only pet is a cat, so he matches. But **Chloe has no pets at all,
and she matches too**. That's surprising at first, but logical: "every one
of Chloe's pets is a cat" is true, because there's no pet to prove it false.
(SQL behaves the same way.) If you want owners who have pets *and* only
cats, combine conditions, as in the next step.

## Step 5: Combine conditions with `and`, `or`, `not`

Filters can be combined. Here are "cats older than 3, or any dog":

```sh
mr query '{
  "collection": "pets",
  "filter": { "or": [
    { "and": [
      { "cmp": { "path": "species", "op": "eq", "value": "cat" } },
      { "cmp": { "path": "age", "op": "gt", "value": 3 } }
    ] },
    { "cmp": { "path": "species", "op": "eq", "value": "dog" } }
  ] },
  "select": ["name"]
}'
```

```json
[{"_id":"rex","name":"Rex"},{"_id":"pickles","name":"Pickles"}]
```

- `and`: every condition in the list must hold.
- `or`: at least one must hold.
- `not`: flips a condition, e.g. `{ "not": { "any": { "relation": "pets" } } }`
  means "has no pets".

Relation filters mix freely with ordinary ones, and they work with
`/v1/count` too:

```sh
mr count '{ "collection": "owners", "filter": { "any": { "relation": "pets" } } }'
```

```json
{"count":2}
```

## What you learned

| You want… | Write… |
|---|---|
| a field of a **single** related document | a dotted path: `"owner.city"` |
| at least one related document matching | `{ "any": { "relation": "pets", "filter": … } }` |
| at least one related document at all | `{ "any": { "relation": "pets" } }` |
| no related documents | `{ "is_null": { "path": "pets" } }` |
| every related document matching | `{ "all": { "relation": "pets", "filter": … } }` |
| several conditions | `and`, `or`, `not` |

## Try it yourself

<details>
<summary>1. Find owners who live in Lisbon <b>and</b> have a dog.</summary>

```sh
mr query '{
  "collection": "owners",
  "filter": { "and": [
    { "cmp": { "path": "city", "op": "eq", "value": "Lisbon" } },
    { "any": { "relation": "pets",
               "filter": { "cmp": { "path": "species", "op": "eq", "value": "dog" } } } }
  ] },
  "select": ["name"]
}'
```

```json
[{"_id":"ana","name":"Ana"}]
```
</details>

<details>
<summary>2. Find owners who have pets, and whose pets are all cats. (Chloe should <i>not</i> appear.)</summary>

Combine `any` (has pets) with `all` (only cats):

```sh
mr query '{
  "collection": "owners",
  "filter": { "and": [
    { "any": { "relation": "pets" } },
    { "all": { "relation": "pets",
               "filter": { "cmp": { "path": "species", "op": "eq", "value": "cat" } } } }
  ] },
  "select": ["name"]
}'
```

```json
[{"_id":"ben","name":"Ben"}]
```
</details>

---

← [Chapter 4](04-first-relation.md) · Next: [Chapter 6: Many-to-many: vets and visits →](06-many-to-many.md)
