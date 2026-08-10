//! `ferrite-sql` AST → the planner's IR.
//!
//! `ferrite-sql` parses more than `ferrite-exec` can run: subqueries, set
//! operations, common table expressions. Everything this
//! module cannot project onto [`crate::expr`] becomes a
//! [`FerriteError::Plan`], never a panic and never a silently wrong plan.
//! The rejections are all in one place so the gap between "parses" and
//! "executes" stays legible.

use ferrite_common::{ColumnDefault, DataType, FerriteError, Schema, Value};
use ferrite_sql::ast as sql;

use crate::expr::{BinaryOp, ColumnRef, Expr};
use crate::logical::LogicalPlan;
use crate::scalar::ScalarFunc;

/// How the lowerer turns a parsed subquery into a plan. The planner owns
/// the catalog, so it hands this in rather than the lowerer reaching for
/// metadata it has no access to.
pub(crate) type SubPlanner<'a> = &'a dyn Fn(&sql::Query) -> Result<LogicalPlan, FerriteError>;

pub(crate) fn unsupported(what: &str) -> FerriteError {
    FerriteError::Plan(format!("{what} is not supported by the v1 planner"))
}

/// The collations Ferrite recognises. SQLite's third built-in, `RTRIM`, is
/// not among them: nothing in the audited corpus uses it, and accepting it
/// as a no-op would compare strings SQLite considers equal as different.
enum Collation {
    /// Byte-order comparison — the default, so an explicit `COLLATE
    /// BINARY` lowers to nothing at all.
    Binary,
    /// Case-insensitive comparison.
    Nocase,
}

fn collation_kind(name: &str) -> Result<Collation, FerriteError> {
    if name.eq_ignore_ascii_case("nocase") {
        return Ok(Collation::Nocase);
    }
    if name.eq_ignore_ascii_case("binary") {
        return Ok(Collation::Binary);
    }
    Err(unsupported(&format!("the collation {name}")))
}

/// Whether an operand carries an explicit `COLLATE NOCASE`, validating any
/// collation name it does carry so an unknown one is refused rather than
/// silently ignored.
fn folds_case(expr: &sql::Expr) -> Result<bool, FerriteError> {
    match expr {
        sql::Expr::Collate { collation, .. } => {
            Ok(matches!(collation_kind(collation)?, Collation::Nocase))
        }
        _ => Ok(false),
    }
}

/// Evaluate `datetime`/`date` over literal arguments once, here, rather
/// than per row.
///
/// Two reasons, both load-bearing. `datetime('now')` is constant for the
/// duration of a statement in SQLite and PawChat depends on it: a `WHERE
/// created_at >= datetime('now', '-30 days')` re-read per row could
/// straddle a second boundary mid-scan and admit rows inconsistently. And
/// folding *before* the physical layer is what lets the result be coerced
/// to the column it is compared against — `datetime()` returns SQLite's
/// text shape, while a column holding ISO timestamps translates to
/// `TIMESTAMP`, and only a literal gets aligned across that gap.
///
/// Only these two functions fold. `randomblob` is deliberately left alone:
/// SQLite re-rolls it per row, and folding would hand every row the same
/// bytes.
fn fold_temporal(func: ScalarFunc, args: &[Expr]) -> Result<Option<Value>, FerriteError> {
    let with_time = match func {
        ScalarFunc::Datetime => true,
        ScalarFunc::Date => false,
        _ => return Ok(None),
    };
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            Expr::Literal(value) => values.push(value.clone()),
            _ => return Ok(None),
        }
    }
    ferrite_common::datetime::eval_datetime(&values, with_time).map(Some)
}

/// Wrap an already-lowered operand in the case fold, unless it is one
/// already — `a COLLATE NOCASE = b COLLATE NOCASE` folds each side once.
fn fold_case(expr: Expr) -> Expr {
    if matches!(
        &expr,
        Expr::Function {
            func: ScalarFunc::Nocase,
            ..
        }
    ) {
        return expr;
    }
    Expr::Function {
        func: ScalarFunc::Nocase,
        args: vec![expr],
    }
}

