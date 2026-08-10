"""Translate a SQLite database into Ferrite's SQL subset.

    python tools/sqlite_to_ferrite.py app.db out
    FERRITE_REPLAY_DIR=out cargo test -p ferrite-server --test replay -- --ignored

Emits one `<table>.sql` per table under `out/`: the `CREATE TABLE`, then one
`INSERT` per row, separated by a sentinel line so a statement may itself
contain newlines. `_after.sql` holds what only makes sense once every table
exists — the index DDL, one query per shape a real application reads through
(`count(*)`, `ORDER BY ... LIMIT`, `LIKE`, a `JOIN` across a `<x>_id` column,
`GROUP BY ... HAVING`, `DISTINCT`), the `ALTER TABLE ... ADD COLUMN`
migrations an application replays at boot together with an `INSERT` that
leans on the `DEFAULT`s, and then the same queries again against the widened
tables. All of it derived from this schema rather than hand-written.
`types.md` records every type decision and
everything that had to be dropped; `crates/ferrite-server/tests/replay.rs`
replays the result and reports what got in and what answered.

Note when migrating off SQLite: `LIKE` is case-insensitive there and
case-sensitive in Ferrite, as in PostgreSQL. Row counts will differ — on this
database, `name LIKE '%a%'` finds 144 rows in SQLite and 136 in Ferrite, and
`ILIKE` is what reproduces the 144. `docs/pawchat-sql-audit.md` lists the
three call sites that have to be rewritten.

Type mapping, and why:

- `INTEGER -> BIGINT`. SQLite has no boolean type, so a flag and a small
  counter look identical in the data — PawChat's `is_public` is 0/1 but its
  `nsfw_level` is 0/1/2. Guessing would corrupt one of them, and `BIGINT` is
  lossless for both. Narrowing to `BOOLEAN` needs the application's own
  types, not the database's.
- `REAL -> DOUBLE PRECISION`.
- `TEXT` splits three ways on the *values*, since SQLite stores all three as
  text: every non-null value parsing as an ISO date gives `TIMESTAMP`, every
  non-null value parsing as a JSON object or array gives `JSON`, anything
  else stays `TEXT`. Ferrite's planner coerces the string literal to the
  column's type on insert, so no value has to be rewritten.
- `BLOB` has no counterpart in `ferrite_common::DataType`; such a column is
  dropped, unless it is empty in this database, in which case it becomes
  `TEXT`.

Dropped, because Ferrite v1 has no equivalent: `FOREIGN KEY`, `AUTOINCREMENT`
and `CHECK`. `PRIMARY KEY`, `NOT NULL` and `DEFAULT` are kept. A table-level
`UNIQUE (a, b)` is re-emitted as a `CREATE UNIQUE INDEX` in `_after.sql`,
because that is what an `INSERT OR IGNORE` with no explicit target conflicts
on. Ferrite enforces those keys, so `_after.sql` also carries statements that
are *expected to fail*, marked `-- @@EXPECT <sqlstate>@@`: re-inserting a row
that is already there must come back as a `23505`, not as a second row. Every identifier is quoted: Ferrite reserves nearly every
keyword, and application schemas are full of columns called `type`,
`position` and `content`.

`DEFAULT` is translated into the subset Ferrite stores — a literal, or the
current timestamp — and dropped with a note otherwise, since Ferrite refuses
a `DEFAULT` it cannot evaluate rather than accepting one it would ignore.
SQLite's `datetime('now')` and `CURRENT_TIMESTAMP` both map to
`CURRENT_TIMESTAMP`.

`_after.sql` also carries one statement per SQL construct the audit found
PawChat emitting (`ILIKE`, `COLLATE NOCASE`, `datetime`/`date`, `CASE`,
`CAST`, `coalesce`, `IN (SELECT ...)`, the upsert idioms), plus a dozen
queries copied verbatim from the PawChat sources. It is
where the two features an application actually depends on get exercised
against real data: one `ALTER TABLE ... ADD COLUMN` of each shape per table
(nullable, and `NOT NULL DEFAULT`, which is what PawChat's migrations look
like), and one `INSERT` per table naming only the columns that have neither
a default nor a nullable type — the shape that used to fail on a column the
application never mentions.
"""

import json
import itertools
import os
import re
import sqlite3
import sys

