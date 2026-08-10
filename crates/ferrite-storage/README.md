# ferrite-storage

The storage engine: checksummed fixed-size pages, a B+-tree per table, MVCC
row versions, and a crash-recovery journal. It is the only implementor of
`ferrite_common::StorageEngine`, and nothing above that trait knows how a
page is laid out.

```rust
use ferrite_common::{Row, StorageEngine, Value};
use ferrite_storage::FerriteStorage;

let storage = FerriteStorage::open("/var/lib/ferrite")?;
let txn = storage.begin()?;
storage.create_table(txn, 1)?;
let row = storage.insert(txn, 1, Row::new(vec![Value::Int8(42)]))?;
storage.commit(txn)?;
```

A database is a directory holding two files: `ferrite.db` (the pages) and
`ferrite.wal` (the journal).

---

## Page format

Pages are **8 KiB**, fixed at compile time. That is Postgres's size, which
keeps the balance between per-page header overhead and read amplification in
well-understood territory; making it configurable would leak into every
offset computation in the crate and buy nothing in v1.

```text
byte  0..4    checksum   CRC-32C of bytes 4..8192
byte  4..12   lsn        journal sequence number of the last write
byte 12..13   kind       Meta | Leaf | Internal | Overflow | TableHeader | Clog | Free
byte 13..14   flags      reserved, always 0
byte 14..16   slot_count
byte 16..18   free_end   start of the item area, which grows downward
byte 18..20   reserved
byte 20..24   extra      kind-specific u32
byte 24..     slot array slot_count entries of (u16 offset, u16 len)
     ..free_end          free space
free_end..8192           item area
```

**Checksums are not optional.** `docs/architecture.md` lists them as an
always-on requirement, so there is no flag to disable them: every page is
verified on the way in from disk and recomputed on the way out. A page that
fails verification produces `FerriteError::Storage` rather than being
trusted. CRC-32C is implemented inside the crate (`src/crc.rs`) so the
on-disk algorithm cannot drift with a dependency upgrade.

Slotted pages give variable-length items with a stable ordering: the slot
array grows up from the header, item bytes grow down from the end, and
deletions leave holes that `compact()` reclaims when a page next needs the
room.

`extra` means something different per kind — the next leaf for a `Leaf`, the
leftmost child for an `Internal`, the next page in the chain for `Overflow`
and `Free`. Pages that hold a single fixed record (`Meta`, `TableHeader`,
`Clog`) ignore the slot array and use the body directly.

Page 0 is always the meta page: magic, format version, page size, allocator
high-water mark, free-list head, `next_txn_id`, the catalog tree root, and
the commit-log directory.

## B+-tree

One structure serves two purposes, keyed by `u64` with arbitrary-length byte
payloads:

- the **catalog** tree maps `TableId` to that table's header page;
- each **table** tree maps `RowId` to that row's version chain.

Payloads live only in leaves, and leaves are chained left to right, so a
sequential scan walks the chain instead of climbing back through internal
nodes. Payloads above 2000 bytes move to a linked list of overflow pages,
leaving an eight-byte descriptor in the leaf; that keeps at least three
entries per leaf whatever the row size, which is what lets a split always
make progress.

A table's header page is separate from its tree root. The root moves when it
splits; the header page never does, so the catalog entry for a table is
written once at creation and never rewritten. The header also holds the
`RowId` allocator, which is intentionally **not** transactional — an aborted
transaction leaves a gap in the sequence, exactly like a Postgres sequence,
because making it transactional would turn every concurrent insert into a
write conflict on a single counter.

Deletion removes a key but never merges or rebalances nodes. This matters
less than it sounds: in an MVCC engine a row deletion is a version-chain
update, not a key removal, so keys only disappear when pruning retires an
entire chain. Space from dropped tables is returned to the free list in one
pass.

## MVCC

Every version of a row carries `xmin` (the transaction that created it) and
`xmax` (the transaction that deleted it, or 0). Unlike Postgres, which
stores each version as an independent heap tuple, Ferrite keeps **all
versions of a row in a single B-tree payload**, newest first:

```text
u16 version_count
repeated:
  u64 xmin
  u64 xmax          0 when live
  u32 len
  len bytes         the encoded Row
```

