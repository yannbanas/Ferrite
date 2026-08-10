# ferrite-catalog

`SystemCatalog`, the implementation of `ferrite_common::Catalog`: name
resolution (`schema.name` → `TableId`), schema lookup (`TableId` → `Schema`)
and index metadata (`IndexCatalog`, defined here).

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

Three tables, all in the reserved schema `ferrite_catalog`, all
self-describing — they have rows describing themselves, so
`table_schema(TABLES_TABLE_ID)` answers from the same data path as any user
table.

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

`ferrite_catalog.ferrite_indexes` (`TableId` 3) — one row per **index column**:

| column | type | |
| --- | --- | --- |
| `index_id` | `Int8` | |
| `table_id` | `Int8` | indexed table |
| `index_name` | `Text` | scoped to the schema of its table |
| `ordinal` | `Int4` | position in the index key |
| `column_name` | `Text` | |
| `is_unique` | `Boolean` | |

One row per column rather than a separate index/index-column pair of tables:
an index always has at least one column, so there are no orphan rows to worry
about, and the cost is repeating the name and uniqueness flag per column. There
is no access-method column because `docs/architecture.md` cuts everything but
B-tree in v1; that is the field to add when a second index type appears.

`data_type` is stored as a stable lower-case name rather than a numeric tag:
adding a type must never renumber the ones already on disk, and an unknown name
read back is a clean error instead of a silent mis-decode.

### Object id allocation

Tables and indexes share one id space, so an index id can later become a
storage object id without colliding with a table's.

- `0` — never allocated.
- `1`, `2`, `3` — the catalog tables above.
- `4..16` — reserved for future catalog tables (roles, procedures, triggers);
  `FIRST_USER_TABLE_ID` is `16`.
- `16..` — user tables and indexes, allocated as `max(existing) + 1`. Ids are
  **not** recycled after a drop, so a stale `TableId` held by another component
  can never silently resolve to a different object.

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

## Index metadata

`IndexCatalog` (`create_index`, `drop_index`, `index`, `index_by_name`,
`indexes_for`) now lives in `ferrite-common` and is implemented here; this
crate re-exports it so callers need only one import. It belongs in the catalog
and not in the planner: it is persistent schema, it must be dropped with its
table, and the planner (choosing an access path) and the executor (maintaining
the index on write) must get the same answer.

`CREATE INDEX` records metadata and nothing more: `ferrite-storage` has no
secondary-index structure yet, only the primary per-table B-tree. The planner
does pick an `IndexScan` from that metadata, and the executor then degrades it
to a filtered sequential scan with a `tracing::warn!` — correct, not fast.

Validated on `create_index`: the table exists and is not a catalog table, the
column list is non-empty and every column exists in the table's schema, no
duplicate columns, and the name is not already taken in that schema. Dropping a
table drops its indexes with it.

## Transaction semantics

`ferrite_common::Catalog` methods take no `TxnId`, while every `StorageEngine`
method requires one — and the `StorageEngine` doc-comment explicitly rules out
an ambient transaction context. Both halves of that are handled:

- The **primitives take an explicit `TxnId`**: `create_table_in`,
  `drop_table_in`, `create_index_in`, `drop_index_in`. DDL can therefore join a
  transaction the executor owns, and several DDL statements can commit
  atomically together.
- The **trait methods wrap those primitives** in a transaction of the catalog's
  own, opened and committed per call, aborting on any error so a failed
  `create_table` leaves nothing half-written.

The consequence, for as long as the trait has no `TxnId` parameter: DDL called
*through the trait* is not transactional with the surrounding user transaction,
so `BEGIN; CREATE TABLE t (…); ROLLBACK;` leaves `t` in place unless the
executor routes through the `*_in` methods.

One caveat on the `*_in` methods: they update the in-memory index
optimistically, since they cannot know whether the caller will commit. **If the
transaction is aborted, call `reload()`** — that is the documented contract, and
it is what the trait methods do for themselves on failure. Adding `TxnId` to
`Catalog` in `ferrite-common` would fix the first half but not this one; a
proper fix needs the executor to tell the catalog when a transaction that ran
DDL aborted.

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
definitions, refusal to drop catalog tables, id allocation, index
create/lookup/drop and cascade-on-drop-table, DDL inside a caller-owned
transaction (committed and aborted), use behind `dyn Catalog`/`dyn
IndexCatalog`, and — the tests that matter most — reopening a `SystemCatalog`
over the same storage and checking every schema and index key order round-trips
through storage rather than through the in-memory index.
