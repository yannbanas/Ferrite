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
case-sensitive in Ferrite, as in PostgreSQL. Row counts will differ.

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

Dropped, because Ferrite v1 has no equivalent: `FOREIGN KEY`, `UNIQUE`,
`COLLATE`, `AUTOINCREMENT` and `CHECK`. `PRIMARY KEY`, `NOT NULL` and
`DEFAULT` are kept. Every identifier is quoted: Ferrite reserves nearly every
keyword, and application schemas are full of columns called `type`,
`position` and `content`.

`DEFAULT` is translated into the subset Ferrite stores — a literal, or the
current timestamp — and dropped with a note otherwise, since Ferrite refuses
a `DEFAULT` it cannot evaluate rather than accepting one it would ignore.
SQLite's `datetime('now')` and `CURRENT_TIMESTAMP` both map to
`CURRENT_TIMESTAMP`.

`_after.sql` holds what only makes sense once every table exists, and is
where the two features an application actually depends on get exercised
against real data: one `ALTER TABLE ... ADD COLUMN` of each shape per table
(nullable, and `NOT NULL DEFAULT`, which is what PawChat's migrations look
like), and one `INSERT` per table naming only the columns that have neither
a default nor a nullable type — the shape that used to fail on a column the
application never mentions.
"""

import json
import os
import re
import sqlite3
import sys

SENTINEL = "-- @@STATEMENT@@"
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
        probe = None
        if defaults and mandatory and rows:
            values = [
                literal(rows[0][i]) for i, col, _ in kept if col["name"] in mandatory
            ]
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

    reads = query_shapes(notes)
    after = index_ddl(cur, notes) + reads + migrations(notes) + reads
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