SENTINEL = "-- @@STATEMENT@@"
EXPECT = "-- @@EXPECT %s@@"
DB = sys.argv[1] if len(sys.argv) > 1 else ".data/pawchat.db"
OUT = sys.argv[2] if len(sys.argv) > 2 else "out"

TS = re.compile(r"^\d{4}-\d{2}-\d{2}([T ]\d{2}:\d{2}(:\d{2}(\.\d{1,6})?)?)?Z?$")


def is_timestamp(v):
    return isinstance(v, str) and bool(TS.match(v.strip()))


def is_json(v):
    if not isinstance(v, str):
        return False
    s = v.strip()
    if not s or s[0] not in "[{":
        return False
    try:
        json.loads(s)
        return True
    except Exception:
        return False


def quote(name):
    return '"' + name.replace('"', '""') + '"'


def literal(v):
    if v is None:
        return "NULL"
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        return repr(v)
    if isinstance(v, bytes):
        return None  # BLOB: no representation in Ferrite v1
    return "'" + str(v).replace("'", "''") + "'"


NUMBER = re.compile(r"^[+-]?(\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?$")
NOW = re.compile(r"^(current_timestamp|datetime\(\s*'now'\s*\)|strftime\(.*'now'.*\))$")


def ferrite_default(raw, ty, nullable):
    """Translate one SQLite `DEFAULT` into Ferrite's subset.

    Returns `(clause, note)`; `clause` is `None` when the default has no
    Ferrite equivalent, which is a dropped constraint like any other rather
    than a silent behaviour change — Ferrite refuses a `DEFAULT` it cannot
    store, so emitting one it does not understand would cost the whole table.
    """
    if raw is None:
        return None, None
    expr = raw.strip()
    while expr.startswith("(") and expr.endswith(")"):
        expr = expr[1:-1].strip()
    folded = expr.lower()

    if folded == "null":
        return ("DEFAULT NULL", None) if nullable else (None, "DEFAULT NULL on a NOT NULL column")
    if NOW.match(folded):
        if ty == "TIMESTAMP":
            return "DEFAULT CURRENT_TIMESTAMP", None
        return None, f"DEFAULT {expr}: needs a TIMESTAMP column, this one is {ty}"
    if NUMBER.match(expr):
        if ty in ("BIGINT", "DOUBLE PRECISION"):
            return f"DEFAULT {expr}", None
        return None, f"DEFAULT {expr}: numeric, but the column is {ty}"
    if len(expr) >= 2 and expr[0] == "'" and expr[-1] == "'":
        body = expr[1:-1].replace("''", "'")
        if ty in ("TEXT", "JSON") or (ty == "TIMESTAMP" and is_timestamp(body)):
            return f"DEFAULT {literal(body)}", None
        return None, f"DEFAULT {expr}: a string, but the column is {ty}"
    return None, f"DEFAULT {expr}: outside the subset Ferrite stores"


def ferrite_type(decl, values, name):
    """Pick a `ferrite_common::DataType` for one column.

    The declared SQLite type is the starting point; the actual values
    decide between TEXT, TIMESTAMP and JSON, since SQLite stores all three
    as TEXT and only the content tells them apart.
    """
    decl = (decl or "").upper()
    present = [v for v in values if v is not None]

    if "INT" in decl:
        return "BIGINT", "INTEGER -> BIGINT"
    if "REAL" in decl or "FLOA" in decl or "DOUB" in decl:
        return "DOUBLE PRECISION", "REAL -> DOUBLE PRECISION"
    if "BLOB" in decl:
        if not present:
            return "TEXT", "BLOB, empty in this database -> TEXT"
        return None, "BLOB: no binary type in Ferrite v1"

    if present and all(is_timestamp(v) for v in present):
        return "TIMESTAMP", "TEXT holding ISO dates -> TIMESTAMP"
    if present and all(is_json(v) for v in present):
        return "JSON", "TEXT holding JSON -> JSON"
    return "TEXT", "TEXT -> TEXT"


INDEX_DDL = re.compile(
    r"^\s*CREATE\s+(UNIQUE\s+)?INDEX\s+(\S+)\s+ON\s+(\S+?)\s*\((.*?)\)\s*$",
    re.IGNORECASE | re.DOTALL,
)