/// Resolve every `DEFAULT` in a freshly parsed schema against the type of
/// the column carrying it, and refuse the ones that cannot hold.
///
/// `ferrite-sql` produces the default untyped, because a quoted literal
/// has the same syntax whatever column it lands in: `DEFAULT '1970-01-01'`
/// parses as `Text` even on a `TIMESTAMP` column. This is the same
/// coercion [`coerce`] does for a `VALUES` literal, applied once at DDL
/// time so that a default which could never be written is rejected when
/// the table is defined rather than on the first `INSERT` that omits the
/// column.
///
/// Refused here: a default whose type cannot be coerced to the column's,
/// `DEFAULT NULL` on a `NOT NULL` column, and `CURRENT_TIMESTAMP` anywhere
/// but a `TIMESTAMP` column.
pub fn typecheck_defaults(schema: &mut Schema) -> Result<(), FerriteError> {
    for column in &mut schema.columns {
        let Some(default) = column.default.take() else {
            continue;
        };
        let bad = |what: &str| {
            FerriteError::InvalidDefinition(format!(
                "the DEFAULT of column `{}` {what}",
                column.name
            ))
        };
        let resolved = match default {
            ColumnDefault::CurrentTimestamp if column.data_type != DataType::Timestamp => {
                return Err(bad("is CURRENT_TIMESTAMP, which needs a TIMESTAMP column"))
            }
            ColumnDefault::CurrentTimestamp => ColumnDefault::CurrentTimestamp,
            ColumnDefault::Constant(Value::Null) if !column.nullable => {
                return Err(bad("is NULL, but the column is NOT NULL"))
            }
            ColumnDefault::Constant(Value::Null) => ColumnDefault::Constant(Value::Null),
            ColumnDefault::Constant(value) => {
                let Expr::Literal(coerced) = coerce(Expr::Literal(value), column.data_type)? else {
                    unreachable!("coerce maps a literal to a literal");
                };
                if coerced.data_type() != Some(column.data_type) {
                    return Err(bad(&format!(
                        "is {coerced:?}, which does not fit a {:?} column",
                        column.data_type
                    )));
                }
                ColumnDefault::Constant(coerced)
            }
        };
        column.default = Some(resolved);
    }
    Ok(())
}

/// Where an aggregate call lands in the row an `Aggregate` node produces:
/// the group keys come first, then `calls` in order.
#[derive(Clone, Copy)]
pub(crate) struct AggregateSlots<'a> {
    pub(crate) calls: &'a [sql::FunctionCall],
    pub(crate) offset: usize,
}

/// Lowers expressions, resolving `$n` placeholders against the bound
/// parameters and checking that qualified column references name a
/// relation actually in scope.
pub(crate) struct Lowerer<'a> {
    params: &'a [Value],
    /// Every table name and alias in scope, empty where column references
    /// are illegal (`INSERT ... VALUES`, `CALL` args).
    qualifiers: Vec<String>,
    /// Set while lowering the expressions that sit above an `Aggregate`
    /// node, where an aggregate call is a reference to an already-computed
    /// column rather than something to evaluate per row.
    aggregates: Option<AggregateSlots<'a>>,
    /// Set where a subquery is legal, i.e. inside a `SELECT`. `INSERT ...
    /// VALUES` and `CALL` arguments leave it unset, and a subquery there
    /// is the plan error it should be.
    subqueries: Option<SubPlanner<'a>>,
}

impl<'a> Lowerer<'a> {
    pub(crate) fn new(params: &'a [Value]) -> Self {
        Self {
            params,
            qualifiers: Vec::new(),
            aggregates: None,
            subqueries: None,
        }
    }

    pub(crate) fn with_subqueries(mut self, subqueries: SubPlanner<'a>) -> Self {
        self.subqueries = Some(subqueries);
        self
    }

    pub(crate) fn with_qualifiers(mut self, qualifiers: Vec<String>) -> Self {
        self.qualifiers = qualifiers;
        self
    }

    pub(crate) fn with_aggregates(mut self, slots: AggregateSlots<'a>) -> Self {
        self.aggregates = Some(slots);
        self
    }

