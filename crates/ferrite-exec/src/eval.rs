//! Expression evaluation with SQL three-valued logic.

use std::cmp::Ordering;

use ferrite_common::{DataType, FerriteError, Row, Value};
use ferrite_planner::BinaryOp;
use ferrite_planner::PhysExpr;

use crate::scalar;

/// Evaluate `expr` against `row`. Column references are positions, resolved
/// by the planner, so this never touches a schema.
pub fn eval(expr: &PhysExpr, row: &Row) -> Result<Value, FerriteError> {
    match expr {
        PhysExpr::Column(position) => row
            .values
            .get(*position)
            .cloned()
            .ok_or_else(|| FerriteError::Exec(format!("column {position} out of range"))),
        PhysExpr::Literal(value) => Ok(value.clone()),
        PhysExpr::IsNull(inner) => Ok(Value::Boolean(eval(inner, row)?.is_null())),
        PhysExpr::Not(inner) => Ok(match as_bool(&eval(inner, row)?)? {
            None => Value::Null,
            Some(b) => Value::Boolean(!b),
        }),
        PhysExpr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => {
            let (subject, pattern) = (eval(expr, row)?, eval(pattern, row)?);
            Ok(match (&subject, &pattern) {
                (Value::Null, _) | (_, Value::Null) => Value::Null,
                (Value::Text(subject), Value::Text(pattern)) => {
                    let matched = match case_insensitive {
                        true => like_matches(&subject.to_lowercase(), &pattern.to_lowercase()),
                        false => like_matches(subject, pattern),
                    };
                    Value::Boolean(matched != *negated)
                }
                _ => {
                    return Err(FerriteError::TypeMismatch {
                        expected: DataType::Text,
                        actual: subject
                            .data_type()
                            .filter(|t| *t != DataType::Text)
                            .or_else(|| pattern.data_type())
                            .expect("nulls were matched above"),
                    })
                }
            })
        }
        // `AND` and `OR` short-circuit: a `false` left operand settles an
        // `AND` whatever the right one would do, including raise. SQL
        // permits this, and real queries depend on it — a guard such as
        // `col LIKE 'pf:%' AND id = CAST(substr(col, 4) AS INTEGER)`
        // evaluates the cast only on rows the guard admitted.
        PhysExpr::Binary { left, op, right } => match op {
            op if op.is_arithmetic() => arithmetic(*op, eval(left, row)?, eval(right, row)?),
            BinaryOp::And => Ok(match as_bool(&eval(left, row)?)? {
                Some(false) => Value::Boolean(false),
                left => match (left, as_bool(&eval(right, row)?)?) {
                    (_, Some(false)) => Value::Boolean(false),
                    (Some(true), Some(true)) => Value::Boolean(true),
                    _ => Value::Null,
                },
            }),
            BinaryOp::Or => Ok(match as_bool(&eval(left, row)?)? {
                Some(true) => Value::Boolean(true),
                left => match (left, as_bool(&eval(right, row)?)?) {
                    (_, Some(true)) => Value::Boolean(true),
                    (Some(false), Some(false)) => Value::Boolean(false),
                    _ => Value::Null,
                },
            }),
            comparison => {
                let (l, r) = (eval(left, row)?, eval(right, row)?);
                Ok(match compare(&l, &r)? {
                    None => Value::Null,
                    Some(ordering) => Value::Boolean(matches(*comparison, ordering)),
                })
            }
        },
        // `Session::execute` replaces every one of these with the value
        // test its subplan produced, before any row is evaluated.
        PhysExpr::InSubquery { .. } => Err(FerriteError::Exec(
            "a subquery reached evaluation without having been run".to_string(),
        )),
        PhysExpr::Cast { expr, data_type } => scalar::cast(&eval(expr, row)?, *data_type),
        PhysExpr::Function { func, args } => {
            let args = args
                .iter()
                .map(|arg| eval(arg, row))
                .collect::<Result<Vec<_>, _>>()?;
            scalar::call(*func, &args)
        }
        // A `CASE` evaluates branches in order and stops at the first
        // match, so a later branch that would raise is never reached.
        PhysExpr::Case {
            operand,
            branches,
            else_result,
        } => {
            let operand = operand.as_ref().map(|o| eval(o, row)).transpose()?;
            for (when, then) in branches {
                let matched = match &operand {
                    Some(operand) => compare(operand, &eval(when, row)?)? == Some(Ordering::Equal),
                    None => matches!(eval(when, row)?, Value::Boolean(true)),
                };
                if matched {
                    return eval(then, row);
                }
            }
            match else_result {
                Some(else_result) => eval(else_result, row),
                None => Ok(Value::Null),
            }
        }
    }
}

/// Evaluate a `WHERE`-style predicate. `NULL` is not `true`, so the row is
/// rejected — the same rule Postgres applies.
pub fn eval_predicate(expr: &PhysExpr, row: &Row) -> Result<bool, FerriteError> {
    Ok(matches!(eval(expr, row)?, Value::Boolean(true)))
}