def index_ddl(cur, notes):
    """The index DDL, which can only run once."""
    by_name = {n["table"]: n for n in notes}
    out = []

    for row in cur.execute(
        "SELECT sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL"
    ).fetchall():
        ddl = row[0].decode() if isinstance(row[0], bytes) else row[0]
        match = INDEX_DDL.match(" ".join(ddl.split()))
        # A partial index (`... WHERE status = 'paid'`) does not match, and
        # Ferrite v1 has no equivalent, so it is left out rather than
        # silently widened into a total one.
        if not match:
            continue
        unique, name, table, columns = match.groups()
        table = table.strip('"')
        if table not in by_name:
            continue
        columns = [c.strip().strip('"') for c in columns.split(",")]
        if any(c not in by_name[table]["types"] for c in columns):
            continue
        out.append(
            f'CREATE {"UNIQUE " if unique else ""}INDEX {quote(name.strip(chr(34)))} '
            f'ON {quote(table)} ({", ".join(quote(c) for c in columns)})'
        )

    # A table-level `UNIQUE (a, b)` has no `CREATE INDEX` of its own in
    # sqlite_master — it is an implicit index. Ferrite needs it recorded,
    # because that is what an `INSERT OR IGNORE` with no explicit target
    # conflicts on; without it the insert has no target and is refused.
    for note in notes:
        table = note["table"]
        for row in cur.execute(f'PRAGMA index_list("{table}")').fetchall():
            name = row[1].decode() if isinstance(row[1], bytes) else row[1]
            unique = bool(row[2])
            origin = row[3].decode() if isinstance(row[3], bytes) else row[3]
            if not unique or origin != "u":
                continue
            columns = [
                (c[2].decode() if isinstance(c[2], bytes) else c[2])
                for c in cur.execute(f'PRAGMA index_info("{name}")').fetchall()
            ]
            if not columns or any(c not in note["types"] for c in columns):
                continue
            out.append(
                f'CREATE UNIQUE INDEX {quote("u_" + table + "_" + "_".join(columns))} '
                f'ON {quote(table)} ({", ".join(quote(c) for c in columns)})'
            )

    return out


def query_shapes(notes):
    """The query shapes a real application runs.

    Loading rows only proves the write path. An application also reads, and
    it reads through joins, counts and sorts — so the replay ends with a
    representative query per shape, derived from this schema rather than
    hand-written, and reports which ones the server accepts.

    Every shape here is replayed twice: once against the loaded rows, and
    once after the migrations below have widened every table. A table that
    just received a column has to stay readable through a `JOIN`, an
    `ORDER BY` and a `GROUP BY`, which is exactly where the two features
    meet.
    """
    by_name = {n["table"]: n for n in notes}
    out = []

    def columns_of(note, *types):
        return [c for c, ty in note["types"].items() if ty in types]

    ranked = sorted(notes, key=lambda n: -(n["rows"] - n["skipped_rows"]))
    populated = [n for n in ranked if n["rows"] - n["skipped_rows"] > 0]
    if not populated:
        return out

    biggest = populated[0]
    out.append(f'SELECT count(*) FROM {quote(biggest["table"])}')

    for note in populated:
        stamps = columns_of(note, "TIMESTAMP")
        if stamps:
            out.append(
                f'SELECT * FROM {quote(note["table"])} '
                f'ORDER BY {quote(stamps[0])} DESC LIMIT 20'
            )
            break

    for note in populated:
        texts = columns_of(note, "TEXT")
        if texts:
            out.append(
                f'SELECT count(*) FROM {quote(note["table"])} '
                f"WHERE {quote(texts[0])} LIKE '%a%'"
            )
            break

    # A `<x>_id` column naming a table whose single-column primary key it
    # can be compared against is the closest thing to a foreign key left
    # after the translation drops them.
    joins = 0
    for note in populated:
        for column in note["types"]:
            if not column.endswith("_id") or joins >= 2:
                continue
            stem = column[:-3]
            target = next(
                (
                    by_name[c]
                    for c in (stem, stem + "s", stem + "es")
                    if c in by_name
                    and by_name[c]["pk"]
                    and by_name[c]["types"].get(by_name[c]["pk"])
                    == note["types"][column]
                ),
                None,
            )
            if target is None or target["table"] == note["table"]:
                continue
            left, right = note["table"], target["table"]
            key = target["pk"]
            out.append(
                f"SELECT {quote(left)}.{quote(column)}, {quote(right)}.{quote(key)} "
                f"FROM {quote(left)} JOIN {quote(right)} "
                f"ON {quote(right)}.{quote(key)} = {quote(left)}.{quote(column)} LIMIT 20"
            )
            out.append(
                f"SELECT {quote(right)}.{quote(key)}, count(*) "
                f"FROM {quote(right)} LEFT JOIN {quote(left)} "
                f"ON {quote(left)}.{quote(column)} = {quote(right)}.{quote(key)} "
                f"GROUP BY {quote(right)}.{quote(key)} "
                f"HAVING count(*) > 1"
            )
            out.append(
                f"SELECT DISTINCT {quote(column)} FROM {quote(left)} "
                f"ORDER BY {quote(column)}"
            )
            joins += 1

    return out