The trade-off is deliberate. Visibility resolves in one B-tree descent with
no extra page hops; `RowId` stays stable across updates, which is what the
`StorageEngine` contract's "update by `RowId`" needs; and reclaiming dead
versions is a local rewrite of one payload rather than a separate vacuum
pass over the whole table. The cost is that a row updated many times inside
one long snapshot window carries all of those versions in one payload. Chains
are pruned on every write, so the steady state for ordinary work is one or
two versions.

### Visibility

`ferrite_common::Snapshot` documents the rule as "visible if its creating
transaction committed before `xmin` and is not itself in-flight", so `xmin`
is used here as an **exclusive upper bound** — the value of `next_txn_id`
when the snapshot was taken — with `active_at_start` listing the
transactions that were still running below it. (The field name is
Postgres-shaped but the semantics are Postgres's `xmax`; see the agent
report.) A transaction id is visible when:

```text
xid == snapshot.txn_id                     own writes, always visible
xid <  snapshot.xmin
  && !active_at_start.contains(xid)
  && commit_log.is_committed(xid)
```

and a version is visible when its `xmin` is visible and its `xmax` either is
unset or is not visible.

`snapshot(txn)` re-takes the transaction's read snapshot and returns it, so
the executor gets the choice the trait describes: call it once per
transaction for repeatable reads, once per statement for read-committed.

### Write conflicts

Before an update, delete or drop, the newest version must be the one this
transaction can see. If it is not — because a concurrent transaction is
still writing it, or committed a write after this snapshot — the operation
fails with `FerriteError::SerializationFailure` instead of overwriting it.
There is no blocking and no wait: with a single-threaded executor there is
nothing to wait for, and first-updater-wins is what keeps a lost update
impossible. A version whose deleter is visible reports `RowNotFound`
instead.

### Commit status

There is no undo log. A separate commit bitmap (`src/clog.rs`, one bit per
transaction id, stored in ordinary pages listed in the meta page) is the
authority on outcomes, exactly as `pg_xact` is for Postgres. A transaction
that never set its bit — rolled back, or interrupted by a crash — is
aborted, and everything it wrote is simply invisible. That is why recovery
needs no undo pass at all, and why an abort costs nothing beyond forgetting
the transaction.

Dead versions are removed on the write path: aborted `xmin`s are dropped,
aborted `xmax`es are cleared, and versions deleted by a transaction older
than every live snapshot are discarded. When a chain empties, the key leaves
the tree.

## Journal

`docs/architecture.md` left the journal format to this crate. Ferrite v1
uses a **physical redo journal of full page images**.

```text
u32 payload_len
u32 crc32c(payload)
payload:
  u8 kind          PageImage | Commit | Abort | Checkpoint
  kind-specific bytes
```

`commit` appends an image of every page dirtied so far, appends a commit
record, and fsyncs before returning. Recovery replays the images in order
over the data file and stops at the first record that fails its length or
CRC check — the normal signature of a write interrupted by a power cut.
Records before the tear are unaffected because each is checksummed
independently.

**Why full images rather than physiological logging.** Logging operations
instead of pages writes far less, but it needs per-page LSN comparison during
replay, an idempotent redo path for every structural B-tree operation, and a
torn-page defence anyway — Postgres writes full pages after each checkpoint
for precisely that reason. Full images are correct by construction:
replaying one twice is the same as replaying it once, and a page the
operating system only half-wrote is overwritten wholesale rather than
patched. The price is write amplification, and it is the first thing to
revisit if commit throughput matters. Nothing above the journal would have
to change.

Uncommitted work is allowed to reach both the journal and the data file;
the commit bitmap makes it harmless. Two ordering rules make that safe:

- **Group flush.** When the page cache has to evict a page whose changes the
  journal has not seen, *every* dirty page is journalled, not just the
  victim. A B-tree split dirties a parent and its new child together, and
  recovering the parent without the child would leave a node pointing at a
  page the data file never received.
- **Meta first.** The meta page carries `next_txn_id`, and a page image
  mentioning transaction N must never become durable while the allocator
  still believes N is unused — after a crash that would let a different
  transaction be handed the same id and inherit the crashed one's row
  versions.

`checkpoint()` writes every cached change into the data file, fsyncs it, and
truncates the journal; after that the data file alone is a complete
database. It is optional — recovery reaches the same state from the journal
— but it bounds both recovery time and journal size. The journal is also
truncated at the end of recovery, once its records have been applied and the
data file fsynced.

