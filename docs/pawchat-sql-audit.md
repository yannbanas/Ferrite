# PawChat SQL audit

What the PawChat application **actually emits**, measured — not what the SQL
standard allows.

## Method

Every string literal in the PawChat sources was extracted with a real
JavaScript/TypeScript string scanner (backtick, single and double quoted,
comment-aware, `${…}` substitutions collapsed to a placeholder), then filtered
to those that begin with a SQL leading keyword. Constructs and function calls
were counted with per-construct regexes over that corpus.

| | |
|---|---|
| Sources scanned | `pawchat/src/**` + `pawchat/ws-server/**`, `*.ts`/`*.tsx` |
| SQL call sites (`.prepare(`/`.exec(`) | 1190 across 181 files |
| SQL literals extracted | 1375 |
| Literals in real DB code (`api/`, `lib/`, `ws-server/`) | 1287 across 181 files |
| Distinct constructs catalogued | 51 |
| Distinct SQL functions called | 13 |

The 88 literals outside `api/`/`lib/`/`ws-server/` are React page components
whose literals begin with a SQL word by coincidence (CSS `drop-shadow(…)`,
`rgba(…)`); they are excluded from every count below.

`src/lib/useSpacetimeMetaverse.ts` is **not** SQLite — it holds SpacetimeDB
subscription queries (the source of the only 6 `:named` parameters found).
It is out of scope for Ferrite and excluded.

## Construct catalogue

Counts are occurrences / distinct files, over the 1287 real literals.

### Already supported before this audit

| Construct | Occ | Files | Notes |
|---|---:|---:|---|
| positional parameter `?` | 1774 | 162 | mapped to `$n` by the translator |
| table alias (`FROM t a`, `AS`) | 741 | 166 | |
| `SELECT` | 675 | 171 | |
| `DEFAULT` (DDL) | 259 | 1 | `db.ts` schema only |
| column alias `AS` | 194 | 67 | |
| `ALTER TABLE … ADD COLUMN` | 194 | 1 | `db.ts` migration block |
| `DELETE` | 187 | 68 | |
| `UPDATE` | 135 | 69 | |
| `INSERT` | 134 | 79 | |
| `ORDER BY` | 129 | 71 | |
| `COUNT(*)` | 79 | 42 | |
| `IF NOT EXISTS` | 76 | 1 | |
| `JOIN` (inner) | 74 | 37 | |
| `CREATE TABLE` | 69 | 1 | |
| `PRIMARY KEY` | 69 | 1 | |
| `LIMIT` | 56 | 42 | |
| `ORDER BY … DESC` | 52 | 41 | |
| `LEFT JOIN` | 30 | 17 | |
| `IN (list)` | 27 | 21 | |
| `GROUP BY` | 18 | 13 | |
| `IS NOT NULL` / `IS NULL` | 22 | 13 | |
| `ORDER BY` multi-column | 12 | 12 | |
| `DISTINCT` | 11 | 7 | |
| `COUNT(DISTINCT …)` | 8 | 4 | |
| `CREATE INDEX` | 7 | 1 | |
| `LIKE` | 5 | 3 | **behaviour differs — see below** |
| `NOT IN (list)` | 4 | 3 | |
| `OFFSET` | 1 | 1 | |
| `HAVING` | 1 | 1 | |
| aggregates `count/sum/avg/min/max` | 113 | 49 | |

### Gaps found, ranked by real frequency

| Construct | Occ | Files | Status after this audit |
|---|---:|---:|---|
| `datetime(…)` scalar call | 115 | 32 | **implemented** |
| `excluded.` reference | 56 | 7 | **implemented** |
| `INSERT OR IGNORE` | 30 | 17 | **implemented** |
| `COALESCE(…)` | 13 | 7 | **implemented** |
| `CASE … WHEN … END` | 13 | 9 | **implemented** |
| `COLLATE NOCASE` | 10 | 8 | **implemented** (see behaviour note) |
| `ON CONFLICT … DO UPDATE` | 10 | 7 | **implemented** |
| `IN (SELECT …)` | 10 | 8 | **implemented** (uncorrelated) |
| `date(…)` | 9 | 2 | **implemented** |
| `INSERT OR REPLACE` | 9 | 7 | **implemented** |
| scalar subquery in select list | 9 | 7 | *not implemented* — see below |
| `CAST(… AS …)` | 7 | 3 | **implemented** |
| `lower(…)` | 4 | 3 | **implemented** |
| `substr(…)` | 3 | 2 | **implemented** |
| `hex(…)` / `randomblob(…)` | 4 | 2 | **implemented** |
| `UNION ALL` | 2 | 2 | *not implemented* — see below |
| `PRAGMA table_info(…)` | 1 | 1 | *not implemented* — see below |
| subquery in `FROM` | 1 | 1 | *not implemented* — see below |
| `ILIKE` | 0 | 0 | **implemented** — migration target for SQLite `LIKE` |