    pub(crate) fn expr(&self, expr: &sql::Expr) -> Result<Expr, FerriteError> {
        match expr {
            sql::Expr::Literal(literal) => Ok(Expr::Literal(value_of(literal))),
            sql::Expr::Column(name) => self.column(name),
            sql::Expr::Parameter(n) => Ok(Expr::Literal(self.parameter(*n)?)),

            sql::Expr::UnaryOp { op, expr } => match op {
                sql::UnaryOp::Not => Ok(Expr::Not(Box::new(self.expr(expr)?))),
                sql::UnaryOp::Plus => self.expr(expr),
                sql::UnaryOp::Minus => match self.expr(expr)? {
                    Expr::Literal(value) => Ok(Expr::Literal(negate(value)?)),
                    other => Ok(Expr::binary(
                        Expr::Literal(Value::Int4(0)),
                        BinaryOp::Minus,
                        other,
                    )),
                },
            },

            // SQL takes the collation of a comparison from whichever
            // operand carries an explicit one, so `username = ? COLLATE
            // NOCASE` has to fold *both* sides, not just the parameter.
            sql::Expr::BinaryOp { left, op, right } => {
                let op = binary_op(*op);
                let (lowered_left, lowered_right) = (self.expr(left)?, self.expr(right)?);
                if op.is_comparison() && (folds_case(left)? || folds_case(right)?) {
                    return Ok(Expr::binary(
                        fold_case(lowered_left),
                        op,
                        fold_case(lowered_right),
                    ));
                }
                Ok(Expr::binary(lowered_left, op, lowered_right))
            }

            sql::Expr::IsNull { expr, negated } => {
                let inner = Expr::IsNull(Box::new(self.expr(expr)?));
                Ok(if *negated {
                    Expr::Not(Box::new(inner))
                } else {
                    inner
                })
            }

            // `BETWEEN` and `IN (list)` are pure sugar over comparisons, so
            // they cost nothing to support and are common enough in real
            // queries to be worth expanding rather than rejecting.
            sql::Expr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let subject = self.expr(expr)?;
                let range = Expr::and(
                    Expr::binary(subject.clone(), BinaryOp::GtEq, self.expr(low)?),
                    Expr::binary(subject, BinaryOp::LtEq, self.expr(high)?),
                );
                Ok(if *negated {
                    Expr::Not(Box::new(range))
                } else {
                    range
                })
            }

            sql::Expr::InList {
                expr,
                list,
                negated,
            } => {
                if list.is_empty() {
                    return Err(unsupported("an empty IN list"));
                }
                let subject = self.expr(expr)?;
                let mut tests = Vec::with_capacity(list.len());
                for item in list {
                    tests.push(Expr::eq(subject.clone(), self.expr(item)?));
                }
                let any = balanced_or(tests);
                Ok(if *negated {
                    Expr::Not(Box::new(any))
                } else {
                    any
                })
            }

            sql::Expr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                // An explicit `COLLATE NOCASE` on either operand makes the
                // match case-insensitive, exactly as `ILIKE` does.
                let nocase = folds_case(expr)? || folds_case(pattern)?;
                Ok(Expr::Like {
                    expr: Box::new(self.expr(expr)?),
                    pattern: Box::new(self.expr(pattern)?),
                    negated: *negated,
                    case_insensitive: *case_insensitive || nocase,
                })
            }

            sql::Expr::Collate { expr, collation } => {
                let inner = self.expr(expr)?;
                Ok(match collation_kind(collation)? {
                    Collation::Binary => inner,
                    Collation::Nocase => Expr::Function {
                        func: ScalarFunc::Nocase,
                        args: vec![inner],
                    },
                })
            }

            sql::Expr::Cast { expr, data_type } => Ok(Expr::Cast {
                expr: Box::new(self.expr(expr)?),
                data_type: *data_type,
            }),

            sql::Expr::Case {
                operand,
                branches,
                else_result,
            } => Ok(Expr::Case {
                operand: operand
                    .as_ref()
                    .map(|o| self.expr(o).map(Box::new))
                    .transpose()?,
                branches: branches
                    .iter()
                    .map(|(when, then)| Ok((self.expr(when)?, self.expr(then)?)))
                    .collect::<Result<_, FerriteError>>()?,
                else_result: else_result
                    .as_ref()
                    .map(|e| self.expr(e).map(Box::new))
                    .transpose()?,
            }),

            sql::Expr::Function(call) => self.function(call),

