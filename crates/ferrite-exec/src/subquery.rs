//! Running the subqueries a plan carries, before the plan itself runs.
//!
//! Ferrite v1 executes only *uncorrelated* `IN (SELECT ...)`: the subquery
//! mentions nothing from the row being tested, so it has one answer for the
//! whole statement. That lets it be run once, up front, and folded into an
//! ordinary value test — no per-row subplan, and no nested executor state.
//!
//! A correlated subquery, a scalar subquery and `EXISTS` are all refused by
//! the planner instead, because each of them needs the subplan re-run per
//! outer row and there is no node shape for that yet.

use ferrite_common::{FerriteError, Value};
use ferrite_planner::{BinaryOp, PhysExpr, PhysicalPlan};

/// Whether `plan` still holds an unrun subquery, so the executor can skip
/// cloning a plan that has none — which is every plan an application
/// mostly runs.
pub(crate) fn present_in(plan: &PhysicalPlan) -> bool {
    let mut found = false;
    walk_exprs(plan, &mut |expr| found |= in_expr(expr));
    found
}

fn in_expr(expr: &PhysExpr) -> bool {
    match expr {
        PhysExpr::InSubquery { .. } => true,
        PhysExpr::Column(_) | PhysExpr::Literal(_) => false,
        PhysExpr::Not(inner) | PhysExpr::IsNull(inner) | PhysExpr::Cast { expr: inner, .. } => {
            in_expr(inner)
        }
        PhysExpr::Binary { left, right, .. } => in_expr(left) || in_expr(right),
        PhysExpr::Like { expr, pattern, .. } => in_expr(expr) || in_expr(pattern),
        PhysExpr::Function { args, .. } => args.iter().any(in_expr),
        PhysExpr::Case {
            operand,
            branches,
            else_result,
        } => {
            operand.iter().chain(else_result.iter()).any(|e| in_expr(e))
                || branches
                    .iter()
                    .any(|(when, then)| in_expr(when) || in_expr(then))
        }
    }
}

/// Every expression in `plan` and its children. The read-only twin of
/// [`expressions_of`] + [`children_of`]; the two must move together.
fn walk_exprs(plan: &PhysicalPlan, visit: &mut impl FnMut(&PhysExpr)) {
    match plan {
        PhysicalPlan::SeqScan { filter, .. } => filter.iter().for_each(visit),
        PhysicalPlan::IndexScan { residual, .. } => residual.iter().for_each(visit),
        PhysicalPlan::Filter { predicate, input } => {
            visit(predicate);
            walk_exprs(input, visit);
        }
        PhysicalPlan::Projection { exprs, input, .. } => {
            exprs.iter().for_each(&mut *visit);
            walk_exprs(input, visit);
        }
        PhysicalPlan::Sort { keys, input } => {
            keys.iter().for_each(|key| visit(&key.expr));
            walk_exprs(input, visit);
        }
        PhysicalPlan::Limit { input, .. } | PhysicalPlan::Distinct { input } => {
            walk_exprs(input, visit)
        }
        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            ..
        } => {
            group_by.iter().for_each(&mut *visit);
            aggregates
                .iter()
                .filter_map(|a| a.arg.as_ref())
                .for_each(&mut *visit);
            walk_exprs(input, visit);
        }
        PhysicalPlan::NestedLoopJoin {
            left,
            right,
            predicate,
            ..
        } => {
            predicate.iter().for_each(&mut *visit);
            walk_exprs(left, visit);
            walk_exprs(right, visit);
        }
        PhysicalPlan::Insert { rows, .. } => rows.iter().flatten().for_each(visit),
        PhysicalPlan::Update {
            source,
            assignments,
            ..
        } => {
            assignments.iter().for_each(|(_, e)| visit(e));
            walk_exprs(source, visit);
        }
        PhysicalPlan::Delete { source, .. } => walk_exprs(source, visit),
        PhysicalPlan::CallProcedure { args, .. } => args.iter().for_each(visit),
    }
}