`EXISTS`, `NOT EXISTS`, `GLOB`, `LIMIT -1`, `RETURNING`, window functions,
CTEs, `INTERSECT`/`EXCEPT`, `UPDATE … FROM`, `DELETE … USING`, `NULLS
FIRST/LAST`, `ESCAPE`, `GROUP_CONCAT`, `strftime`, `julianday`, `unixepoch`,
`json_extract` and every other JSON1 function: **zero occurrences**. PawChat
does not use them, so they are deliberately not implemented.

### Functions PawChat actually calls

Complete list — there are exactly 13.

```
115  32f  datetime      9  5f  max        3  2f  substr
 92  46f  count         7  5f  sum        3  2f  min
 13   7f  coalesce      7  3f  cast       2  2f  hex
  9   2f  date          4  3f  lower      2  2f  randomblob
                                          2  2f  avg
```

`datetime` modifiers used, exhaustively:

```
106  datetime('now')
  4  datetime('now', ?)              4  datetime('now', '-30 days')
  2  datetime('now', '-7 days')      2  datetime('now', '-14 days')
  1  datetime('now', '-1 day')       1  datetime('now', '-1 hour')
  1  datetime('now', '-5 minutes')   1  datetime('now', '-2 minutes')
  1  datetime('now', '-60 seconds')
```

### Transactions

`better-sqlite3`'s `.transaction()` wrapper is used at 18 call sites. It emits
`BEGIN`/`COMMIT`/`ROLLBACK`, and `SAVEPOINT`/`RELEASE` only when a transaction
function is *nested inside another one*. No nesting exists in PawChat — every
`.transaction()` call is top-level — so plain `BEGIN`/`COMMIT`/`ROLLBACK`, which
Ferrite already has, is sufficient. Savepoints are not needed.

`PRAGMA journal_mode` / `busy_timeout` are set through `db.pragma()` at startup;
they are SQLite storage-engine knobs with no Postgres equivalent and no
application-visible semantics to preserve.

## Behaviour differences that cannot be translated away

These are the places where SQLite and Postgres genuinely disagree. They are
documented rather than hidden behind a translation that looks right and
silently returns different rows.

### 1. `LIKE` case sensitivity

SQLite's `LIKE` is **case-insensitive for ASCII** by default
(`case_sensitive_like` defaults off). Postgres — and Ferrite — make `LIKE`
case-**sensitive**, with `ILIKE` as the insensitive form.

Every `LIKE` in PawChat relies on the SQLite default:

- `src/app/api/admin/users/route.ts:18` — `WHERE id != 'bot-0' AND (username LIKE ? OR display_name LIKE ?)`, admin user search
- `src/app/api/store/route.ts:88` — `AND (title LIKE ? OR description LIKE ?)`, store search
- `src/lib/channelStore.ts:150` — `AND m.content LIKE ?`, in-channel message search

Ported unchanged to Ferrite, these three searches become case-sensitive and
stop matching what users expect. **They must be rewritten as `ILIKE`.**

The remaining three `LIKE`s are prefix tests against a lowercase literal
(`u.display_font LIKE 'pf:%'`) where casing cannot vary; those are safe as-is.

Ferrite now implements `ILIKE`/`NOT ILIKE`. `LIKE` keeps Postgres semantics —
it is not silently made insensitive, because that would break the many places
where case sensitivity is the correct behaviour.

`ILIKE` folds case with Rust's `to_lowercase()`, which is full Unicode. SQLite's
built-in `LIKE` folds **ASCII only**: `'É' LIKE 'é'` is false in SQLite and true
under Ferrite's `ILIKE`. For PawChat's usernames and search boxes the Unicode
behaviour is the desirable one, but it is a difference, not an equivalence.

### 2. `COLLATE NOCASE`

PawChat's case-insensitive username lookup is built on `COLLATE NOCASE`:

- `src/lib/db.ts:39` — `username TEXT NOT NULL UNIQUE COLLATE NOCASE` (the
  column-level declaration that makes the unique index case-insensitive)
- `src/lib/userStore.ts:30`, `src/app/api/friends/route.ts:85`,
  `src/app/api/e2e/route.ts:63`,
  `src/app/api/servers/[serverId]/badges/[badgeId]/assign/route.ts:30` —
  `WHERE username = ? COLLATE NOCASE`