            sql::Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let Some(build) = self.subqueries else {
                    return Err(unsupported("a subquery here"));
                };
                Ok(Expr::InSubquery {
                    expr: Box::new(self.expr(expr)?),
                    subquery: Box::new(build(subquery)?),
                    negated: *negated,
                })
            }

            // A scalar subquery and `EXISTS` both need the subplan run
            // once per outer row, which the v1 executor has no shape for;
            // `IN (SELECT ...)` above is the uncorrelated case, run once.
            sql::Expr::Subquery(_) => Err(unsupported("a scalar subquery")),
            sql::Expr::Exists { .. } => Err(unsupported("EXISTS")),
        }
    }

    /// An aggregate call is a reference to a column the `Aggregate` node
    /// below has already computed. A scalar call lowers to itself. Anything
    /// else is a function the executor has no implementation for.
    fn function(&self, call: &sql::FunctionCall) -> Result<Expr, FerriteError> {
        if !sql::is_aggregate(&call.name) {
            let Some(func) = ScalarFunc::parse(&call.name) else {
                return Err(unsupported(&format!("the function {}", call.name)));
            };
            let sql::FunctionArgs::List(args) = &call.args else {
                return Err(FerriteError::Plan(format!(
                    "{}(*) is not a valid call; `*` is only an argument to count()",
                    call.name
                )));
            };
            if call.distinct {
                return Err(FerriteError::Plan(format!(
                    "DISTINCT is only meaningful inside an aggregate, not in {}()",
                    call.name
                )));
            }
            func.check_arity(args.len())?;
            let args = args
                .iter()
                .map(|arg| self.expr(arg))
                .collect::<Result<Vec<_>, FerriteError>>()?;
            return Ok(match fold_temporal(func, &args)? {
                Some(folded) => Expr::Literal(folded),
                None => Expr::Function { func, args },
            });
        }
        let Some(slots) = self.aggregates else {
            return Err(FerriteError::Plan(format!(
                "{}() is only allowed in a select list, HAVING or ORDER BY",
                call.name
            )));
        };
        let position = slots
            .calls
            .iter()
            .position(|c| c == call)
            .expect("every aggregate in the statement was collected first");
        Ok(Expr::Slot(slots.offset + position))
    }

    fn column(&self, name: &sql::ObjectName) -> Result<Expr, FerriteError> {
        let Some(qualifier) = name.qualifier() else {
            return Ok(Expr::Column(ColumnRef::new(name.base())));
        };
        if self.qualifiers.is_empty() {
            return Err(FerriteError::Plan(format!(
                "column reference {qualifier}.{} has no relation in scope",
                name.base()
            )));
        }
        if !self.qualifiers.iter().any(|q| q == qualifier) {
            return Err(FerriteError::Plan(format!(
                "no relation named {qualifier} in this statement"
            )));
        }
        Ok(Expr::Column(ColumnRef::qualified(qualifier, name.base())))
    }

    /// `$n` is 1-based on the wire, as PostgreSQL numbers placeholders.
    fn parameter(&self, n: u32) -> Result<Value, FerriteError> {
        let index = usize::try_from(n)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .ok_or_else(|| FerriteError::Plan(format!("invalid parameter ${n}")))?;
        self.params
            .get(index)
            .cloned()
            .ok_or_else(|| FerriteError::Plan(format!("${n} has no bound value")))
    }
}

/// Folds `tests` into a single `OR`, halving rather than chaining.
///
/// A left-deep chain of a thousand `OR`s is a tree a thousand levels tall,
/// and every later pass — binding, optimizing, evaluating, dropping —
/// walks it recursively. Balanced, the same thousand terms are ten levels.
/// `OR` is associative, so the two trees mean the same thing.
fn balanced_or(mut tests: Vec<Expr>) -> Expr {
    debug_assert!(!tests.is_empty(), "callers reject an empty IN list");
    while tests.len() > 1 {
        let mut folded = Vec::with_capacity(tests.len().div_ceil(2));
        let mut pairs = tests.into_iter();
        while let Some(left) = pairs.next() {
            folded.push(match pairs.next() {
                Some(right) => Expr::binary(left, BinaryOp::Or, right),
                None => left,
            });
        }
        tests = folded;
    }
    tests
        .pop()
        .unwrap_or(Expr::Literal(ferrite_common::Value::Boolean(false)))
}

