# ferrite-sql

SQL text → AST. Hand-written lexer and recursive-descent parser (precedence
climbing for expressions) for the Ferrite v1 dialect. Depends only on
`ferrite-common`: no storage, no execution, no name resolution — the parser
never decides whether a table or column exists.

```rust
use ferrite_sql::{ast::Statement, parse, parse_statement};

let stmts = parse("BEGIN; SELECT 1; COMMIT;")?;            // several statements
let one = parse_statement("SELECT id FROM users LIMIT 1")?; // exactly one
```

Both return `Result<_, ferrite_sql::ParseError>`, which carries a message and
the byte offset in the input, and converts into `ferrite_common::FerriteError`
via `From`.

## Design rules

- **Never panic on user input.** Query text arrives from the network. There is
  no `unwrap`/`expect`/`panic!`/slicing on any input-derived path; every
  failure is a `ParseError`.
- **Bounded recursion.** Grammar recursion is capped (`MAX_DEPTH = 100`), so
  `((((…))))` is a clean parse error rather than a stack overflow. This is the
  one way a recursive-descent parser can kill the process without panicking.
- **Bounded literals.** Integer literals that do not fit in `i64` and floats
  that overflow to infinity are rejected instead of silently wrapping.
- **Case folding matches Postgres.** Unquoted identifiers fold to *lower* case;
  `"Quoted"` identifiers keep their spelling.

## Covered

### DDL

```sql
CREATE TABLE [IF NOT EXISTS] [schema.]name (
    col type [NOT NULL | NULL] [PRIMARY KEY] [UNIQUE] [DEFAULT expr], …
    [, PRIMARY KEY (a, b)] [, UNIQUE (a)]
);
DROP TABLE [IF EXISTS] a, b [CASCADE | RESTRICT];

CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON [schema.]table (a, b);
DROP INDEX [IF EXISTS] name;
```

There is no access-method clause on `CREATE INDEX` (`USING gin`, …):
`docs/architecture.md` keeps B-tree only in v1. Index names are not qualified —
an index lives in the schema of its table, matching `ferrite-catalog`.

Types map straight onto `ferrite_common::DataType`:

| SQL | `DataType` |
| --- | --- |
| `BOOLEAN`, `BOOL` | `Boolean` |
| `INT`, `INTEGER`, `INT4` | `Int4` |
| `BIGINT`, `INT8` | `Int8` |
| `DOUBLE PRECISION`, `FLOAT8` | `Float8` |
| `TEXT`, `VARCHAR`, `VARCHAR(n)` | `Text` |
| `TIMESTAMP`, `TIMESTAMPTZ` | `Timestamp` |
| `UUID` | `Uuid` |
| `JSON`, `JSONB` | `Json` |

`VARCHAR(n)` parses but the length is discarded — Ferrite v1 has one string
type. `TIMESTAMPTZ` is accepted as a spelling of `TIMESTAMP`; timestamps are
always UTC microseconds.

`CreateTable::to_schema()` projects the parsed columns onto a
`ferrite_common::Schema` ready for `Catalog::create_table`, treating
`NOT NULL` and primary-key membership as non-nullable.

### Queries

```sql
WITH recent (id) AS (SELECT id FROM events WHERE at > 0)
SELECT DISTINCT u.id, count(*) AS n, t.*
FROM users u
  JOIN accounts a ON a.user_id = u.id
  LEFT OUTER JOIN recent r ON r.id = u.id
  CROSS JOIN flags
  JOIN tags t USING (id),
  (SELECT 1 AS one) AS sub
WHERE u.age BETWEEN 18 AND 99 AND u.email LIKE '%@example.com'
GROUP BY u.id
HAVING count(*) > 1
ORDER BY n DESC NULLS LAST, u.id ASC
LIMIT 10 OFFSET 5;
```

- Projections: `*`, `qualifier.*`, `expr`, `expr AS alias`, `expr alias`.
- `FROM`: tables (optionally schema-qualified and aliased), parenthesised
  subqueries (alias **required**), comma-separated relations.