def dialect_shapes(notes):
    """One query per SQL construct the audit found PawChat emitting, built
    from this schema rather than hand-written.

    `docs/pawchat-sql-audit.md` counts what the application actually runs;
    this turns that list into statements the replay can execute, so the
    report says which constructs a real schema supports rather than which
    ones parse in isolation.
    """
    by_name = {n["table"]: n for n in notes}
    out = []

    def columns_of(note, *types):
        return [c for c, ty in note["types"].items() if ty in types]

    populated = sorted(
        (n for n in notes if n["rows"] - n["skipped_rows"] > 0),
        key=lambda n: -(n["rows"] - n["skipped_rows"]),
    )
    if not populated:
        return out

    text_table = next((n for n in populated if columns_of(n, "TEXT")), None)
    stamp_table = next((n for n in populated if columns_of(n, "TIMESTAMP")), None)
    int_table = next((n for n in populated if columns_of(n, "BIGINT")), None)

    if text_table:
        t, c = quote(text_table["table"]), quote(columns_of(text_table, "TEXT")[0])
        # ILIKE is the migration target for every LIKE PawChat relies on:
        # SQLite folds case there by default and Ferrite does not.
        out.append(f"SELECT count(*) FROM {t} WHERE {c} ILIKE '%A%'")
        out.append(f"SELECT count(*) FROM {t} WHERE {c} NOT ILIKE '%A%'")
        # COLLATE NOCASE, in the two places PawChat writes it.
        out.append(f"SELECT count(*) FROM {t} WHERE {c} = 'A' COLLATE NOCASE")
        out.append(f"SELECT {c} FROM {t} ORDER BY {c} COLLATE NOCASE ASC LIMIT 20")
        out.append(f"SELECT lower({c}), upper({c}), substr({c}, 1, 3) FROM {t} LIMIT 20")
        out.append(f"SELECT coalesce({c}, 'none') FROM {t} LIMIT 20")
        out.append(f"SELECT {c} || '!' FROM {t} LIMIT 20")
        out.append(f"SELECT lower(hex(randomblob(4))) FROM {t} LIMIT 1")
        out.append(
            f"SELECT CASE WHEN {c} IS NULL THEN 'empty' ELSE 'set' END FROM {t} LIMIT 20"
        )
        out.append(f"SELECT CAST({c} AS TEXT) FROM {t} LIMIT 20")

    if stamp_table:
        t = quote(stamp_table["table"])
        c = quote(columns_of(stamp_table, "TIMESTAMP")[0])
        out.append(f"SELECT count(*) FROM {t} WHERE {c} >= datetime('now', '-30 days')")
        out.append(f"SELECT count(*) FROM {t} WHERE {c} >= datetime('now', '-1 hour')")
        out.append(
            f"SELECT date({c}) AS day, count(*) FROM {t} GROUP BY day ORDER BY day ASC"
        )
        out.append(f"SELECT datetime({c}) FROM {t} LIMIT 20")

    if int_table:
        t = quote(int_table["table"])
        c = quote(columns_of(int_table, "BIGINT")[0])
        out.append(f"SELECT CAST({c} AS TEXT) FROM {t} LIMIT 20")
        out.append(
            f"SELECT coalesce(sum(CASE WHEN {c} > 0 THEN {c} END), 0), "
            f"count(DISTINCT {c}) FROM {t}"
        )

    # An uncorrelated `IN (SELECT ...)` between two tables that share a
    # column type, which is the shape of every one PawChat runs.
    for note in populated:
        done = False
        for column in note["types"]:
            if not column.endswith("_id"):
                continue
            stem = column[:-3]
            target = next(
                (
                    by_name[c]
                    for c in (stem, stem + "s", stem + "es")
                    if c in by_name
                    and by_name[c]["pk"]
                    and by_name[c]["types"].get(by_name[c]["pk"])
                    == note["types"][column]
                ),
                None,
            )
            if target is None or target["table"] == note["table"]:
                continue
            outer = quote(target["table"])
            key = quote(target["pk"])
            inner = quote(note["table"])
            out.append(
                f"SELECT count(*) FROM {outer} WHERE {key} IN "
                f"(SELECT {quote(column)} FROM {inner})"
            )
            out.append(
                f"SELECT count(*) FROM {outer} WHERE {key} NOT IN "
                f"(SELECT {quote(column)} FROM {inner})"
            )
            done = True
            break
        if done:
            break

    return out