The engine does no work in `Drop`. Dropping a `FerriteStorage` therefore
leaves the files exactly as a power cut would, which is what most of the
crash tests rely on; `tests/recovery.rs` also has the operating system kill
a real child process, so the guarantee does not rest on that alone.

## Concurrency

One engine-wide lock. The v1 executor is single-threaded, so page-level
latching would add contention machinery with nothing to contend for. The
lock does not serialise *transactions* — several may be open at once and
interleave their statements freely, which is the case MVCC has to get right.
Scans take the lock per step rather than holding it for the life of the
iterator. Replacing the lock with finer latching later changes no
visibility logic, only who may touch a page.

## Configuration

`StorageConfig` has two knobs: `cache_pages` (default 1024, i.e. 8 MiB) and
`fsync` (default on). Turning `fsync` off trades durability for speed and is
only appropriate for data you are willing to lose.

## Known limitations

- **No node merging.** Deletions leave underfull B-tree nodes in place.
  Space comes back when a table is dropped, not when its rows are.
- **Aborted DDL leaks pages.** A `create_table` that is rolled back leaves
  its header and root pages allocated; the table is correctly invisible, but
  the space is not reclaimed until a future vacuum exists.
- **Dropped tables are only reclaimed when nothing else is open.** With no
  lock manager, freeing a dropped table's pages while another snapshot might
  still read them would be unsafe, so the space is left allocated unless the
  committing transaction is the last one running.
- **No DDL locking.** Postgres takes an `ACCESS EXCLUSIVE` lock for
  `DROP TABLE`; Ferrite v1 has nowhere to put one. Concurrent readers see
  the drop through the normal snapshot rules rather than being blocked.
- **Commit-bitmap ceiling.** The directory in the meta page holds 2032
  segments of 65 344 transactions, so 1.32 x 10^8 transactions per database
  (`ferrite_storage::MAX_TXN_ID`). Lifting it means moving the directory out
  of the meta page or naming clog segments as files. It is a wall rather
  than a slope — commits stop dead — so `ferrite-server` samples the
  distance to it and warns from 70 % of the way, then on every sample past
  90 %; `ferrite_txn_id` / `ferrite_txn_id_ceiling` carry the same number.
- **Write amplification at commit.** Every dirty page is journalled in full;
  see the journal section.
- **Scans are `O(log n)` per row.** The cursor re-descends the tree for each
  step so it can release the lock between rows. Caching the current leaf
  would make it amortised constant.
- **No savepoints** and no transaction-id wraparound handling, both
  deliberately out of scope for v1 per `docs/architecture.md`.

## Ce qui est journalise pour l'operateur

Trois evenements sortent en `tracing::error!`/`warn!` avec un compteur en
face. Ils doivent etre vus avant de devenir un incident, et aucun ne
remonte forcement jusqu'a un humain par le chemin d'erreur — une requete
qui echoue est souvent retentee et l'erreur disparait.

| Evenement | Ou | Compteur |
| --- | --- | --- |
| Somme de controle de page invalide | `page.rs`, a la lecture | `ferrite_checksum_failures_total` |
| Ecriture / `fsync` / troncature en echec (disque plein, monte en lecture seule) | `pager.rs`, `wal.rs` | `ferrite_storage_write_failures_total` |
| Transaction ouverte anormalement longtemps | echantillonne par `ferrite-server` | `ferrite_oldest_transaction_seconds` |

Le dernier n'est pas cosmetique : une transaction laissee ouverte retient
l'horizon d'elagage MVCC pour toute la base, donc les versions mortes
s'accumulent tant qu'elle vit.

## Tests

```
cargo test -p ferrite-storage --all-targets
```

- `src/*` — unit tests for the CRC, the row codec, the page layout, the
  allocator and cache, and the B-tree.
- `tests/engine.rs` — the `StorageEngine` surface and isolation behaviour.
- `tests/mvcc_properties.rs` — `proptest` against a reference implementation
  of snapshot isolation, plus a no-lost-update property.
- `tests/recovery.rs` — crash recovery, including a torn journal tail, a
  corrupted page, a property test that crashes at arbitrary points, and a
  child process killed by the operating system.