- Joins: `[INNER] JOIN`, `LEFT/RIGHT/FULL [OUTER] JOIN`, `CROSS JOIN`, with
  `ON expr` or `USING (cols)`. A non-cross join without a condition is a parse
  error; a `CROSS JOIN` with one is too.
- `HAVING` without `GROUP BY` is a parse error.
- Set operations: `UNION [ALL]`, `INTERSECT`, `EXCEPT`, left-associative;
  `ORDER BY`/`LIMIT`/`OFFSET` bind to the whole set expression.
- CTEs: non-recursive `WITH`, optional column list, several comma-separated.

### DML

```sql
INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y') RETURNING id;
INSERT INTO t SELECT a, b FROM u;
UPDATE t SET a = 1, b = a + 1 WHERE id = $1 RETURNING *;
DELETE FROM t AS x WHERE x.a IS NULL RETURNING x.id;
```

When an `INSERT` names columns, every `VALUES` row must have that many
expressions — checked at parse time.

### Transactions

`BEGIN [TRANSACTION|WORK]`, `START TRANSACTION`, `COMMIT [TRANSACTION|WORK]`,
`END`, `ROLLBACK [TRANSACTION|WORK]`.

### Procedures and triggers

Kept because they are the anchor of Ferrite's security model
(`docs/architecture.md`): there is no `CREATE POLICY`, so a procedure that
inspects the caller's identity and refuses *is* the access-control mechanism.
The body is therefore a deliberately small imperative block — enough to check
and refuse, not a general-purpose PL.

```sql
CREATE [OR REPLACE] PROCEDURE [schema.]name (p1 TYPE, p2 TYPE) AS BEGIN
    IF p1 <> p2 THEN
        RAISE 'access denied';
    ELSIF p2 IS NULL THEN
        RETURN;
    ELSE
        UPDATE rows SET seen = true WHERE id = p2;
        RETURN 1;
    END IF;
END;

DROP PROCEDURE [IF EXISTS] name;
CALL name($1, 2);

CREATE TRIGGER audit AFTER INSERT OR UPDATE ON public.rows
    FOR EACH ROW WHEN (active = true)
    EXECUTE PROCEDURE log_it('audit');
DROP TRIGGER [IF EXISTS] audit ON public.rows;
```

Body statements: any SQL statement, `RETURN [expr]`, `RAISE expr`, and
`IF … THEN … ELSIF … ELSE … END IF`. Every statement in a block ends with `;`,
including the last one before `END`, and a body must not be empty.
`PROCEDURE` and `FUNCTION` are accepted interchangeably. `BEFORE`/`AFTER`
timing, `INSERT`/`UPDATE`/`DELETE` events combined with `OR`,
`FOR EACH ROW|STATEMENT` (statement-level is the default), optional
`WHEN (expr)`.

### Expressions

Literals (integer, float, string with `''` escapes, `TRUE`/`FALSE`/`NULL`),
column references (`a`, `t.a`), `$n` placeholders, quoted identifiers,
parentheses, function calls (`f(a, b)`, `count(*)`, `count(DISTINCT x)`),
`CAST(expr AS type)`, `CASE [operand] WHEN … THEN … [ELSE …] END`, scalar
subqueries, `EXISTS (…)`, `[NOT] IN (list)`, `[NOT] IN (subquery)`,
`[NOT] BETWEEN … AND …`, `[NOT] LIKE`, `IS [NOT] NULL`.

Operators, lowest precedence first: `OR`, `AND`, `NOT`, `IS`/`IN`/`BETWEEN`/
`LIKE`, comparisons (`= <> != < <= > >=`), `||`, `+ -`, `* / %`, unary `+ -`.
Comments: `--` to end of line, and `/* … */` which nests.

`ast::is_aggregate` marks `count`/`sum`/`avg`/`min`/`max`; the parser itself
treats every call identically — resolving and type-checking functions is the
planner's job.

### Reserved words

