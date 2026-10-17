# Chapter 1: Why relations?

> **In this chapter:** what a document database is, why data often needs to
> point at other data, what goes wrong when it does, and how mongo-relations
> helps. There are no commands yet, just ideas. ☕

## Documents

MongoDB stores **documents**. A document is a bundle of named values, very
much like a JSON object:

```json
{
  "_id": "rex",
  "name": "Rex",
  "species": "dog",
  "age": 4
}
```

Documents of the same kind live together in a **collection**: all pets go
in a `pets` collection, all owners in an `owners` collection.

Every document has an `_id` field that uniquely identifies it, like a
passport number. No two documents in a collection can share an `_id`.

Documents are **flexible**. One pet might have a `microchip` field and
another might not, and MongoDB doesn't mind. That flexibility is a big
reason people like MongoDB.

## Data that points at other data

Rex has an owner, Ana. How do we record that? There are two options.

**Option A: copy the owner into the pet.**

```json
{ "_id": "rex", "name": "Rex", "owner": { "name": "Ana", "city": "Lisbon" } }
```

This is simple, but Ana also owns Misty, so Ana's details now live in two
places. When Ana moves to Porto, you have to find and update every copy. If
you miss one, your data contradicts itself.

**Option B: store a reference.**

```json
{ "_id": "rex", "name": "Rex", "owner_id": "ana" }
```

Ana's details live in exactly one document, in `owners`, and each pet just
says *which* owner it belongs to. The `owner_id` field is called a
**foreign key**: a field holding the `_id` of a document in another
("foreign") collection.

Option B is how relational databases like PostgreSQL work, and it's usually
the right default. But with plain MongoDB it comes with chores.

## The chores of references

### Chore 1: joining

To show "Rex, owned by Ana from Lisbon", you need data from two collections.
Combining them is called a **join**. In plain MongoDB you either write an
aggregation pipeline with `$lookup` stages, or make two queries and stitch
the results together in your code. That is fine once, and tedious the
hundredth time. Joins are also easy to get slow, for example by running one
extra query per pet (the "N+1 problem").

### Chore 2: keeping references valid

Nothing in plain MongoDB stops you from:

- creating a pet with `"owner_id": "anna"`, a typo for an owner that
  doesn't exist, or
- deleting Ana while Rex still points at her.

Either way you end up with **dangling references**: pets whose owner doesn't
exist. Keeping references valid is called **referential integrity**, and in
plain MongoDB it's entirely your job.

## What mongo-relations does

mongo-relations is a server that sits between your application and MongoDB:

```
  your app ──▶ mongo-relations ──▶ MongoDB
                 • knows how collections relate
                 • does the joins for you
                 • refuses writes that would break references
                 • keeps copies up to date
```

You describe your collections and their **relations** once, in a
**schema**. From then on:

| Chore | With mongo-relations |
|---|---|
| Joining | Ask for "pets, including their owner" and the server builds an efficient query. |
| Filtering through references | Ask for "pets whose owner lives in Lisbon" directly. |
| Typos in references | The insert is refused: *owner "anna" doesn't exist*. |
| Deleting referenced data | You choose per relation: refuse, delete the dependents too, or clear the reference. |
| Copies going stale | The server updates copies for you (chapter 8). |

And your documents stay flexible: fields you don't mention in the schema are
stored exactly as you send them.

## Vocabulary

You'll meet these words throughout the tutorial:

| Term | Meaning |
|---|---|
| **Collection** | A group of documents of the same kind (`pets`). |
| **Document** | One record (`Rex`). |
| **`_id`** | The unique identifier of a document. |
| **Schema** | Your description of collections, fields and relations. |
| **Relation** | A named connection between two collections ("a pet's `owner`"). |
| **Foreign key** | A field holding another document's `_id` (`owner_id`). |
| **One-to-many** | One owner, many pets. |
| **Many-to-many** | Many pets, many vets. |
| **Join / include** | Fetching related documents together with the main ones. |
| **Referential integrity** | Guaranteeing references always point at something real. |

## Check your understanding

<details>
<summary>1. Why not just copy the owner's details into every pet?</summary>

The same information would live in many places. When it changes (Ana moves),
every copy must be updated, and any copy you miss becomes wrong.
</details>

<details>
<summary>2. In <code>{ "name": "Misty", "owner_id": "ana" }</code>, which field is the foreign key, and what does it point at?</summary>

`owner_id`. It holds the `_id` of a document in the `owners` collection, here
the owner whose `_id` is `"ana"`.
</details>

<details>
<summary>3. What is a "dangling reference"?</summary>

A foreign key pointing at a document that doesn't exist, for example because
of a typo or because the document was deleted.
</details>

---

Next: [Chapter 2: Setting up →](02-setup.md)
