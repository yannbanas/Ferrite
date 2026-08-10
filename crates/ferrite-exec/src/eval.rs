//! Expression evaluation with SQL three-valued logic.

use std::cmp::Ordering;

use ferrite_common::{FerriteError, Row, Value};
use ferrite_planner::BinaryOp;
use ferrite_planner::PhysExpr;

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
        PhysExpr::Binary { left, op, right } => match op {
            BinaryOp::And => {
                let (l, r) = (as_bool(&eval(left, row)?)?, as_bool(&eval(right, row)?)?);
                Ok(match (l, r) {
                    (Some(false), _) | (_, Some(false)) => Value::Boolean(false),
                    (Some(true), Some(true)) => Value::Boolean(true),
                    _ => Value::Null,
                })
            }
            BinaryOp::Or => {
                let (l, r) = (as_bool(&eval(left, row)?)?, as_bool(&eval(right, row)?)?);
                Ok(match (l, r) {
                    (Some(true), _) | (_, Some(true)) => Value::Boolean(true),
                    (Some(false), Some(false)) => Value::Boolean(false),
                    _ => Value::Null,
                })
            }
            comparison => {
                let (l, r) = (eval(left, row)?, eval(right, row)?);
                Ok(match compare(&l, &r)? {
                    None => Value::Null,
                    Some(ordering) => Value::Boolean(matches(*comparison, ordering)),
                })
            }
        },
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
        BinaryOp::And | BinaryOp::Or => unreachable!("handled before comparison"),
    }
}

fn as_bool(value: &Value) -> Result<Option<bool>, FerriteError> {
    match value {
        Value::Null => Ok(None),
        Value::Boolean(b) => Ok(Some(*b)),
        other => Err(FerriteError::TypeMismatch {
            expected: ferrite_common::DataType::Boolean,
            actual: other
                .data_type()
                .expect("Null was matched above, so a type exists"),
        }),
    }
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
