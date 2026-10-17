# Chapter 3: Your first collection

> **In this chapter:** describe the `owners` collection in a schema, add
> owners, and find them again with filters, sorting and field selection.
> No relations yet; those come in chapter 4.

Make sure the server from chapter 2 is running, and that your second
terminal is in `~/petclinic`.

## Step 1: Your API key in a variable

Every request needs an `authorization` header carrying your key. See what
happens without it:

```sh
curl -s localhost:8080/v1/schema
```

```json
{"error":{"kind":"unauthenticated","message":"missing or invalid API key"}}
```

Save the header in a shell variable so you don't have to retype it:

```sh
export AUTH="authorization: Bearer tutorial-key"
```

You'll pass it to `curl` as `-H "$AUTH"`. (If you open a new terminal later,
run the `export` line again.)

## Step 2: Describe the collection

A **schema** lists your collections and their fields. Create `schema.json`:

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
        ]
      }
    ]
  }
}
EOF
```

This says: there is a collection called `owners`, and its documents have:

- a `name`, which must be text (`string`) and is `required`: every owner
  must have one;
- a `city`, also text, but optional;
- an `email`, optional, but `unique`: no two owners may share one.

Other types include `int` (whole numbers), `double` (decimals), `bool`
(true/false), `date` and `object_id`. You'll meet several of them later.

Send the schema to the server. This step is called **applying** it:

```sh
curl -s -X PUT localhost:8080/v1/schema -H "$AUTH" --data @schema.json
```

```json
{"changed":true,"created_collections":["owners"],"created_indexes":["owners.mr_uniq_email"],
 "dropped_indexes":[],"dry_run":false,"validators_updated":["owners"],"version":1}
```

(`--data @schema.json` tells curl to send the contents of the file.)

The response tells you what the server did:

- **`created_collections`**: it created `owners` in MongoDB.
- **`created_indexes`**: it created an index that enforces "unique email".
  An index also makes lookups by email fast.
- **`validators_updated`**: it told MongoDB itself to check the field
  types. Even programs that write to MongoDB directly can't store a
  non-text `name`.
- **`version`: 1**: this is the first version of your schema. Every change
  creates a new version.

## Step 3: Add some owners

Changes go to `/v1/write` as a list of **operations** (`ops`). Each
operation says what to do (`op`), where (`collection`), and with what:

```sh
curl -s -X POST localhost:8080/v1/write -H "$AUTH" -d '{
  "ops": [
    { "op": "insert", "collection": "owners", "documents": [
      { "_id": "ana",   "name": "Ana",   "city": "Lisbon", "email": "ana@example.com" },
      { "_id": "ben",   "name": "Ben",   "city": "Porto",  "email": "ben@example.com" },
      { "_id": "chloe", "name": "Chloe", "city": "Lisbon", "phone": "+351 900 000 000" }
    ] }
  ]
}'
```

```json
{"deleted":0,"inserted_ids":["ana","ben","chloe"],"matched":0,"modified":0}
```

Three owners were inserted. Notice two things:

1. **We chose readable `_id`s** (`"ana"`, `"ben"`, `"chloe"`). That makes this
   tutorial easier to follow. In a real application you'd usually let
   MongoDB generate ids; you'll see one in step 6.
2. **Chloe has a `phone` field** that isn't in the schema, and that's fine.
   Fields the schema doesn't mention are stored exactly as you send them.
   That's the document flexibility from chapter 1, kept intact.

## Step 4: A shortcut for requests

Most requests in this tutorial are a `POST` to `/v1/<something>` with a
JSON body. Define a small shell function so you can type less:

```sh
mr() { curl -s -X POST "localhost:8080/v1/$1" -H "$AUTH" -d "$2"; echo; }
```

Now `mr query '{...}'` means "POST this JSON to `/v1/query`". The `echo`
just adds a newline after the response. (Like `AUTH`, this function lasts
until you close the terminal.)

> 💡 **Tip:** if you have [jq](https://jqlang.org) installed, pipe any
> response through it to pretty-print: `mr query '...' | jq`.

## Step 5: Find owners

**Everyone:**

```sh
mr query '{ "collection": "owners" }'
```

```json
[{"_id":"ana","city":"Lisbon","email":"ana@example.com","name":"Ana"},
 {"_id":"ben","city":"Porto","email":"ben@example.com","name":"Ben"},
 {"_id":"chloe","city":"Lisbon","name":"Chloe","phone":"+351 900 000 000"}]