All keywords are reserved except type names and a handful of noise words
(`KEY`, `ROW`, `STATEMENT`, `EACH`, `WORK`, `TRANSACTION`, `FIRST`, `LAST`,
`NULLS`, `CASCADE`, `RESTRICT`, `REPLACE`, `PROCEDURE`, `TRIGGER`, `INDEX`,
`FUNCTION`, `PRECISION`), so `SELECT text, key FROM t` works but
`SELECT select FROM t` needs `"select"`.

## Not covered (v1)

Cut on purpose; several are cut project-wide by `docs/architecture.md`.

- **Window functions** (`OVER`, `PARTITION BY`), `GROUPING SETS`, `ROLLUP`,
  `CUBE`, `FILTER`, `WITHIN GROUP`, `ORDER BY` inside an aggregate.
- **Recursive CTEs** (`WITH RECURSIVE`), `LATERAL`, `NATURAL JOIN`.
- `ALTER TABLE`, `CREATE SCHEMA/VIEW/SEQUENCE/TYPE/ROLE`, `GRANT`/`REVOKE`,
  `TRUNCATE`, `COPY`, `EXPLAIN`, `ANALYZE`, `VACUUM`, `PREPARE`/`EXECUTE`,
  `SET`/`SHOW`, `COMMENT ON`.
- On indexes: partial (`WHERE`), expression and `CONCURRENTLY` indexes, plus
  per-column `ASC`/`DESC`/`NULLS` ordering and access-method clauses.
- `INSERT … ON CONFLICT`, `DEFAULT VALUES`, `UPDATE … FROM`,
  `DELETE … USING`, `SELECT … FOR UPDATE`, `FETCH FIRST`, cursors.
- `REFERENCES`/foreign keys, `CHECK`, `GENERATED`/`IDENTITY`, `COLLATE`,
  named constraints, `DEFERRABLE`.
- Savepoints, isolation-level clauses on `BEGIN` (`docs/architecture.md`
  defers both).
- Arrays, ranges, composite/`ROW` constructors, `::` cast shorthand, interval
  literals, `E''`/`U&''`/dollar-quoted strings, `SIMILAR TO`, regex operators,
  `IS DISTINCT FROM`, `AT TIME ZONE`, `EXTRACT`, `SUBSTRING(x FROM y FOR z)`,
  `ILIKE`, `ESCAPE` clauses.
- Multi-level qualified names (`db.schema.table`).
- Anything an extension would add: `docs/architecture.md` removes the
  extension system entirely.

Semantics are out of scope everywhere: the parser accepts
`SELECT count(count(x)) FROM t` and `INSERT INTO t (a) VALUES ('x')` against
an `INT` column. Resolving names, checking types and rejecting nonsense
belongs to `ferrite-planner`.

## Tests

```bash
cargo test -p ferrite-sql --all-targets
```

`tests/parse.rs` covers every supported construct plus a table of malformed
inputs that must produce a clean error (unterminated literals and comments,
truncated statements, stray bytes, oversized literals, 5000-deep nesting).
`tests/proptest_parser.rs` generates random statements from the supported
subset and asserts they parse, then throws random text and random token soup
at the parser and asserts it returns rather than unwinds.

## Fuzzing

`fuzz/` holds a `cargo-fuzz` harness with two targets: `parse_sql`
(text → statements) and `lex_sql` (tokens only, which reaches lexer edge cases
faster). It is a workspace of its own so nightly and libFuzzer's `-Z` flags
never leak into a normal `cargo build`.

```bash
cargo install cargo-fuzz
cd crates/ferrite-sql

# seed the corpus once
mkdir -p fuzz/corpus/parse_sql && cp fuzz/seeds/* fuzz/corpus/parse_sql/

cargo +nightly fuzz run parse_sql -- -max_total_time=300 -max_len=4096
cargo +nightly fuzz run lex_sql   -- -max_total_time=300 -max_len=4096

# reproduce and minimise a crash
cargo +nightly fuzz run parse_sql fuzz/artifacts/parse_sql/crash-…
cargo +nightly fuzz tmin parse_sql fuzz/artifacts/parse_sql/crash-…
```

CI runs both targets nightly and on demand via
`.github/workflows/fuzz.yml`; the fast panic-freedom signal on every push
comes from the proptest suite in the normal test job.