fn matches(op: BinaryOp, ordering: Ordering) -> bool {
    match op {
        BinaryOp::Eq => ordering == Ordering::Equal,
        BinaryOp::NotEq => ordering != Ordering::Equal,
        BinaryOp::Lt => ordering == Ordering::Less,
        BinaryOp::LtEq => ordering != Ordering::Greater,
        BinaryOp::Gt => ordering == Ordering::Greater,
        BinaryOp::GtEq => ordering != Ordering::Less,
        other => unreachable!("{other:?} is not a comparison"),
    }
}

fn as_bool(value: &Value) -> Result<Option<bool>, FerriteError> {
    match value {
        Value::Null => Ok(None),
        Value::Boolean(b) => Ok(Some(*b)),
        other => Err(FerriteError::TypeMismatch {
            expected: DataType::Boolean,
            actual: other
                .data_type()
                .expect("Null was matched above, so a type exists"),
        }),
    }
}

/// `+ - * / %` and `||`. Null propagates; the result type is the wider of
/// the two operands, matching the type the planner wrote into the output
/// schema.
fn arithmetic(op: BinaryOp, left: Value, right: Value) -> Result<Value, FerriteError> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let mismatch = |expected: DataType| {
        let actual = match left.data_type() {
            Some(actual) if actual != expected => actual,
            _ => right.data_type().expect("nulls were handled above"),
        };
        FerriteError::TypeMismatch { expected, actual }
    };
    if op == BinaryOp::Concat {
        return match (&left, &right) {
            (Value::Text(a), Value::Text(b)) => Ok(Value::Text(format!("{a}{b}"))),
            _ => Err(mismatch(DataType::Text)),
        };
    }
    match (&left, &right) {
        (Value::Float8(_), _) | (_, Value::Float8(_)) => {
            let (a, b) = (as_float(&left), as_float(&right));
            match (a, b) {
                (Some(a), Some(b)) => Ok(Value::Float8(float_op(op, a, b)?)),
                _ => Err(mismatch(DataType::Float8)),
            }
        }
        (Value::Int4(a), Value::Int4(b)) => {
            integer_op(op, i64::from(*a), i64::from(*b)).and_then(|v| {
                i32::try_from(v)
                    .map(Value::Int4)
                    .map_err(|_| FerriteError::Exec("integer out of range".into()))
            })
        }
        (Value::Int4(_) | Value::Int8(_), Value::Int4(_) | Value::Int8(_)) => {
            match (as_int(&left), as_int(&right)) {
                (Some(a), Some(b)) => Ok(Value::Int8(integer_op(op, a, b)?)),
                _ => Err(mismatch(DataType::Int8)),
            }
        }
        _ => Err(mismatch(DataType::Float8)),
    }
}

fn integer_op(op: BinaryOp, a: i64, b: i64) -> Result<i64, FerriteError> {
    let overflow = || FerriteError::Exec("integer out of range".to_string());
    match op {
        BinaryOp::Plus => a.checked_add(b).ok_or_else(overflow),
        BinaryOp::Minus => a.checked_sub(b).ok_or_else(overflow),
        BinaryOp::Multiply => a.checked_mul(b).ok_or_else(overflow),
        BinaryOp::Divide => a
            .checked_div(b)
            .ok_or_else(|| FerriteError::Exec("division by zero".to_string())),
        BinaryOp::Modulo => a
            .checked_rem(b)
            .ok_or_else(|| FerriteError::Exec("division by zero".to_string())),
        other => Err(FerriteError::Exec(format!("{other:?} is not arithmetic"))),
    }
}

fn float_op(op: BinaryOp, a: f64, b: f64) -> Result<f64, FerriteError> {
    match op {
        BinaryOp::Plus => Ok(a + b),
        BinaryOp::Minus => Ok(a - b),
        BinaryOp::Multiply => Ok(a * b),
        BinaryOp::Divide if b == 0.0 => Err(FerriteError::Exec("division by zero".to_string())),
        BinaryOp::Divide => Ok(a / b),
        BinaryOp::Modulo if b == 0.0 => Err(FerriteError::Exec("division by zero".to_string())),
        BinaryOp::Modulo => Ok(a % b),
        other => Err(FerriteError::Exec(format!("{other:?} is not arithmetic"))),
    }
}

fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Int4(v) => Some(i64::from(*v)),
        Value::Int8(v) => Some(*v),
        _ => None,
    }
}

/// SQL `LIKE`: `%` matches any run of characters, `_` exactly one, and a
/// backslash escapes either. Backtracking on `%` only, which is linear in
/// practice and needs no allocation beyond the two character vectors.
pub fn like_matches(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut t, mut p) = (0, 0);
    let (mut wildcard, mut resume) = (None, 0);

    while t < text.len() {
        if p < pattern.len() {
            match pattern[p] {
                '%' => {
                    wildcard = Some(p);
                    resume = t;
                    p += 1;
                    continue;
                }
                '_' => {
                    t += 1;
                    p += 1;
                    continue;
                }
                '\\' if p + 1 < pattern.len() && pattern[p + 1] == text[t] => {
                    t += 1;
                    p += 2;
                    continue;
                }
                c if c != '\\' && c == text[t] => {
                    t += 1;
                    p += 1;
                    continue;
                }
                _ => {}
            }
        }
        match wildcard {
            Some(position) => {
                p = position + 1;
                resume += 1;
                t = resume;
            }
            None => return false,
        }
    }
    pattern[p..].iter().all(|c| *c == '%')
}