/// Replace every `IN (SELECT ...)` in `plan` with the test its subquery
/// evaluates to, using `run` to execute each subplan.
pub(crate) fn resolve(
    plan: &mut PhysicalPlan,
    run: &mut dyn FnMut(&PhysicalPlan) -> Result<Vec<Value>, FerriteError>,
) -> Result<(), FerriteError> {
    for expr in expressions_of(plan) {
        resolve_expr(expr, run)?;
    }
    for child in children_of(plan) {
        resolve(child, run)?;
    }
    Ok(())
}

fn resolve_expr(
    expr: &mut PhysExpr,
    run: &mut dyn FnMut(&PhysicalPlan) -> Result<Vec<Value>, FerriteError>,
) -> Result<(), FerriteError> {
    if let PhysExpr::InSubquery {
        expr: tested,
        subquery,
        negated,
    } = expr
    {
        resolve(subquery, run)?;
        resolve_expr(tested, run)?;
        let values = run(subquery)?;
        *expr = value_test(tested.as_ref().clone(), &values, *negated);
        return Ok(());
    }
    for child in subexpressions_of(expr) {
        resolve_expr(child, run)?;
    }
    Ok(())
}

/// `x IN (v1, v2, …)` as a balanced tree of equalities, matching how the
/// planner already expands a literal `IN` list.
///
/// Balanced rather than chained for the reason `balanced_or` gives in
/// `ferrite-planner`, and it matters more here than there: the width of
/// this tree is the *row count of the subquery*, which no one wrote out and
/// nothing caps below the result-set budget. Chained, a subquery returning
/// a few thousand rows is a tree a few thousand levels tall, and evaluating
/// or dropping it recurses once per level — a stack overflow, which aborts
/// the process rather than unwinding into an error the connection could
/// report.
///
/// An empty result makes `IN` false and `NOT IN` true for every row,
/// including rows where `x` is null — which is what SQL says, and the one
/// case where the null-propagation rule below does not apply.
fn value_test(tested: PhysExpr, values: &[Value], negated: bool) -> PhysExpr {
    if values.is_empty() {
        return PhysExpr::Literal(Value::Boolean(negated));
    }
    let mut tests: Vec<PhysExpr> = values
        .iter()
        .map(|value| {
            PhysExpr::binary(
                tested.clone(),
                BinaryOp::Eq,
                PhysExpr::Literal(value.clone()),
            )
        })
        .collect();
    while tests.len() > 1 {
        let mut folded = Vec::with_capacity(tests.len().div_ceil(2));
        let mut pairs = tests.into_iter();
        while let Some(left) = pairs.next() {
            folded.push(match pairs.next() {
                Some(right) => PhysExpr::binary(left, BinaryOp::Or, right),
                None => left,
            });
        }
        tests = folded;
    }
    let any = tests
        .pop()
        .expect("a non-empty value list folds to one test");
    match negated {
        false => any,
        true => PhysExpr::Not(Box::new(any)),
    }
}

fn subexpressions_of(expr: &mut PhysExpr) -> Vec<&mut PhysExpr> {
    match expr {
        PhysExpr::Column(_) | PhysExpr::Literal(_) => Vec::new(),
        PhysExpr::Not(inner) | PhysExpr::IsNull(inner) | PhysExpr::Cast { expr: inner, .. } => {
            vec![inner]
        }
        PhysExpr::Binary { left, right, .. } => vec![left, right],
        PhysExpr::Like { expr, pattern, .. } => vec![expr, pattern],
        PhysExpr::Function { args, .. } => args.iter_mut().collect(),
        PhysExpr::InSubquery { expr, .. } => vec![expr],
        PhysExpr::Case {
            operand,
            branches,
            else_result,
        } => {
            let mut out: Vec<&mut PhysExpr> = Vec::new();
            if let Some(operand) = operand {
                out.push(operand);
            }
            for (when, then) in branches {
                out.push(when);
                out.push(then);
            }
            if let Some(else_result) = else_result {
                out.push(else_result);
            }
            out
        }
    }
}