- `src/app/api/admin/metaverse/rooms/route.ts:31`,
  `src/app/api/metaverse/public-rooms/route.ts:28`, `src/lib/botFlows.ts:84` —
  `ORDER BY … COLLATE NOCASE ASC`

Ferrite now parses `COLLATE NOCASE` and lowers it to a case-folding wrapper on
the expression, so `=` and `ORDER BY` behave case-insensitively as the
application expects.

Two honest caveats:

- **Scope.** Ferrite applies `NOCASE` where it is written on an *expression*.
  Postgres has no per-column default collation of this shape; the
  `UNIQUE COLLATE NOCASE` on the `users.username` column is accepted and
  recorded, but the resulting index is a plain unique index. Two usernames that
  differ only by case would be rejected by SQLite and accepted by Ferrite. The
  application already lowercases at registration, so this has no live effect
  today — but it is not an equivalence, and a production port should add a
  `UNIQUE` index on the folded value.
- **Fold width.** `NOCASE` in SQLite folds ASCII A–Z only. Ferrite's fold is
  full Unicode, same divergence as `ILIKE` above.

### 3. `INSERT OR REPLACE` is not `ON CONFLICT DO UPDATE`

`INSERT OR REPLACE` **deletes** the conflicting row and inserts a new one.
`ON CONFLICT DO UPDATE` **updates** it in place. The two differ in three
observable ways:

- columns not named in the `INSERT` are reset to their defaults by
  `OR REPLACE`, but preserved by `DO UPDATE`;
- `OR REPLACE` fires delete triggers and `ON DELETE CASCADE`;
- `OR REPLACE` assigns a fresh rowid / autoincrement id.

Ferrite translates `INSERT OR REPLACE` to `ON CONFLICT DO UPDATE SET` over the
inserted columns. All 9 PawChat sites list **every** non-default column of the
target table (`event_rsvps`, `server_bans`, `server_mutes`, `ticket_configs`,
`server_badge_assignments`, …) and none of those tables carry delete triggers or
child cascades, so the translation is exact **for these call sites**. It is not
exact in general.

### 4. Weak typing

SQLite compares `'5' = 5` as true after affinity conversion; Ferrite, like
Postgres, does not. `CAST(cp.target_id AS INTEGER)` at
`src/app/api/channels/[channelId]/permissions/route.ts:29` and
`CAST(mr.role_id AS TEXT)` at `src/lib/channelStore.ts:416` exist precisely
because the application stores a role id as text in one table and as an integer
in another. Those two `CAST`s are now supported and make the comparison
explicit, so those joins port correctly. Any *implicit* cross-type comparison
elsewhere would behave differently — none was found in the corpus, but the
schema's `TEXT`-typed id columns make it a standing risk.

### 5. `datetime('now')` resolution and zone

SQLite's `datetime('now')` returns UTC `'YYYY-MM-DD HH:MM:SS'`, second
resolution, no zone suffix. Ferrite's implementation returns the same string
shape in UTC so that the `TEXT`-typed `created_at` columns PawChat uses keep
sorting and comparing exactly as before. It is deliberately **not** mapped to
`now()`/`CURRENT_TIMESTAMP`, whose Postgres output format
(`2026-08-10 12:34:56.789012+00`) would break the lexicographic comparisons the
application performs against stored strings.

Within a statement, SQLite's `datetime('now')` is constant. Ferrite evaluates it
once per statement for the same reason.

## Deliberately not implemented

| Construct | Occ | Why |
|---|---:|---|
| correlated scalar subquery in select list | 9 | Needs a per-outer-row subplan executor. Every occurrence is a `(SELECT COUNT(*) … WHERE x = outer.id)` counter that can be rewritten as a `LEFT JOIN … GROUP BY` in the application. Rejected with a clear plan error, never mis-planned. |
| `UNION ALL` | 2 | `src/app/api/fonts/route.ts:26` and `src/lib/channelStore.ts:692`. Both are two-branch merges the application can do in TypeScript. |
| subquery in `FROM` | 1 | Parses; the planner rejects it. Same call site as the second `UNION ALL`. |
| `PRAGMA table_info(…)` | 1 | `src/lib/dmStore.ts:4`, a runtime column-existence probe. Has no Postgres syntax; the port should query `information_schema.columns`, which `ferrite-catalog` already serves. |

All four fail loudly with a `FerriteError::Plan` naming the construct. None is
silently accepted.

## Coverage after this audit

Of the 1287 real SQL literals, the constructs left unsupported appear in **12
literals across 10 files** (9 correlated scalar subqueries, 2 `UNION ALL`, 1
`PRAGMA`, with one file overlapping). Every other construct and all 13 functions
PawChat calls are now parsed, planned and executed by Ferrite.
