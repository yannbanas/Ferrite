# ferrite-planner

AST → logical plan → physical plan. Rule-based only.

```text
Statement --build_logical--> LogicalPlan --optimize--> LogicalPlan --to_physical--> PhysicalPlan
```

## Why there is no cost model

`docs/architecture.md` cuts the cost-based optimizer for v1: no statistics
collection, no cardinality estimation, no plan search. What remains is a
single deterministic pass with two rules, which is enough to avoid the two
mistakes that actually hurt on a small single-node engine — reading rows
you immediately throw away, and ignoring an index that answers the
predicate outright.

## The two rules

**Predicate pushdown** (`rules.rs`). `WHERE` clauses are split on `AND`
and each conjunct is pushed as far down as it can go, ending up inside the
`Scan` node. Two barriers stop it:

- `Limit` — filtering below a limit changes which rows survive.
- a `Projection` that is not a plain column list — the names below it may
  no longer mean the same thing.

Anything that hits a barrier is re-materialized as a `Filter` immediately
above it, so the plan stays correct in every case.

**Index vs. scan** (`planner.rs::access_path`). Once a predicate sits in
the scan, the first conjunct shaped `indexed_column = <literal>` selects an
`IndexScan`; every other conjunct becomes the scan's `residual` filter. If
no conjunct matches, or no index exists on that column, the result is a
`SeqScan`. Deliberately narrow:

- equality only — range predicates never select an index in v1;
- single-column B-tree indexes only, matching the storage v1 scope;
- `col = NULL` never probes an index, since it is never true;
- a unique index wins over a non-unique one on the same column.

No statistics means no "the index is not selective enough, scan instead"
judgement. That trade is accepted for v1.

## Two expression types, on purpose

`Planner::build_logical` is the only method that reads
`ferrite_sql::ast::Statement`. `ferrite-sql` parses a good deal more than
`ferrite-exec` can run, so `src/lower.rs` projects the parsed statement
onto the narrow IR in `src/expr.rs` and rejects everything else with a
`FerriteError::Plan`. Every rejection lives in that one module, which is
what keeps the gap between "parses" and "executes" readable.

Expanded rather than rejected, because they are sugar over comparisons the
executor already has: `BETWEEN`, `IN (list)`, `IS [NOT] NULL`, `NOT`, unary
minus on a literal.

## Literal coercion

The parser gives one `Value` shape per literal syntax, so `'…'` is always
`Text` and `1` is always the narrowest integer that fits. The planner
coerces a literal to the type of the column it is written into or compared
against — `Text` into `UUID`/`TIMESTAMP`/`JSON`, and integer widening. This
is not cosmetic: the executor compares `Value` variants, an index probe
compares them for exact equality, and the wire encoder reads the *stored*
variant, so an `Int4` left in a `BIGINT` column would go out as four bytes
where the client expects eight.

Anything beyond a literal would need the type-inference pass v1 does not
have, so `CAST` and computed expressions are `Plan` errors.

## Index metadata

`ferrite_common::IndexCatalog` is the shared contract, implemented by
`ferrite-catalog`. Only single-column indexes are considered: the
executor's `IndexProvider` probes with one key value, so using the leading
column of a composite key would need a range probe that does not exist in
v1.

## Known limits

- One table per statement: no joins, no subqueries, no set operations.
- No `GROUP BY`, no aggregates, no `ORDER BY`, no window functions.
- Projections may contain column references and literals only; computed
  expressions need a type-inference pass that does not exist yet.
- `LIMIT` without `OFFSET`.
- No plan cache. The executor does detect a plan built against a stale
  schema, so caching can be added without a correctness hole.