def upsert_shapes(cur, notes):
    """`INSERT OR IGNORE` / `INSERT OR REPLACE` / `ON CONFLICT DO UPDATE`
    against a table that really has a unique key, re-inserting a row that is
    already there.

    Running these after the rows are loaded is the whole point: the conflict
    has to be real for `DO NOTHING` and `DO UPDATE` to be told apart, and for
    the row count afterwards to mean anything.
    """
    out = []
    tried = 0
    for note in notes:
        if not note["pk"] or note["rows"] - note["skipped_rows"] == 0 or tried >= 3:
            continue
        table = note["table"]
        rows = cur.execute('SELECT * FROM "%s" LIMIT 1' % table).fetchall()
        if not rows:
            continue
        info = cur.execute('PRAGMA table_info("%s")' % table).fetchall()
        names = [c[1].decode() if isinstance(c[1], bytes) else c[1] for c in info]
        pairs = []
        for name, value in zip(names, rows[0]):
            if name not in note["types"]:
                continue
            if isinstance(value, bytes):
                try:
                    value = value.decode("utf-8")
                except UnicodeDecodeError:
                    pairs = []
                    break
            lit = literal(value)
            if lit is None:
                pairs = []
                break
            pairs.append((name, lit))
        if not pairs:
            continue
        cols = ", ".join(quote(n) for n, _ in pairs)
        vals = ", ".join(v for _, v in pairs)
        pk = quote(note["pk"])
        other = next((n for n, _ in pairs if n != note["pk"]), None)
        out.append("SELECT count(*) FROM %s" % quote(table))
        out.append(
            "INSERT OR IGNORE INTO %s (%s) VALUES (%s)" % (quote(table), cols, vals)
        )
        out.append(
            "INSERT OR REPLACE INTO %s (%s) VALUES (%s)" % (quote(table), cols, vals)
        )
        if other:
            out.append(
                "INSERT INTO %s (%s) VALUES (%s) ON CONFLICT (%s) DO UPDATE SET %s = excluded.%s"
                % (quote(table), cols, vals, pk, quote(other), quote(other))
            )
            out.append(
                "INSERT INTO %s (%s) VALUES (%s) ON CONFLICT (%s) DO NOTHING"
                % (quote(table), cols, vals, pk)
            )
        # The count must be unchanged: every statement above collided.
        out.append("SELECT count(*) FROM %s" % quote(table))
        tried += 1
    return out


PROBE_KEY = itertools.count(1)


def key_columns(cur, table, pk):
    """Every column of `table` covered by a primary key or a unique index,
    whether declared inline, at table level, or as a `CREATE UNIQUE INDEX`.

    A generated probe row has to give all of them a value of its own; any
    one it copies from an existing row is a duplicate key.
    """
    out = set(pk)
    for row in cur.execute('PRAGMA index_list("%s")' % table).fetchall():
        name = row[1].decode() if isinstance(row[1], bytes) else row[1]
        if not bool(row[2]):
            continue
        for c in cur.execute('PRAGMA index_info("%s")' % name).fetchall():
            out.add(c[2].decode() if isinstance(c[2], bytes) else c[2])
    return out


def fresh_key(ty):
    """A key value no row can already hold, or `None` when the type gives
    no way to be sure of that."""
    n = next(PROBE_KEY)
    if ty == "BIGINT":
        # Far above any autoincrement this database has handed out.
        return str(8_000_000_000 + n)
    if ty == "TEXT":
        return "'ferrite-probe-%d'" % n
    return None