fn expressions_of(plan: &mut PhysicalPlan) -> Vec<&mut PhysExpr> {
    match plan {
        PhysicalPlan::SeqScan { filter, .. } => filter.iter_mut().collect(),
        PhysicalPlan::IndexScan { residual, .. } => residual.iter_mut().collect(),
        PhysicalPlan::Filter { predicate, .. } => vec![predicate],
        PhysicalPlan::Projection { exprs, .. } => exprs.iter_mut().collect(),
        PhysicalPlan::Sort { keys, .. } => keys.iter_mut().map(|key| &mut key.expr).collect(),
        PhysicalPlan::NestedLoopJoin { predicate, .. } => predicate.iter_mut().collect(),
        PhysicalPlan::Aggregate {
            group_by,
            aggregates,
            ..
        } => group_by
            .iter_mut()
            .chain(aggregates.iter_mut().filter_map(|a| a.arg.as_mut()))
            .collect(),
        PhysicalPlan::Insert { rows, .. } => rows.iter_mut().flatten().collect(),
        PhysicalPlan::Update { assignments, .. } => {
            assignments.iter_mut().map(|(_, e)| e).collect()
        }
        PhysicalPlan::CallProcedure { args, .. } => args.iter_mut().collect(),
        PhysicalPlan::Limit { .. }
        | PhysicalPlan::Distinct { .. }
        | PhysicalPlan::Delete { .. } => Vec::new(),
    }
}

fn children_of(plan: &mut PhysicalPlan) -> Vec<&mut PhysicalPlan> {
    match plan {
        PhysicalPlan::SeqScan { .. }
        | PhysicalPlan::IndexScan { .. }
        | PhysicalPlan::Insert { .. }
        | PhysicalPlan::CallProcedure { .. } => Vec::new(),
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Projection { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. }
        | PhysicalPlan::Distinct { input, .. }
        | PhysicalPlan::Aggregate { input, .. } => vec![input],
        PhysicalPlan::NestedLoopJoin { left, right, .. } => vec![left, right],
        PhysicalPlan::Update { source, .. } | PhysicalPlan::Delete { source, .. } => vec![source],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_result_makes_in_false_and_not_in_true() {
        assert_eq!(
            value_test(PhysExpr::Column(0), &[], false),
            PhysExpr::Literal(Value::Boolean(false))
        );
        assert_eq!(
            value_test(PhysExpr::Column(0), &[], true),
            PhysExpr::Literal(Value::Boolean(true))
        );
    }

    #[test]
    fn values_become_a_chain_of_equalities() {
        let test = value_test(
            PhysExpr::Column(0),
            &[Value::Int8(1), Value::Int8(2)],
            false,
        );
        let PhysExpr::Binary { op, .. } = test else {
            panic!("expected a disjunction");
        };
        assert_eq!(op, BinaryOp::Or);
    }

    /// A subquery's row count is not something a client wrote out, so this
    /// is the width that has to stay shallow: chained, these four thousand
    /// values are four thousand levels, and merely dropping the tree
    /// overflows the stack.
    #[test]
    fn a_wide_result_stays_a_shallow_tree() {
        fn height(expr: &PhysExpr) -> usize {
            match expr {
                PhysExpr::Binary { left, right, .. } => 1 + height(left).max(height(right)),
                PhysExpr::Not(inner) => 1 + height(inner),
                _ => 1,
            }
        }
        let values: Vec<Value> = (0..4000).map(Value::Int8).collect();
        let test = value_test(PhysExpr::Column(0), &values, true);
        assert!(
            height(&test) <= 20,
            "4000 values produced a tree {} levels tall",
            height(&test)
        );
    }
}
