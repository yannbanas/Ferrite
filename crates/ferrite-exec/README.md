# ferrite-exec

Walks a `ferrite-planner` physical plan against a
`ferrite_common::StorageEngine` / `Catalog`, firing `ferrite-proc` triggers
on every row written.

## Session

```rust
let session = Session::new(&storage, &catalog, &procs, caller_identity)
    .with_indexes(&indexes);

let result = session.execute(txn, &plan)?;
```

A `Session` binds the engines to one caller's `Identity`. That identity is
what every permission check and every trigger sees, so authorization is not
something the caller can route around — there is no path into the executor
that does not carry one.

`execute` returns:

| | |
| --- | --- |
| `QueryResult::Rows { schema, rows }` | queries |
| `QueryResult::Affected(n)` | `INSERT`/`UPDATE`/`DELETE`; rows skipped by a trigger are not counted |
| `QueryResult::Value(v)` | `CALL` |

## Order of operations

Every statement goes through the same sequence, and each step can stop it:

1. **Statement permission** — `Select`/`Insert`/`Update`/`Delete` checked
   against the caller's roles *before* anything reaches storage.
2. **Stale-plan check** — the planner baked the table schema into the plan;
   if the catalog now disagrees, the plan's column positions cannot be
   trusted and it is rejected. This is what makes plan caching safe to add
   later.
3. **`BEFORE` trigger** — per row, for writes. A trigger may allow, rewrite
   (`Replace`), skip the row, or refuse the statement by returning
   `Err(FerriteError::PermissionDenied(..))`.
4. **Row validation** — arity, nullability, and type assignability against
   the schema. Runs *after* triggers, so a rewritten row is checked too.
   Widening (`Int4 → Int8 → Float8`) is implicit; nothing else is, and it is
   *applied*, not merely permitted — a value stored under a variant its
   column does not declare would be read back and put on the wire under the
   column's OID.
5. **Storage write**.

`UPDATE` evaluates its assignments against the row's pre-image, as SQL
requires, and hands the trigger both versions (`row` = new,
`ctx.old_row()` = old).

## Execution model

Single-threaded and materializing: each node collects its input into a
`Vec<Tuple>` rather than pulling lazily. `docs/architecture.md` cuts
parallel query execution for v1, and materializing keeps the borrow story
trivial while `StorageEngine::scan`'s iterator borrows the engine. The cost
is memory proportional to intermediate result size — a real Volcano-style
pull pipeline is the natural v2 change, and the node set does not have to
change for it.

A `Tuple` carries an optional `RowId`, present only while the row comes
straight from storage. Projections drop it, which is why `UPDATE`/`DELETE`
sources may not contain one; the planner enforces this
(`PhysicalPlan::preserves_row_identity`).

## Index access

`IndexProvider` is the runtime counterpart to the planner's
`IndexCatalog`: an equality probe returning `RowId`s. It is a separate
trait because `ferrite_common::StorageEngine` has no index vocabulary in
the v0 contract. Wiring one is optional — without it an `IndexScan` still
executes correctly, degrading to a sequential scan filtered on the index
key and logging a `tracing::warn!`.

## Three-valued logic

`eval.rs` implements SQL `NULL` semantics: a comparison with `NULL` is
`NULL`, `false AND NULL` is `false`, `true OR NULL` is `true`, and a
`WHERE` predicate keeps a row only when it evaluates to exactly `true`.
Integers compare exactly across `Int4`/`Int8`; mixing with `Float8` falls
back to floating point. Incompatible types are a `TypeMismatch`, not a
silent `false`.

## Tests

`tests/end_to_end.rs` runs the whole chain — SQL text → `ferrite-sql` →
planner → physical plan → executor — against the in-memory
`StorageEngine`/`Catalog` in `tests/support/`. Those are test scaffolding, not a second engine:
`ferrite-storage` (Agent 1) and `ferrite-catalog` (Agent 2) own the real
ones, and there is no MVCC visibility in the fakes.

## Known limits

- No DDL: `CREATE TABLE` and friends are not in the v1 plan set.
- No transaction control statements; the caller passes a `TxnId` it opened
  itself. A statement that fails halfway leaves earlier rows written —
  undoing them is `ferrite-storage`'s `abort`, which the caller must
  invoke.
- No `ORDER BY`, no aggregates, no joins — the planner does not produce
  those nodes.
- `Affected(n)` counts rows written, not rows matched.
