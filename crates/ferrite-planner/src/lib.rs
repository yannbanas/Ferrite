//! Owned by Agent 3, alongside `ferrite-exec` and `ferrite-proc`. Turns a
//! `ferrite-sql` AST into a logical plan, then a physical plan, using a
//! small set of fixed rules (predicate pushdown, index-vs-scan choice by
//! simple heuristic) rather than a statistics-driven cost model — see
//! workspace README for why that's the v1 scope.

pub struct NotYetImplemented;