/// Every aggregate call in `expr`, appended to `out` without duplicates so
/// `count(*)` written twice is computed once.
///
/// An aggregate inside another aggregate has no meaning — there is no inner
/// grouping for it to run over — and is refused here rather than producing
/// a slot that refers to itself.
pub(crate) fn collect_aggregates(
    expr: &sql::Expr,
    out: &mut Vec<sql::FunctionCall>,
) -> Result<(), FerriteError> {
    walk(expr, &mut |e| {
        if let sql::Expr::Function(call) = e {
            if sql::is_aggregate(&call.name) {
                if let sql::FunctionArgs::List(args) = &call.args {
                    for arg in args {
                        if contains_aggregate(arg) {
                            return Err(unsupported("an aggregate inside another aggregate"));
                        }
                    }
                }
                if !out.contains(call) {
                    out.push(call.clone());
                }
            }
        }
        Ok(())
    })
}

pub(crate) fn contains_aggregate(expr: &sql::Expr) -> bool {
    let mut found = false;
    let _ = walk(expr, &mut |e| {
        if let sql::Expr::Function(call) = e {
            found |= sql::is_aggregate(&call.name);
        }
        Ok(())
    });
    found
}

/// Pre-order walk over the sub-expressions this planner understands.
/// Subqueries are not descended into: they are rejected on their own, and
/// an aggregate inside one belongs to that query, not to this one.
fn walk(
    expr: &sql::Expr,
    visit: &mut impl FnMut(&sql::Expr) -> Result<(), FerriteError>,
) -> Result<(), FerriteError> {
    visit(expr)?;
    match expr {
        sql::Expr::Literal(_)
        | sql::Expr::Column(_)
        | sql::Expr::Parameter(_)
        | sql::Expr::Subquery(_)
        | sql::Expr::Exists { .. } => {}
        sql::Expr::UnaryOp { expr, .. }
        | sql::Expr::IsNull { expr, .. }
        | sql::Expr::Cast { expr, .. }
        | sql::Expr::Collate { expr, .. }
        | sql::Expr::InSubquery { expr, .. } => walk(expr, visit)?,
        sql::Expr::BinaryOp { left, right, .. } => {
            walk(left, visit)?;
            walk(right, visit)?;
        }
        sql::Expr::Between {
            expr, low, high, ..
        } => {
            walk(expr, visit)?;
            walk(low, visit)?;
            walk(high, visit)?;
        }
        sql::Expr::InList { expr, list, .. } => {
            walk(expr, visit)?;
            for item in list {
                walk(item, visit)?;
            }
        }
        sql::Expr::Like { expr, pattern, .. } => {
            walk(expr, visit)?;
            walk(pattern, visit)?;
        }
        sql::Expr::Case {
            operand,
            branches,
            else_result,
        } => {
            for e in operand.iter().chain(else_result.iter()) {
                walk(e, visit)?;
            }
            for (when, then) in branches {
                walk(when, visit)?;
                walk(then, visit)?;
            }
        }
        sql::Expr::Function(call) => {
            if let sql::FunctionArgs::List(args) = &call.args {
                for arg in args {
                    walk(arg, visit)?;
                }
            }
        }
    }
    Ok(())
}