def constraint_shapes(cur, notes):
    """Plain duplicate writes against tables that really have a primary key.

    These are the only statements in the replay that must *fail*, so they
    carry an `EXPECT` marker the replay reads. The point is the pair: the
    row count either side of the duplicate has to be identical, which is
    what tells a refused write apart from an accepted one that happened to
    report an error.
    """
    out = []
    tried = 0
    # A natural key first — an application invents those (a username, an
    # invite code) and collides on them; an integer surrogate is handed out
    # by the database and rarely does.
    ordered = sorted(
        notes, key=lambda n: (n["types"].get(n["pk"] or "") != "TEXT", n["table"])
    )
    for note in ordered:
        if not note["pk"] or note["rows"] - note["skipped_rows"] == 0 or tried >= 3:
            continue
        table = note["table"]
        rows = cur.execute('SELECT * FROM "%s" LIMIT 1' % table).fetchall()
        if not rows:
            continue
        info = cur.execute('PRAGMA table_info("%s")' % table).fetchall()
        names = [c[1].decode() if isinstance(c[1], bytes) else c[1] for c in info]
        pairs = []
        for name, value in zip(names, rows[0]):
            if name not in note["types"]:
                continue
            if isinstance(value, bytes):
                try:
                    value = value.decode("utf-8")
                except UnicodeDecodeError:
                    pairs = []
                    break
            lit = literal(value)
            if lit is None:
                pairs = []
                break
            pairs.append((name, lit))
        if not pairs:
            continue
        cols = ", ".join(quote(n) for n, _ in pairs)
        vals = ", ".join(v for _, v in pairs)
        out.append("SELECT count(*) FROM %s" % quote(table))
        out.append(
            EXPECT % "23505"
            + "\n"
            + "INSERT INTO %s (%s) VALUES (%s)" % (quote(table), cols, vals)
        )
        out.append("SELECT count(*) FROM %s" % quote(table))
        tried += 1
    return out


def real_queries(notes):
    """Queries copied from the PawChat sources, translated rather than
    paraphrased: identifiers quoted, `?` replaced by a constant, and the
    three `LIKE`s the audit flagged rewritten as `ILIKE`.

    A shape derived from the schema proves a construct parses. These prove
    the statements the application really sends do. Each carries the file
    and line it came from.
    """
    have = {n["table"]: n for n in notes}
    out = []

    def add(needs, sql):
        if all(t in have for t in needs):
            out.append(sql)

    # src/app/api/admin/users/route.ts:18 - LIKE -> ILIKE, see the audit.
    add(
        ["users"],
        'SELECT "id", "username", "display_name" FROM "users" '
        "WHERE \"id\" != 'bot-0' AND (\"username\" ILIKE '%a%' "
        "OR \"display_name\" ILIKE '%a%') LIMIT 50",
    )
    # src/lib/userStore.ts:30
    add(
        ["users"],
        'SELECT "id" FROM "users" WHERE "username" = \'Demo\' COLLATE NOCASE',
    )
    # src/app/api/admin/analytics/route.ts:11
    add(
        ["users"],
        'SELECT date("created_at") AS day, COUNT(*) AS n FROM "users" '
        "WHERE \"created_at\" >= datetime('now', '-30 days') "
        "GROUP BY day ORDER BY day ASC",
    )
    # src/app/api/channels/[channelId]/messages/route.ts:123
    add(
        ["users", "memberships"],
        'SELECT "id" FROM "users" WHERE "is_bot" = 1 AND "id" IN '
        '(SELECT "user_id" FROM "memberships" WHERE "server_id" = 1)',
    )
    # src/lib/channelStore.ts:233
    add(
        ["servers", "memberships"],
        'SELECT "id" FROM "servers" WHERE "is_public" = 1 AND "id" NOT IN '
        '(SELECT "server_id" FROM "memberships" WHERE "user_id" = \'demo\') '
        'ORDER BY "id" DESC',
    )
    # src/app/api/servers/route.ts:86
    add(
        ["memberships"],
        'INSERT OR IGNORE INTO "memberships" ("server_id", "user_id") '
        "VALUES (1, 'demo')",
    )
    # src/app/api/admin/config/route.ts:34
    add(
        ["platform_config"],
        'INSERT INTO "platform_config" ("key", "value", "updated_at") '
        "VALUES ('ferrite_probe', 'on', datetime('now')) "
        'ON CONFLICT ("key") DO UPDATE SET "value" = excluded."value", '
        '"updated_at" = excluded."updated_at"',
    )
    # src/app/api/admin/store/route.ts:18
    add(
        ["store_items"],
        'SELECT COUNT(DISTINCT "id") AS total_items, '
        "COUNT(DISTINCT CASE WHEN \"status\" = 'live' THEN \"id\" END) AS live_items "
        'FROM "store_items"',
    )
    # src/app/api/admin/support-tickets/route.ts:47
    add(
        ["support_tickets"],
        'UPDATE "support_tickets" SET "status" = \'resolved\', '
        "\"updated_at\" = datetime('now'), "
        "\"closed_at\" = CASE WHEN 'resolved' IN ('resolved','closed') "
        "THEN datetime('now') ELSE \"closed_at\" END WHERE \"id\" = -1",
    )
    # src/app/api/servers/[serverId]/invite/regenerate/route.ts:18
    out.append("SELECT lower(hex(randomblob(4))) AS code")
    # src/lib/botFlows.ts:84
    add(
        ["bot_flows"],
        'SELECT DISTINCT "group_name" FROM "bot_flows" '
        'WHERE "group_name" IS NOT NULL ORDER BY "group_name" COLLATE NOCASE ASC',
    )
    # src/app/api/admin/protection/route.ts:33
    add(
        ["users", "channel_messages"],
        'SELECT "users"."id", MAX("channel_messages"."created_at") AS last_msg '
        'FROM "users" JOIN "channel_messages" '
        'ON "channel_messages"."user_id" = "users"."id" '
        'GROUP BY "users"."id" HAVING count(*) > 1',
    )
    return out


