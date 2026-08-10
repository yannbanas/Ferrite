//! Fixed rewrite rules. Ferrite v1 has no cost model and no statistics
//! (`docs/architecture.md`), so this is a single deterministic pass rather
//! than a search over equivalent plans.

use crate::expr::Expr;
use crate::logical::{combine_conjunction, split_conjunction, LogicalPlan, ProjectionItem};

/// Run every rule once, in a fixed order.
pub fn optimize(plan: LogicalPlan) -> LogicalPlan {
    push_predicates(plan, Vec::new())
}

/// Predicate pushdown.
///
/// `pending` carries conjuncts collected from `Filter` nodes above the
/// current one. They are pushed down until either they reach a `Scan` (the
/// good case — the scan can then pick an index and reject rows before they
/// are materialized) or they hit a node they cannot cross, at which point
/// they are re-materialized as a `Filter` immediately above it.
///
/// Barriers: `Limit` (filtering below it changes which rows survive) and
/// any `Projection` that is not a plain column list.
fn push_predicates(plan: LogicalPlan, mut pending: Vec<Expr>) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            pending.extend(split_conjunction(predicate));
            push_predicates(*input, pending)
        }

        LogicalPlan::Scan { source, filter } => {
            let mut conjuncts = filter.map(split_conjunction).unwrap_or_default();
            conjuncts.append(&mut pending);
            LogicalPlan::Scan {
                source,
                filter: combine_conjunction(conjuncts),
            }
        }

        LogicalPlan::Projection { input, items } => {
            if is_transparent(&items) {
                LogicalPlan::Projection {
                    input: Box::new(push_predicates(*input, pending)),
                    items,
                }
            } else {
                let inner = LogicalPlan::Projection {
                    input: Box::new(push_predicates(*input, Vec::new())),
                    items,
                };
                materialize(inner, pending)
            }
        }

        LogicalPlan::Limit { input, count } => {
            let inner = LogicalPlan::Limit {
                input: Box::new(push_predicates(*input, Vec::new())),
                count,
            };
            materialize(inner, pending)
        }

        LogicalPlan::Update {
            source,
            input,
            assignments,
        } => LogicalPlan::Update {
            source,
            input: Box::new(push_predicates(*input, pending)),
            assignments,
        },

        LogicalPlan::Delete { source, input } => LogicalPlan::Delete {
            source,
            input: Box::new(push_predicates(*input, pending)),
        },

        leaf @ (LogicalPlan::Insert { .. } | LogicalPlan::Call { .. }) => {
            materialize(leaf, pending)
        }
    }
}

/// A projection is transparent to pushdown when every item is a bare
/// column reference: the predicate below it still sees the same names.
fn is_transparent(items: &[ProjectionItem]) -> bool {
    items.iter().all(|i| matches!(i.expr, Expr::Column(_)))
}