/// Replace every subtree equal to one of `keys` with the slot it occupies
/// in the aggregate's output row. This is what makes `SELECT dept,
/// count(*) ... GROUP BY dept` legal: `dept` above the aggregate means the
/// group key, not the column it was computed from.
pub(crate) fn substitute_group_keys(expr: Expr, keys: &[Expr]) -> Expr {
    if let Some(position) = keys.iter().position(|key| *key == expr) {
        return Expr::Slot(position);
    }
    match expr {
        Expr::Binary { left, op, right } => Expr::Binary {
            left: Box::new(substitute_group_keys(*left, keys)),
            op,
            right: Box::new(substitute_group_keys(*right, keys)),
        },
        Expr::Not(inner) => Expr::Not(Box::new(substitute_group_keys(*inner, keys))),
        Expr::IsNull(inner) => Expr::IsNull(Box::new(substitute_group_keys(*inner, keys))),
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => Expr::Like {
            expr: Box::new(substitute_group_keys(*expr, keys)),
            pattern: Box::new(substitute_group_keys(*pattern, keys)),
            negated,
            case_insensitive,
        },
        Expr::Cast { expr, data_type } => Expr::Cast {
            expr: Box::new(substitute_group_keys(*expr, keys)),
            data_type,
        },
        Expr::Function { func, args } => Expr::Function {
            func,
            args: args
                .into_iter()
                .map(|arg| substitute_group_keys(arg, keys))
                .collect(),
        },
        Expr::Case {
            operand,
            branches,
            else_result,
        } => Expr::Case {
            operand: operand.map(|o| Box::new(substitute_group_keys(*o, keys))),
            branches: branches
                .into_iter()
                .map(|(when, then)| {
                    (
                        substitute_group_keys(when, keys),
                        substitute_group_keys(then, keys),
                    )
                })
                .collect(),
            else_result: else_result.map(|e| Box::new(substitute_group_keys(*e, keys))),
        },
        other => other,
    }
}

/// A column reference that survived [`substitute_group_keys`] is a column
/// read outside any aggregate and outside the grouping — one row of the
/// group would have to be picked arbitrarily, which SQL does not allow.
pub(crate) fn reject_ungrouped(expr: &Expr) -> Result<(), FerriteError> {
    match expr.referenced_columns().first() {
        None => Ok(()),
        Some(reference) => Err(FerriteError::Plan(format!(
            "{reference} must appear in the GROUP BY clause or be used in an aggregate function"
        ))),
    }
}

/// Integer literals take the narrowest type that fits, so a literal can be
/// assigned to an `INT` column (the executor widens `Int4 → Int8 → Float8`
/// but never narrows).
fn value_of(literal: &sql::Literal) -> Value {
    match literal {
        sql::Literal::Null => Value::Null,
        sql::Literal::Boolean(b) => Value::Boolean(*b),
        sql::Literal::Int(n) => match i32::try_from(*n) {
            Ok(small) => Value::Int4(small),
            Err(_) => Value::Int8(*n),
        },
        sql::Literal::Float(f) => Value::Float8(*f),
        sql::Literal::String(s) => Value::Text(s.clone()),
    }
}

fn negate(value: Value) -> Result<Value, FerriteError> {
    match value {
        Value::Int4(n) => Ok(Value::Int4(-n)),
        Value::Int8(n) => Ok(Value::Int8(-n)),
        Value::Float8(f) => Ok(Value::Float8(-f)),
        other => Err(FerriteError::Plan(format!("cannot negate {other:?}"))),
    }
}

fn binary_op(op: sql::BinaryOp) -> BinaryOp {
    match op {
        sql::BinaryOp::Or => BinaryOp::Or,
        sql::BinaryOp::And => BinaryOp::And,
        sql::BinaryOp::Eq => BinaryOp::Eq,
        sql::BinaryOp::NotEq => BinaryOp::NotEq,
        sql::BinaryOp::Lt => BinaryOp::Lt,
        sql::BinaryOp::LtEq => BinaryOp::LtEq,
        sql::BinaryOp::Gt => BinaryOp::Gt,
        sql::BinaryOp::GtEq => BinaryOp::GtEq,
        sql::BinaryOp::Plus => BinaryOp::Plus,
        sql::BinaryOp::Minus => BinaryOp::Minus,
        sql::BinaryOp::Multiply => BinaryOp::Multiply,
        sql::BinaryOp::Divide => BinaryOp::Divide,
        sql::BinaryOp::Modulo => BinaryOp::Modulo,
        sql::BinaryOp::Concat => BinaryOp::Concat,
    }
}

/// Unwrap a [`sql::Query`] down to the one `SELECT` the executor can run,
/// carrying the `ORDER BY`/`LIMIT`/`OFFSET` that apply to it.
pub(crate) struct QueryTail<'a> {
    pub(crate) select: &'a sql::Select,
    pub(crate) order_by: &'a [sql::OrderByItem],
    pub(crate) limit: Option<&'a sql::Expr>,
    pub(crate) offset: Option<&'a sql::Expr>,
}