```

**Only owners in Lisbon.** A `filter` describes which documents you want.
`cmp` means *compare*: the field at `path` must be equal (`eq`) to `value`:

```sh
mr query '{
  "collection": "owners",
  "filter": { "cmp": { "path": "city", "op": "eq", "value": "Lisbon" } }
}'
```

```json
[{"_id":"ana","city":"Lisbon","email":"ana@example.com","name":"Ana"},
 {"_id":"chloe","city":"Lisbon","name":"Chloe","phone":"+351 900 000 000"}]
```

**Just the names, in reverse alphabetical order.** `select` picks the
fields to return (`_id` always comes along), and `sort` orders the results:

```sh
mr query '{
  "collection": "owners",
  "select": ["name"],
  "sort": [ { "field": "name", "desc": true } ]
}'
```

```json
[{"_id":"chloe","name":"Chloe"},{"_id":"ben","name":"Ben"},{"_id":"ana","name":"Ana"}]
```

**How many owners live in Lisbon?** Counting has its own endpoint:

```sh
mr count '{
  "collection": "owners",
  "filter": { "cmp": { "path": "city", "op": "eq", "value": "Lisbon" } }
}'
```

```json
{"count":2}
```

### The comparison operators

| `op` | Meaning |
|---|---|
| `eq` / `ne` | equal / not equal |
| `gt` / `gte` | greater than / greater than or equal |
| `lt` / `lte` | less than / less than or equal |

To match any of several values, use `in`:
`{ "in": { "path": "city", "values": ["Porto", "Faro"] } }`.

## Step 6: Mistakes and how the server catches them

**A missing required field.** Every owner needs a name:

```sh
mr write '{ "ops": [ { "op": "insert", "collection": "owners",
  "documents": [ { "city": "Faro" } ] } ] }'
```

```json
{"error":{"kind":"invalid_argument","message":"document validation failed: `name`: is required"}}
```

**A duplicate email.** Ana already uses `ana@example.com`:

```sh
mr write '{ "ops": [ { "op": "insert", "collection": "owners",
  "documents": [ { "name": "Ann", "email": "ana@example.com" } ] } ] }'
```

```json
{"error":{"kind":"failed_precondition","message":"unique constraint violated: `owners` already has a document with { email: \"ana@example.com\" } (index `mr_uniq_email`)"}}
```

In both cases **nothing was saved**. Errors always have a `kind`, which a
program can check, and a `message` for humans.

**A generated id.** Leave out `_id` and MongoDB creates one for you:

```sh
mr write '{ "ops": [ { "op": "insert", "collection": "owners",
  "documents": [ { "name": "Dan", "city": "Faro" } ] } ] }'
```

```json
{"deleted":0,"inserted_ids":[{"$oid":"6ad382432ae15a6289019842"}],"matched":0,"modified":0}
```

That `{"$oid": "..."}` is an **ObjectId**, MongoDB's standard id type, shown
in JSON form. (Yours will be different.) Dan has no email, and that's
allowed: `unique` only stops two owners from having the *same* email.

Dan was only a demonstration, so delete him. A `delete` operation removes
every document matching its `filter` (deletes get their own chapter later):

```sh
mr write '{ "ops": [ { "op": "delete", "collection": "owners",
  "filter": { "cmp": { "path": "name", "op": "eq", "value": "Dan" } } } ] }'
```

```json
{"deleted":1,"matched":0,"modified":0}
```

## What you learned

- A **schema** describes collections and their fields. You *apply* it with
  `PUT /v1/schema`, and each change gets a new version number.
- Fields can be `required` or `unique`, and fields the schema doesn't
  mention are still allowed.
- `/v1/write` takes a list of operations (`insert`, `delete`, and more
  later).
- `/v1/query` finds documents, using `filter`, `select` and `sort`.
  `/v1/count` counts them.

## Try it yourself

<details>
<summary>1. List the names of owners who do <b>not</b> live in Lisbon.</summary>

Use the `ne` (not equal) operator:

```sh
mr query '{
  "collection": "owners",
  "filter": { "cmp": { "path": "city", "op": "ne", "value": "Lisbon" } },
  "select": ["name"]
}'
```

```json
[{"_id":"ben","name":"Ben"}]
```
</details>

<details>
<summary>2. Count the owners who have an email address.</summary>

The `exists` filter checks whether a field is present:

```sh
mr count '{ "collection": "owners", "filter": { "exists": { "path": "email", "exists": true } } }'
```

```json
{"count":2}
```
</details>

---

← [Chapter 2](02-setup.md) · Next: [Chapter 4: Your first relation →](04-first-relation.md)