/// `None` means "the comparison is `NULL`", i.e. at least one side was
/// `NULL`. Integer types compare exactly; mixing an integer with a
/// `Float8` falls back to floating point.
pub fn compare(left: &Value, right: &Value) -> Result<Option<Ordering>, FerriteError> {
    if left.is_null() || right.is_null() {
        return Ok(None);
    }
    let ordering = match (left, right) {
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
        (Value::Text(a), Value::Text(b)) | (Value::Json(a), Value::Json(b)) => a.cmp(b),
        (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        (Value::Int4(a), Value::Int4(b)) => a.cmp(b),
        (Value::Int8(a), Value::Int8(b)) => a.cmp(b),
        (Value::Int4(a), Value::Int8(b)) => i64::from(*a).cmp(b),
        (Value::Int8(a), Value::Int4(b)) => a.cmp(&i64::from(*b)),
        _ => match (as_float(left), as_float(right)) {
            (Some(a), Some(b)) => a.partial_cmp(&b).ok_or_else(|| {
                FerriteError::Exec("cannot order NaN against a number".to_string())
            })?,
            _ => {
                return Err(FerriteError::TypeMismatch {
                    expected: left.data_type().expect("nulls were handled above"),
                    actual: right.data_type().expect("nulls were handled above"),
                })
            }
        },
    };
    Ok(Some(ordering))
}

fn as_float(value: &Value) -> Option<f64> {
    match value {
        Value::Int4(v) => Some(f64::from(*v)),
        Value::Int8(v) => Some(*v as f64),
        Value::Float8(v) => Some(*v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Row {
        Row::new(vec![
            Value::Int8(10),
            Value::Text("ada".into()),
            Value::Null,
            Value::Boolean(true),
        ])
    }

    fn lit(v: Value) -> PhysExpr {
        PhysExpr::Literal(v)
    }

    #[test]
    fn columns_resolve_by_position() {
        assert_eq!(
            eval(&PhysExpr::Column(1), &row()).unwrap(),
            Value::Text("ada".into())
        );
    }

    #[test]
    fn an_out_of_range_column_is_an_execution_error() {
        assert!(matches!(
            eval(&PhysExpr::Column(99), &row()),
            Err(FerriteError::Exec(_))
        ));
    }

    #[test]
    fn comparisons_across_integer_widths_work() {
        let expr = PhysExpr::binary(PhysExpr::Column(0), BinaryOp::Eq, lit(Value::Int4(10)));
        assert!(eval_predicate(&expr, &row()).unwrap());
    }

    #[test]
    fn a_comparison_with_null_is_null_and_rejects_the_row() {
        let expr = PhysExpr::binary(PhysExpr::Column(2), BinaryOp::Eq, lit(Value::Int8(1)));
        assert_eq!(eval(&expr, &row()).unwrap(), Value::Null);
        assert!(!eval_predicate(&expr, &row()).unwrap());
    }

    #[test]
    fn is_null_is_not_null_aware() {
        assert_eq!(
            eval(&PhysExpr::IsNull(Box::new(PhysExpr::Column(2))), &row()).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval(&PhysExpr::IsNull(Box::new(PhysExpr::Column(0))), &row()).unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn false_and_null_is_false() {
        let expr = PhysExpr::binary(lit(Value::Boolean(false)), BinaryOp::And, lit(Value::Null));
        assert_eq!(eval(&expr, &row()).unwrap(), Value::Boolean(false));
    }

    #[test]
    fn true_and_null_is_null() {
        let expr = PhysExpr::binary(lit(Value::Boolean(true)), BinaryOp::And, lit(Value::Null));
        assert_eq!(eval(&expr, &row()).unwrap(), Value::Null);
    }

    #[test]
    fn true_or_null_is_true() {
        let expr = PhysExpr::binary(lit(Value::Boolean(true)), BinaryOp::Or, lit(Value::Null));
        assert_eq!(eval(&expr, &row()).unwrap(), Value::Boolean(true));
    }

    #[test]
    fn not_null_is_null() {
        assert_eq!(
            eval(&PhysExpr::Not(Box::new(lit(Value::Null))), &row()).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn incompatible_types_are_a_type_mismatch() {
        let expr = PhysExpr::binary(PhysExpr::Column(1), BinaryOp::Lt, lit(Value::Int8(1)));
        assert!(matches!(
            eval(&expr, &row()),
            Err(FerriteError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn a_non_boolean_in_a_logical_operator_is_a_type_mismatch() {
        let expr = PhysExpr::binary(
            PhysExpr::Column(0),
            BinaryOp::And,
            lit(Value::Boolean(true)),
        );
        assert!(matches!(
            eval(&expr, &row()),
            Err(FerriteError::TypeMismatch { .. })
        ));
    }
}