pub(crate) fn single_select(query: &sql::Query) -> Result<QueryTail<'_>, FerriteError> {
    if !query.with.is_empty() {
        return Err(unsupported("WITH (common table expressions)"));
    }
    let select = match &query.body {
        sql::SetExpr::Select(select) => select.as_ref(),
        // A parenthesised query keeps its own `SELECT`, but the outer
        // clauses are the ones that apply.
        sql::SetExpr::Query(inner) => single_select(inner)?.select,
        sql::SetExpr::SetOp { .. } => {
            return Err(unsupported("UNION/INTERSECT/EXCEPT"));
        }
    };
    Ok(QueryTail {
        select,
        order_by: &query.order_by,
        limit: query.limit.as_ref(),
        offset: query.offset.as_ref(),
    })
}

/// `LIMIT`/`OFFSET` must be a constant, but not necessarily a literal: a
/// bound `$n` is substituted before this runs, which is how a paginated
/// application query arrives.
pub(crate) fn row_count(
    expr: Option<&sql::Expr>,
    lowerer: &Lowerer<'_>,
    clause: &str,
) -> Result<Option<u64>, FerriteError> {
    let Some(expr) = expr else {
        return Ok(None);
    };
    let bad = || FerriteError::Plan(format!("{clause} must be a non-negative integer constant"));
    let Expr::Literal(value) = lowerer.expr(expr)? else {
        return Err(bad());
    };
    let n = match value {
        Value::Int4(n) => i64::from(n),
        Value::Int8(n) => n,
        // `LIMIT NULL` means "no limit" in PostgreSQL.
        Value::Null => return Ok(None),
        _ => return Err(bad()),
    };
    u64::try_from(n).map(Some).map_err(|_| bad())
}

/// Coerce a literal to the type of the column it is being written into.
///
/// The parser produces one `Value` shape per literal syntax, so a quoted
/// string is always `Text`, even when it is written into a `UUID`,
/// `TIMESTAMP` or `JSON` column. PostgreSQL resolves that during parse
/// analysis; this is the same job, kept to literals, since a computed
/// expression would need a type-inference pass the v1 planner does not
/// have.
/// Numeric literals are widened here too, so the row that reaches storage
/// already carries the column's declared variant — triggers and the wire
/// encoder both read the stored variant, so leaving an `Int4` in a `BIGINT`
/// column would be visible in both.
pub(crate) fn coerce(expr: Expr, target: DataType) -> Result<Expr, FerriteError> {
    let Expr::Literal(value) = &expr else {
        return Ok(expr);
    };
    Ok(match (value, target) {
        (Value::Text(text), DataType::Uuid) => Expr::Literal(Value::Uuid(parse_uuid(text)?)),
        (Value::Text(text), DataType::Timestamp) => {
            Expr::Literal(Value::Timestamp(parse_timestamp(text)?))
        }
        (Value::Text(text), DataType::Json) => Expr::Literal(Value::Json(text.clone())),
        (Value::Int4(n), DataType::Int8) => Expr::Literal(Value::Int8(i64::from(*n))),
        (Value::Int4(n), DataType::Float8) => Expr::Literal(Value::Float8(f64::from(*n))),
        (Value::Int8(n), DataType::Float8) => Expr::Literal(Value::Float8(*n as f64)),
        _ => expr,
    })
}

fn parse_uuid(text: &str) -> Result<u128, FerriteError> {
    let mut bits: u128 = 0;
    let mut digits = 0;
    for c in text.chars() {
        if c == '-' {
            continue;
        }
        let nibble = c
            .to_digit(16)
            .ok_or_else(|| FerriteError::Plan(format!("{text:?} is not a UUID")))?;
        bits = (bits << 4) | u128::from(nibble);
        digits += 1;
    }
    if digits != 32 {
        return Err(FerriteError::Plan(format!("{text:?} is not a UUID")));
    }
    Ok(bits)
}