def migrations(notes):
    """The migration an application replays at boot, then a write that leans
    on the defaults: a nullable column, a `NOT NULL DEFAULT` one, the same
    migration re-run, a read of the rows written before both existed, and an
    `INSERT` naming only the columns the application has no choice but to
    supply.
    """
    out = []
    for n in notes:
        t = quote(n["table"])
        out.append(f'ALTER TABLE {t} ADD COLUMN IF NOT EXISTS "ferrite_added" TEXT')
        out.append(
            f'ALTER TABLE {t} ADD COLUMN IF NOT EXISTS "ferrite_added_flag" '
            "BIGINT NOT NULL DEFAULT 0"
        )
        out.append(f'ALTER TABLE {t} ADD COLUMN IF NOT EXISTS "ferrite_added" TEXT')
        out.append(f'SELECT "ferrite_added", "ferrite_added_flag" FROM {t}')
        if n["probe"]:
            out.append(n["probe"])
    return out


def main():
    os.makedirs(OUT, exist_ok=True)
    db = sqlite3.connect(DB)
    db.text_factory = bytes
    db.row_factory = None
    cur = db.cursor()

    tables = [
        r[0].decode()
        for r in cur.execute(
            "SELECT name FROM sqlite_master WHERE type='table' "
            "AND name NOT LIKE 'sqlite_%' ORDER BY name"
        ).fetchall()
    ]

    notes = []
    for table in tables:
        info = cur.execute(f'PRAGMA table_info("{table}")').fetchall()
        cols = [
            {
                "name": c[1].decode(),
                "decl": c[2].decode() if c[2] else "",
                "notnull": bool(c[3]),
                "default": c[4].decode() if isinstance(c[4], bytes) else c[4],
                "pk": c[5],
            }
            for c in info
        ]
        rows = cur.execute(f'SELECT * FROM "{table}"').fetchall()

        # Decode bytes back to str where the value is text.
        def decode(v):
            if isinstance(v, bytes):
                try:
                    return v.decode("utf-8")
                except UnicodeDecodeError:
                    return v
            return v

        rows = [[decode(v) for v in row] for row in rows]

        kept, dropped, decisions = [], [], []
        for i, col in enumerate(cols):
            values = [r[i] for r in rows]
            ty, why = ferrite_type(col["decl"], values, col["name"])
            if ty is None:
                dropped.append((col["name"], why))
                continue
            kept.append((i, col, ty))
            decisions.append((col["name"], col["decl"] or "(none)", ty, why))

        pk = [c["name"] for c in cols if c["pk"]]
        pieces = []
        defaults, mandatory = {}, []
        for _, col, ty in kept:
            piece = f'  {quote(col["name"])} {ty}'
            not_null = col["notnull"] or col["name"] in pk
            if col["notnull"] and col["name"] not in pk:
                piece += " NOT NULL"
            clause, why = ferrite_default(col["default"], ty, not not_null)
            if clause:
                piece += " " + clause
                defaults[col["name"]] = clause
            elif why:
                dropped.append((col["name"], why))
            if not_null and not clause:
                mandatory.append(col["name"])
            pieces.append(piece)
        if len(pk) == 1 and any(c["name"] == pk[0] for _, c, _ in kept):
            pieces = [
                p + " PRIMARY KEY" if p.strip().startswith(quote(pk[0])) else p
                for p in pieces
            ]
        elif len(pk) > 1 and all(any(c["name"] == p for _, c, _ in kept) for p in pk):
            pieces.append("  PRIMARY KEY (" + ", ".join(quote(p) for p in pk) + ")")

        ddl = f"CREATE TABLE {quote(table)} (\n" + ",\n".join(pieces) + "\n)"

        statements = [ddl]
        skipped_rows = 0
        names = ", ".join(quote(c["name"]) for _, c, _ in kept)
        for row in rows:
            values = []
            bad = False
            for i, col, ty in kept:
                lit = literal(row[i])
                if lit is None:
                    bad = True
                    break
                values.append(lit)
            if bad:
                skipped_rows += 1
                continue
            statements.append(
                f"INSERT INTO {quote(table)} ({names}) VALUES (" + ", ".join(values) + ")"
            )

        # An INSERT naming only the columns the application has no choice
        # but to supply. Every other column has to come from its DEFAULT,
        # which is the case that used to write a wrong value or fail.
        #
        # Every key column gets a value of its own rather than the sample
        # row's: copying it made this a duplicate-key insert, which used to
        # be accepted only because nothing enforced the key.
        probe = None
        keyed = key_columns(cur, table, pk)
        if defaults and mandatory and rows:
            values = []
            for i, col, ty in kept:
                if col["name"] not in mandatory:
                    continue
                if col["name"] in keyed:
                    values.append(fresh_key(ty))
                else:
                    values.append(literal(rows[0][i]))
            if all(v is not None for v in values):
                probe = (
                    f"INSERT INTO {quote(table)} ("
                    + ", ".join(quote(n) for n in mandatory)
                    + ") VALUES ("
                    + ", ".join(values)
                    + ")"
                )

        with open(os.path.join(OUT, f"{table}.sql"), "w", encoding="utf-8") as f:
            f.write(("\n" + SENTINEL + "\n").join(statements))

        notes.append(
            {
                "table": table,
                "columns": len(cols),
                "kept": len(kept),
                "dropped": dropped,
                "defaults": defaults,
                "rows": len(rows),
                "skipped_rows": skipped_rows,
                "decisions": decisions,
                "probe": probe,
                "types": {c["name"]: ty for _, c, ty in kept},
                "pk": pk[0] if len(pk) == 1 else None,
            }
        )

    reads = query_shapes(notes) + dialect_shapes(notes) + real_queries(notes)
    after = (
        index_ddl(cur, notes)
        + reads
        + upsert_shapes(cur, notes)
        + constraint_shapes(cur, notes)
        + migrations(notes)
        + reads
    )
    with open(os.path.join(OUT, "_after.sql"), "w", encoding="utf-8") as f:
        f.write(("\n" + SENTINEL + "\n").join(after))

    with open(os.path.join(OUT, "manifest.txt"), "w", encoding="utf-8") as f:
        for n in notes:
            f.write(f'{n["table"]}\t{n["rows"] - n["skipped_rows"]}\n')

    with open(os.path.join(OUT, "types.md"), "w", encoding="utf-8") as f:
        for n in notes:
            f.write(f'## {n["table"]} ({n["rows"]} rows, {n["kept"]}/{n["columns"]} columns)\n\n')
            for name, decl, ty, why in n["decisions"]:
                f.write(f"- `{name}`: {decl} -> {ty} ({why})\n")
            for name, why in n["dropped"]:
                f.write(f"- **dropped** `{name}`: {why}\n")
            if n["skipped_rows"]:
                f.write(f'- **{n["skipped_rows"]} rows skipped** (unrepresentable value)\n')
            f.write("\n")

    total_rows = sum(n["rows"] - n["skipped_rows"] for n in notes)
    kept_defaults = sum(len(n["defaults"]) for n in notes)
    probes = sum(1 for n in notes if n["probe"])
    print(f"{len(tables)} tables, {total_rows} rows, out={OUT}")
    print(f"{kept_defaults} DEFAULT clauses kept, {probes} defaults probes in _after.sql")
    for n in notes:
        if n["dropped"] or n["skipped_rows"]:
            print(f'  {n["table"]}: dropped={[d[0] for d in n["dropped"]]} skipped={n["skipped_rows"]}')


main()
