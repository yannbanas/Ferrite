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

## Provisional AST

`src/ast.rs` is **not the real AST**. `ferrite-sql` (Agent 2) owns that
type and was being written in parallel; the planner needed something to be
developed against. The provisional type covers single-table
`SELECT`/`INSERT`/`UPDATE`/`DELETE` plus `CALL`, with expressions over
column references and literals.

Only `Planner::build_logical` reads it. Integration means rewriting that
one method against `ferrite_sql::ast` and deleting `src/ast.rs`.

## Index metadata

`ferrite_common::Catalog` has no notion of an index, so this crate declares
the narrow view it needs — `IndexCatalog` in `src/index.rs` — instead of
widening the shared v0 contract unilaterally. `NoIndexes` is a valid
implementation that always yields a sequential scan. The natural end state
is `ferrite-catalog` implementing `IndexCatalog`, or the method moving into
`ferrite_common::Catalog` once the agents coordinate on it.

## Known limits

- One table per statement: no joins, no subqueries, no set operations.
- No `GROUP BY`, no aggregates, no `ORDER BY`, no window functions.
- Projections may contain column references and literals only; computed
  expressions need a type-inference pass that does not exist yet.
- `LIMIT` without `OFFSET`.
- No plan cache. The executor does detect a plan built against a stale
  schema, so caching can be added without a correctness hole.
