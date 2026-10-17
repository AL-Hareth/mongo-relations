# The mongo-relations tutorial

Welcome! This tutorial teaches you mongo-relations from scratch, one small
step at a time. By the end you will have built the database for a little
**pet clinic**: owners, their pets, the vets who treat them, and a history of
visits. You will also understand *why* each piece exists.

## Who this is for

You don't need to know SQL, database theory or Rust. You should be
comfortable:

- typing commands into a terminal, and
- reading a bit of JSON (`{ "name": "Rex" }` should look familiar).

If you have used MongoDB before, great. If not, chapter 1 explains everything
you need.

> **Already experienced?** The [getting-started guide](../getting-started.md)
> covers the same ground in one fast page.

## What you'll build

```
  owners ──< pets >──< visits >── vets
  (Ana)      (Rex)     (Jan 10)    (Dr. Patel)
```

An owner can have many pets. Each visit records one pet seeing one vet on a
date, so a pet can see many vets and a vet can see many pets.

## Chapters

Each chapter takes 10–20 minutes and builds on the one before.

| # | Chapter | You will learn |
|---|---------|----------------|
| 1 | [Why relations?](01-why-relations.md) | Documents, references, and the problems mongo-relations solves |
| 2 | [Setting up](02-setup.md) | Install everything and start your own server |
| 3 | [Your first collection](03-first-collection.md) | Schemas, inserting and finding documents |
| 4 | [Your first relation](04-first-relation.md) | Connecting pets to owners, and joining them back together |
| 5 | [Asking questions across relations](05-filtering-across-relations.md) | Filters like "pets whose owner lives in Lisbon" |
| 6 | [Many-to-many: vets and visits](06-many-to-many.md) | Linking collections through a junction collection |
| 7 | [Protecting your data](07-protecting-your-data.md) | What happens on delete, and all-or-nothing writes |
| 8 | [Copies that update themselves](08-embedded-copies.md) | Fast reads with embedded copies |
| 9 | [Where to go next](09-next-steps.md) | Recap, cheat sheet and further reading |

## How to read this tutorial

- **Type the commands yourself.** Copy-paste is fine, but run every one. The
  responses shown are real output, sometimes spread over several lines for
  readability, so you can compare. (The random `_id` values MongoDB
  generates will differ on your machine.)
- **Do the "Try it yourself" exercises.** Each chapter ends with one or two.
  Solutions are hidden in a collapsible block, so peek only when you're
  stuck.
- **Boxes like this one** hold background information you can skip on a
  first read:

  > 💡 **Behind the scenes:** extra detail for the curious.

The commands are written for **macOS or Linux**. On Windows, use
[WSL](https://learn.microsoft.com/windows/wsl/install) and follow along
inside its terminal.

Ready? Start with [chapter 1: Why relations?](01-why-relations.md)