fn materialize(plan: LogicalPlan, pending: Vec<Expr>) -> LogicalPlan {
    match combine_conjunction(pending) {
        None => plan,
        Some(predicate) => LogicalPlan::Filter {
            input: Box::new(plan),
            predicate,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::BinaryOp;
    use crate::logical::TableSource;
    use ferrite_common::{ColumnDef, DataType, Schema, Value};

    fn source() -> TableSource {
        TableSource {
            id: 1,
            name: "users".into(),
            schema: Schema {
                columns: vec![
                    ColumnDef::new("id", DataType::Int8, false),
                    ColumnDef::new("name", DataType::Text, true),
                ],
            },
        }
    }

    fn scan() -> LogicalPlan {
        LogicalPlan::Scan {
            source: source(),
            filter: None,
        }
    }

    fn projection(input: LogicalPlan, cols: &[&str]) -> LogicalPlan {
        LogicalPlan::Projection {
            input: Box::new(input),
            items: cols
                .iter()
                .map(|c| ProjectionItem {
                    expr: Expr::column(*c),
                    output_name: (*c).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn filter_is_pushed_into_the_scan() {
        let predicate = Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(7)));
        let plan = projection(
            LogicalPlan::Filter {
                input: Box::new(scan()),
                predicate: predicate.clone(),
            },
            &["name"],
        );

        let optimized = optimize(plan);

        match optimized {
            LogicalPlan::Projection { input, .. } => match *input {
                LogicalPlan::Scan { filter, .. } => assert_eq!(filter, Some(predicate)),
                other => panic!("expected Scan below Projection, got {other:?}"),
            },
            other => panic!("expected Projection at the root, got {other:?}"),
        }
    }

    #[test]
    fn conjunction_is_split_and_fully_pushed() {
        let plan = LogicalPlan::Filter {
            input: Box::new(scan()),
            predicate: Expr::and(
                Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(7))),
                Expr::binary(
                    Expr::column("name"),
                    BinaryOp::NotEq,
                    Expr::Literal(Value::Text("bob".into())),
                ),
            ),
        };

        let optimized = optimize(plan);

        let LogicalPlan::Scan { filter, .. } = optimized else {
            panic!("filter should have collapsed into the scan");
        };
        let conjuncts = split_conjunction(filter.expect("scan should carry a filter"));
        assert_eq!(conjuncts.len(), 2);
        assert!(conjuncts.contains(&Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(7)))));
    }

    #[test]
    fn stacked_filters_merge_into_one_scan_filter() {
        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(scan()),
                predicate: Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(7))),
            }),
            predicate: Expr::eq(Expr::column("name"), Expr::Literal(Value::Text("a".into()))),
        };

        let optimized = optimize(plan);

        let LogicalPlan::Scan { filter, .. } = optimized else {
            panic!("both filters should have collapsed into the scan");
        };
        assert_eq!(split_conjunction(filter.unwrap()).len(), 2);
    }

    #[test]
    fn limit_blocks_pushdown() {
        let predicate = Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(7)));
        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Limit {
                input: Box::new(scan()),
                count: 10,
            }),
            predicate,
        };

        let optimized = optimize(plan);

        match optimized {
            LogicalPlan::Filter { input, .. } => match *input {
                LogicalPlan::Limit { input, .. } => {
                    assert!(matches!(*input, LogicalPlan::Scan { filter: None, .. }));
                }
                other => panic!("expected Limit under Filter, got {other:?}"),
            },
            other => panic!("filter must stay above the limit, got {other:?}"),
        }
    }

    #[test]
    fn filter_below_a_limit_still_reaches_the_scan() {
        let predicate = Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(7)));
        let plan = LogicalPlan::Limit {
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(scan()),
                predicate: predicate.clone(),
            }),
            count: 10,
        };

        let optimized = optimize(plan);

        let LogicalPlan::Limit { input, .. } = optimized else {
            panic!("expected Limit at the root");
        };
        assert!(matches!(*input, LogicalPlan::Scan { filter: Some(f), .. } if f == predicate));
    }

    #[test]
    fn non_transparent_projection_blocks_pushdown() {
        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Projection {
                input: Box::new(scan()),
                items: vec![ProjectionItem {
                    expr: Expr::Literal(Value::Int4(1)),
                    output_name: "one".into(),
                }],
            }),
            predicate: Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(7))),
        };

        let optimized = optimize(plan);

        assert!(matches!(optimized, LogicalPlan::Filter { .. }));
    }

    #[test]
    fn update_predicate_reaches_the_scan() {
        let predicate = Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(7)));
        let plan = LogicalPlan::Update {
            source: source(),
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(scan()),
                predicate: predicate.clone(),
            }),
            assignments: vec![(1, Expr::Literal(Value::Text("x".into())))],
        };

        let optimized = optimize(plan);

        let LogicalPlan::Update { input, .. } = optimized else {
            panic!("expected Update at the root");
        };
        assert!(matches!(*input, LogicalPlan::Scan { filter: Some(f), .. } if f == predicate));
    }
}
