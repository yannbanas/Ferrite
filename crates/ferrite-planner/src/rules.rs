//! Fixed rewrite rules. Ferrite v1 has no cost model and no statistics
//! (`docs/architecture.md`), so this is a single deterministic pass rather
//! than a search over equivalent plans.

use crate::expr::Expr;
use crate::logical::{
    combine_conjunction, split_conjunction, JoinType, LogicalPlan, ProjectionItem,
};
use crate::scope::Scope;

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
/// Barriers: `Limit` (filtering below it changes which rows survive),
/// `Aggregate` and `Distinct` (a predicate above them is about the
/// collapsed rows, not the input ones), and any `Projection` that is not a
/// plain column list. `Sort` is not a barrier: reordering rows does not
/// change which of them a predicate keeps.
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

        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
        } => push_join(*left, *right, join_type, on, pending),

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

        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: Box::new(push_predicates(*input, pending)),
            keys,
        },

        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            let inner = LogicalPlan::Aggregate {
                input: Box::new(push_predicates(*input, Vec::new())),
                group_by,
                aggregates,
            };
            materialize(inner, pending)
        }

        LogicalPlan::Distinct { input } => {
            let inner = LogicalPlan::Distinct {
                input: Box::new(push_predicates(*input, Vec::new())),
            };
            materialize(inner, pending)
        }

        LogicalPlan::Limit {
            input,
            count,
            offset,
        } => {
            let inner = LogicalPlan::Limit {
                input: Box::new(push_predicates(*input, Vec::new())),
                count,
                offset,
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

/// Which side of a join a conjunct belongs to, when it belongs to exactly
/// one. A conjunct with no column references at all belongs to neither: it
/// would be pushed to an arbitrary side for no gain, and a `USING` equality
/// (already resolved to positions) must never be treated as one-sided.
enum Side {
    Left,
    Right,
}

fn side(expr: &Expr, left: &Scope, right: &Scope) -> Option<Side> {
    let references = expr.referenced_columns();
    if references.is_empty() {
        return None;
    }
    if references.iter().all(|r| left.can_resolve(r))
        && !references.iter().any(|r| right.can_resolve(r))
    {
        return Some(Side::Left);
    }
    if references.iter().all(|r| right.can_resolve(r))
        && !references.iter().any(|r| left.can_resolve(r))
    {
        return Some(Side::Right);
    }
    None
}

/// Pushing through a join is where getting it wrong is silent, so the two
/// directions are spelled out separately.
///
/// A `WHERE` conjunct may only sink into a side that is *not* null-extended
/// by this join: filtering `posts.id = 3` below a `LEFT JOIN posts` would
/// keep the unmatched left rows that the same predicate above the join
/// removes. An `ON` conjunct is the mirror — it may only sink into the side
/// that *is* null-extended, since restricting the preserved side there
/// would drop rows the join is supposed to keep. For an inner or cross
/// join neither side is preserved, so both directions are open, and a
/// `WHERE` that mentions both sides becomes the join predicate rather than
/// a filter over the full cross product.
fn push_join(
    left: LogicalPlan,
    right: LogicalPlan,
    join_type: JoinType,
    on: Option<Expr>,
    pending: Vec<Expr>,
) -> LogicalPlan {
    let (Some(left_scope), Some(right_scope)) = (left.scope(), right.scope()) else {
        let rebuilt = LogicalPlan::Join {
            left: Box::new(push_predicates(left, Vec::new())),
            right: Box::new(push_predicates(right, Vec::new())),
            join_type,
            on,
        };
        return materialize(rebuilt, pending);
    };

    let inner = matches!(join_type, JoinType::Inner | JoinType::Cross);
    let (mut to_left, mut to_right, mut above, mut condition) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());

    for conjunct in pending {
        match side(&conjunct, &left_scope, &right_scope) {
            Some(Side::Left) if !join_type.preserves_right() => to_left.push(conjunct),
            Some(Side::Right) if !join_type.preserves_left() => to_right.push(conjunct),
            _ if inner => condition.push(conjunct),
            _ => above.push(conjunct),
        }
    }

    for conjunct in on.map(split_conjunction).unwrap_or_default() {
        match side(&conjunct, &left_scope, &right_scope) {
            Some(Side::Left) if !join_type.preserves_left() => to_left.push(conjunct),
            Some(Side::Right) if !join_type.preserves_right() => to_right.push(conjunct),
            _ => condition.push(conjunct),
        }
    }

    let joined = LogicalPlan::Join {
        left: Box::new(push_predicates(left, to_left)),
        right: Box::new(push_predicates(right, to_right)),
        join_type,
        on: combine_conjunction(condition),
    };
    materialize(joined, above)
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
            alias: None,
            schema: Schema {
                columns: vec![
                    ColumnDef::new("id", DataType::Int8, false),
                    ColumnDef::new("name", DataType::Text, true),
                ],
            },
        }
    }

    fn posts() -> TableSource {
        TableSource {
            id: 2,
            name: "posts".into(),
            alias: None,
            schema: Schema {
                columns: vec![
                    ColumnDef::new("author", DataType::Int8, false),
                    ColumnDef::new("title", DataType::Text, true),
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

    fn scan_posts() -> LogicalPlan {
        LogicalPlan::Scan {
            source: posts(),
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

    fn join(join_type: JoinType, predicate: Expr) -> LogicalPlan {
        LogicalPlan::Join {
            left: Box::new(scan()),
            right: Box::new(scan_posts()),
            join_type,
            on: Some(predicate),
        }
    }

    fn join_condition() -> Expr {
        Expr::eq(
            Expr::qualified_column("users", "id"),
            Expr::qualified_column("posts", "author"),
        )
    }

    fn scan_filters(plan: &LogicalPlan) -> Vec<Option<Expr>> {
        match plan {
            LogicalPlan::Scan { filter, .. } => vec![filter.clone()],
            other => other
                .children()
                .iter()
                .flat_map(|c| scan_filters(c))
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
                count: Some(10),
                offset: 0,
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
            count: Some(10),
            offset: 0,
        };

        let optimized = optimize(plan);

        let LogicalPlan::Limit { input, .. } = optimized else {
            panic!("expected Limit at the root");
        };
        assert!(matches!(*input, LogicalPlan::Scan { filter: Some(f), .. } if f == predicate));
    }

    #[test]
    fn sorting_does_not_block_pushdown() {
        let predicate = Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(7)));
        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Sort {
                input: Box::new(scan()),
                keys: Vec::new(),
            }),
            predicate: predicate.clone(),
        };

        let optimized = optimize(plan);

        let LogicalPlan::Sort { input, .. } = optimized else {
            panic!("expected Sort at the root, got {optimized:?}");
        };
        assert!(matches!(*input, LogicalPlan::Scan { filter: Some(f), .. } if f == predicate));
    }

    #[test]
    fn aggregation_blocks_pushdown() {
        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Aggregate {
                input: Box::new(scan()),
                group_by: vec![Expr::column("id")],
                aggregates: Vec::new(),
            }),
            predicate: Expr::eq(Expr::Slot(0), Expr::Literal(Value::Int8(7))),
        };

        assert!(matches!(optimize(plan), LogicalPlan::Filter { .. }));
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

    #[test]
    fn a_one_sided_where_reaches_the_scan_through_an_inner_join() {
        let plan = LogicalPlan::Filter {
            input: Box::new(join(JoinType::Inner, join_condition())),
            predicate: Expr::eq(
                Expr::qualified_column("posts", "title"),
                Expr::Literal(Value::Text("hi".into())),
            ),
        };

        let optimized = optimize(plan);

        let LogicalPlan::Join { on, .. } = &optimized else {
            panic!("the filter should have sunk into the join, got {optimized:?}");
        };
        assert_eq!(*on, Some(join_condition()));
        let filters = scan_filters(&optimized);
        assert_eq!(filters[0], None, "nothing belongs on the users side");
        assert!(filters[1].is_some(), "the title predicate belongs to posts");
    }

    #[test]
    fn a_where_on_the_null_extended_side_of_a_left_join_stays_above_it() {
        let plan = LogicalPlan::Filter {
            input: Box::new(join(JoinType::Left, join_condition())),
            predicate: Expr::eq(
                Expr::qualified_column("posts", "title"),
                Expr::Literal(Value::Text("hi".into())),
            ),
        };

        let optimized = optimize(plan);

        assert!(
            matches!(optimized, LogicalPlan::Filter { .. }),
            "pushing it below would resurrect the unmatched left rows: {optimized:?}"
        );
        assert_eq!(scan_filters(&optimized), vec![None, None]);
    }

    #[test]
    fn a_where_on_the_preserved_side_of_a_left_join_still_reaches_its_scan() {
        let predicate = Expr::eq(
            Expr::qualified_column("users", "id"),
            Expr::Literal(Value::Int8(7)),
        );
        let plan = LogicalPlan::Filter {
            input: Box::new(join(JoinType::Left, join_condition())),
            predicate: predicate.clone(),
        };

        let optimized = optimize(plan);

        assert!(matches!(optimized, LogicalPlan::Join { .. }));
        assert_eq!(scan_filters(&optimized), vec![Some(predicate), None]);
    }

    #[test]
    fn an_on_conjunct_sinks_into_the_null_extended_side_only() {
        let restriction = Expr::eq(
            Expr::qualified_column("posts", "title"),
            Expr::Literal(Value::Text("hi".into())),
        );
        let optimized = optimize(join(
            JoinType::Left,
            Expr::and(join_condition(), restriction.clone()),
        ));

        let LogicalPlan::Join { on, .. } = &optimized else {
            panic!("expected a Join at the root, got {optimized:?}");
        };
        assert_eq!(*on, Some(join_condition()));
        assert_eq!(scan_filters(&optimized), vec![None, Some(restriction)]);
    }

    #[test]
    fn an_on_conjunct_on_the_preserved_side_of_a_left_join_stays_in_the_condition() {
        let restriction = Expr::eq(
            Expr::qualified_column("users", "id"),
            Expr::Literal(Value::Int8(7)),
        );
        let optimized = optimize(join(
            JoinType::Left,
            Expr::and(join_condition(), restriction),
        ));

        let LogicalPlan::Join { on, .. } = &optimized else {
            panic!("expected a Join at the root, got {optimized:?}");
        };
        assert_eq!(split_conjunction(on.clone().unwrap()).len(), 2);
        assert_eq!(scan_filters(&optimized), vec![None, None]);
    }

    #[test]
    fn a_cross_join_with_a_relating_where_becomes_a_join_predicate() {
        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Join {
                left: Box::new(scan()),
                right: Box::new(scan_posts()),
                join_type: JoinType::Cross,
                on: None,
            }),
            predicate: join_condition(),
        };

        let optimized = optimize(plan);

        let LogicalPlan::Join { on, .. } = &optimized else {
            panic!("expected a Join at the root, got {optimized:?}");
        };
        assert_eq!(*on, Some(join_condition()));
    }
}
