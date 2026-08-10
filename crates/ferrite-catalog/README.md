# ferrite-catalog

`SystemCatalog`, the implementation of `ferrite_common::Catalog`: name
resolution (`schema.name` → `TableId`) and schema lookup (`TableId` →
`Schema`).

The catalog is stored as ordinary tables through a
`ferrite_common::StorageEngine`. There is no metadata file format, no bespoke
serialisation, no second durability path to get right — whatever crash
recovery `ferrite-storage` gives user tables, the catalog gets for free.

```rust
use std::sync::Arc;
use ferrite_catalog::SystemCatalog;
use ferrite_common::Catalog;

let catalog = SystemCatalog::bootstrap(storage.clone())?; // fresh database
let catalog = SystemCatalog::open(storage.clone())?;      // existing database

let id = catalog.create_table("public", "users", schema)?;
assert_eq!(catalog.table_id("public", "users")?, Some(id));
```

## Layout

Two tables, both in the reserved schema `ferrite_catalog`, both self-describing
— they have rows describing themselves, so `table_schema(TABLES_TABLE_ID)`
answers from the same data path as any user table.

`ferrite_catalog.ferrite_tables` (`TableId` 1) — one row per table:

| column | type | |
| --- | --- | --- |
| `table_id` | `Int8` | the `TableId`, widened to `i64` (`DataType` has no unsigned type) |
| `schema_name` | `Text` | `public`, `app`, … |
| `table_name` | `Text` | |

`ferrite_catalog.ferrite_columns` (`TableId` 2) — one row per column:

| column | type | |
| --- | --- | --- |
| `table_id` | `Int8` | owning table |
| `ordinal` | `Int4` | position in the row; rows are re-sorted by it on read |
| `column_name` | `Text` | |
| `data_type` | `Text` | `boolean`/`int4`/`int8`/`float8`/`text`/`timestamp`/`uuid`/`json` |
| `nullable` | `Boolean` | |

`data_type` is stored as a stable lower-case name rather than a numeric tag:
adding a type must never renumber the ones already on disk, and an unknown name
read back is a clean error instead of a silent mis-decode.

### Table id allocation

- `0` — never allocated.
- `1`, `2` — the catalog tables above.
- `3..16` — reserved for future catalog tables (roles, procedures, triggers);
  `FIRST_USER_TABLE_ID` is `16`.
- `16..` — user tables, allocated as `max(existing) + 1`. Ids are **not**
  recycled after a drop, so a stale `TableId` held by another component can
  never silently resolve to a different table.

`SystemCatalog::is_system_table` reports whether an id is in the reserved
range; `drop_table` refuses those, and `create_table` refuses the
`ferrite_catalog` schema.

### The in-memory index

Storage is the source of truth. A `RwLock`-guarded index is kept beside it so
`table_id`/`table_schema`/`list_tables` — called on every statement — do not
scan, and is updated in lockstep with each committed write. `open()` rebuilds
it from the catalog tables alone, and `reload()` does the same on demand for a
process that did not make the change itself. Nothing is ever read from the
index that was not first written to storage.

## Transaction semantics — and a caveat

`ferrite_common::Catalog` methods take no `TxnId`, while every
`StorageEngine` method requires one. Each catalog operation therefore opens its
own transaction and commits it (aborting on any error, so a failed
`create_table` leaves no half-written rows).

The consequence: **DDL is not transactional with the surrounding user
transaction.** `BEGIN; CREATE TABLE t (…); ROLLBACK;` leaves `t` in place. That
is a property of the shared trait, not of this implementation — the internals
here (`create_in`, `drop_in`) already do all their work inside one caller-
supplied `TxnId` and would need only a signature change to become
transactional. See the Agent 2 report for the proposed change.

Validation performed on `create_table`: non-empty schema and table names, at
least one column, non-empty column names, no duplicate column names, and the
`ferrite_catalog` schema is refused.

Error mapping, given the variants `FerriteError` currently offers:

| situation | error |
| --- | --- |
| unknown `TableId` | `TableNotFound` |
| duplicate table, invalid definition, corrupt catalog row | `Storage("catalog: …")` |
| dropping a catalog table, writing to the `ferrite_catalog` schema | `PermissionDenied` |

`Storage` is the generic escape hatch, used here because `FerriteError` has no
`ObjectAlreadyExists`/`InvalidDefinition` variant yet; messages are prefixed
`catalog:` so they stay legible. Also flagged in the Agent 2 report.

## `memory::MemoryStorage`

A minimal in-memory `StorageEngine` behind the `test-util` feature, written so
this crate could be built and tested before `ferrite-storage` existed. Reusable
by any other crate that wants a stand-in:

```toml
[dev-dependencies]
ferrite-catalog = { workspace = true, features = ["test-util"] }
```

Rows live in a `BTreeMap` behind one `Mutex`. Writes apply immediately and are
reversed from a per-transaction undo log on `abort`, so all-or-nothing works
for a single writer; `snapshot` returns a plausibly shaped `Snapshot` but
nothing enforces it. **There is no isolation between concurrent transactions**
— a transaction sees other transactions' uncommitted writes. It is a test
double, not a reference implementation, and it is off by default so it never
reaches the server binary.

Extras beyond the trait, for assertions: `rows(table)`, `table_exists(table)`,
`committed_transactions()`.

## Tests

```bash
cargo test -p ferrite-catalog --all-targets
```

`tests/catalog.rs` covers bootstrap (including the self-describing rows),
create/drop/list/lookup, schema qualification, duplicate and invalid
definitions, refusal to drop catalog tables, id allocation, use behind
`dyn Catalog`, and — the test that matters most — reopening a `SystemCatalog`
over the same storage and checking every schema round-trips through storage
rather than through the in-memory index.