/// `YYYY-MM-DD[ T]HH:MM:SS[.ffffff][Z]`, or a bare `YYYY-MM-DD`, to
/// microseconds since the Unix epoch. A trailing offset other than `Z` is
/// refused rather than silently read as UTC — Ferrite stores UTC only, and
/// quietly dropping an offset would shift the value.
fn parse_timestamp(text: &str) -> Result<i64, FerriteError> {
    let bad = || FerriteError::Plan(format!("{text:?} is not a timestamp"));
    let text = text.trim().trim_end_matches('Z');
    let (date, time) = match text.split_once(['T', ' ']) {
        Some((date, time)) => (date, time),
        None => (text, "00:00:00"),
    };

    let mut parts = date.split('-');
    let year: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let month: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let day: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(bad());
    }

    let (time, fraction) = match time.split_once('.') {
        Some((time, fraction)) => (time, fraction),
        None => (time, ""),
    };
    let mut parts = time.split(':');
    let hour: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let minute: i64 = parts.next().unwrap_or("0").parse().map_err(|_| bad())?;
    let second: i64 = parts.next().unwrap_or("0").parse().map_err(|_| bad())?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return Err(bad());
    }

    let mut micros = 0i64;
    if !fraction.is_empty() {
        if fraction.len() > 6 || !fraction.chars().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        micros = fraction.parse::<i64>().map_err(|_| bad())?
            * 10i64.pow(6 - u32::try_from(fraction.len()).map_err(|_| bad())?);
    }

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    seconds
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(micros))
        .ok_or_else(bad)
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 for a proleptic
/// Gregorian date, with no table and no branch on leap years.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_literals_take_the_narrowest_type_that_fits() {
        assert_eq!(value_of(&sql::Literal::Int(1)), Value::Int4(1));
        assert_eq!(
            value_of(&sql::Literal::Int(i64::from(i32::MAX) + 1)),
            Value::Int8(i64::from(i32::MAX) + 1)
        );
    }

    #[test]
    fn uuid_text_is_accepted_with_and_without_hyphens() {
        let hyphenated = parse_uuid("0190f0d8-4b1a-7c3e-9d2f-1a2b3c4d5e6f").unwrap();
        let bare = parse_uuid("0190f0d84b1a7c3e9d2f1a2b3c4d5e6f").unwrap();
        assert_eq!(hyphenated, bare);
        assert!(parse_uuid("not-a-uuid").is_err());
        assert!(parse_uuid("0190f0d8").is_err());
    }

    #[test]
    fn timestamps_convert_to_microseconds_since_the_epoch() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(parse_timestamp("1970-01-02").unwrap(), 86_400_000_000);
        assert_eq!(
            parse_timestamp("2024-02-29 12:00:00.5").unwrap(),
            1_709_208_000_500_000
        );
        assert_eq!(parse_timestamp("1969-12-31T23:59:59Z").unwrap(), -1_000_000);
        assert!(parse_timestamp("2024-13-01").is_err());
        assert!(parse_timestamp("yesterday").is_err());
    }

    #[test]
    fn only_text_literals_are_coerced() {
        let untouched = Expr::Literal(Value::Int4(1));
        assert_eq!(
            coerce(untouched.clone(), DataType::Timestamp).unwrap(),
            untouched
        );
        assert_eq!(
            coerce(Expr::Literal(Value::Text("x".into())), DataType::Text).unwrap(),
            Expr::Literal(Value::Text("x".into()))
        );
    }

    #[test]
    fn the_same_aggregate_written_twice_is_collected_once() {
        let query = ferrite_sql::parse_statement(
            "SELECT count(*), count(*), max(age) FROM users GROUP BY dept",
        )
        .unwrap();
        let sql::Statement::Query(query) = query else {
            panic!("expected a query");
        };
        let sql::SetExpr::Select(select) = &query.body else {
            panic!("expected a select");
        };

        let mut collected = Vec::new();
        for item in &select.projection {
            if let sql::SelectItem::Expr { expr, .. } = item {
                collect_aggregates(expr, &mut collected).unwrap();
            }
        }
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn a_group_key_is_replaced_wherever_it_appears() {
        let key = Expr::column("dept");
        let rewritten = substitute_group_keys(
            Expr::eq(key.clone(), Expr::Literal(Value::Int4(1))),
            std::slice::from_ref(&key),
        );
        assert_eq!(
            rewritten,
            Expr::eq(Expr::Slot(0), Expr::Literal(Value::Int4(1)))
        );
        assert!(reject_ungrouped(&rewritten).is_ok());
        assert!(reject_ungrouped(&Expr::column("other")).is_err());
    }
}
