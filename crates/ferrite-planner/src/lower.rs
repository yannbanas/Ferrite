//! `ferrite-sql` AST → the planner's IR.
//!
//! `ferrite-sql` parses a good deal more than `ferrite-exec` can run: joins,
//! aggregates, subqueries, set operations, `ORDER BY`. Everything this
//! module cannot project onto [`crate::expr`] becomes a
//! [`FerriteError::Plan`], never a panic and never a silently wrong plan.
//! The rejections are all in one place so the gap between "parses" and
//! "executes" stays legible.

use ferrite_common::{ColumnDefault, DataType, FerriteError, Schema, Value};
use ferrite_sql::ast as sql;

use crate::expr::{BinaryOp, Expr};

fn unsupported(what: &str) -> FerriteError {
    FerriteError::Plan(format!("{what} is not supported by the v1 planner"))
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

/// Lowers expressions, resolving `$n` placeholders against the bound
/// parameters and checking that qualified column references name a
/// relation actually in scope.
pub(crate) struct Lowerer<'a> {
    params: &'a [Value],
    /// Table name and alias of the single relation in scope, empty where
    /// column references are illegal (`INSERT ... VALUES`, `CALL` args).
    qualifiers: Vec<String>,
}

impl<'a> Lowerer<'a> {
    pub(crate) fn new(params: &'a [Value]) -> Self {
        Self {
            params,
            qualifiers: Vec::new(),
        }
    }

    pub(crate) fn with_qualifiers(mut self, qualifiers: Vec<String>) -> Self {
        self.qualifiers = qualifiers;
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
                    _ => Err(unsupported("unary minus on a non-literal")),
                },
            },

            sql::Expr::BinaryOp { left, op, right } => {
                let op = binary_op(*op)?;
                Ok(Expr::binary(self.expr(left)?, op, self.expr(right)?))
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
                let mut disjunction: Option<Expr> = None;
                for item in list {
                    let test = Expr::eq(subject.clone(), self.expr(item)?);
                    disjunction = Some(match disjunction {
                        None => test,
                        Some(acc) => Expr::binary(acc, BinaryOp::Or, test),
                    });
                }
                let any = disjunction.expect("the list was checked non-empty");
                Ok(if *negated {
                    Expr::Not(Box::new(any))
                } else {
                    any
                })
            }

            sql::Expr::Cast { .. } => Err(unsupported("CAST")),
            sql::Expr::Like { .. } => Err(unsupported("LIKE")),
            sql::Expr::Case { .. } => Err(unsupported("CASE")),
            sql::Expr::Function(call) => Err(unsupported(&format!("the function {}", call.name))),
            sql::Expr::Subquery(_) | sql::Expr::InSubquery { .. } | sql::Expr::Exists { .. } => {
                Err(unsupported("subqueries"))
            }
        }
    }

    fn column(&self, name: &sql::ObjectName) -> Result<Expr, FerriteError> {
        if let Some(qualifier) = name.qualifier() {
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
        }
        Ok(Expr::Column(name.base().to_string()))
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

fn binary_op(op: sql::BinaryOp) -> Result<BinaryOp, FerriteError> {
    Ok(match op {
        sql::BinaryOp::Or => BinaryOp::Or,
        sql::BinaryOp::And => BinaryOp::And,
        sql::BinaryOp::Eq => BinaryOp::Eq,
        sql::BinaryOp::NotEq => BinaryOp::NotEq,
        sql::BinaryOp::Lt => BinaryOp::Lt,
        sql::BinaryOp::LtEq => BinaryOp::LtEq,
        sql::BinaryOp::Gt => BinaryOp::Gt,
        sql::BinaryOp::GtEq => BinaryOp::GtEq,
        sql::BinaryOp::Plus
        | sql::BinaryOp::Minus
        | sql::BinaryOp::Multiply
        | sql::BinaryOp::Divide
        | sql::BinaryOp::Modulo
        | sql::BinaryOp::Concat => {
            return Err(unsupported(
                "arithmetic and string operators in expressions",
            ))
        }
    })
}

/// The single relation a statement reads, plus the names it may be
/// qualified by. Multi-table `FROM` clauses, joins and derived tables are
/// all rejected here: `ferrite-exec` has no join node to run them with.
pub(crate) fn single_relation(
    from: &[sql::TableWithJoins],
) -> Result<(&sql::ObjectName, Vec<String>), FerriteError> {
    let [only] = from else {
        return Err(unsupported("more than one relation in FROM"));
    };
    if !only.joins.is_empty() {
        return Err(unsupported("JOIN"));
    }
    match &only.relation {
        sql::TableFactor::Table { name, alias } => {
            let mut qualifiers = vec![name.base().to_string()];
            if let Some(alias) = alias {
                qualifiers.push(alias.clone());
            }
            Ok((name, qualifiers))
        }
        sql::TableFactor::Derived { .. } => Err(unsupported("a subquery in FROM")),
    }
}

/// Unwrap a [`sql::Query`] down to the one `SELECT` the executor can run,
/// returning it with the `LIMIT` that applies to it.
pub(crate) fn single_select(
    query: &sql::Query,
) -> Result<(&sql::Select, Option<u64>), FerriteError> {
    if !query.with.is_empty() {
        return Err(unsupported("WITH (common table expressions)"));
    }
    if !query.order_by.is_empty() {
        return Err(unsupported("ORDER BY"));
    }
    if query.offset.is_some() {
        return Err(unsupported("OFFSET"));
    }
    let select = match &query.body {
        sql::SetExpr::Select(select) => select.as_ref(),
        // A parenthesised query keeps its own `SELECT`, but the outer
        // `LIMIT` is the one that applies.
        sql::SetExpr::Query(inner) => {
            let (select, _) = single_select(inner)?;
            return Ok((select, limit(query)?));
        }
        sql::SetExpr::SetOp { .. } => {
            return Err(unsupported("UNION/INTERSECT/EXCEPT"));
        }
    };
    if select.distinct {
        return Err(unsupported("SELECT DISTINCT"));
    }
    if !select.group_by.is_empty() {
        return Err(unsupported("GROUP BY"));
    }
    if select.having.is_some() {
        return Err(unsupported("HAVING"));
    }
    Ok((select, limit(query)?))
}

fn limit(query: &sql::Query) -> Result<Option<u64>, FerriteError> {
    let Some(expr) = &query.limit else {
        return Ok(None);
    };
    match expr {
        sql::Expr::Literal(sql::Literal::Int(n)) if *n >= 0 => Ok(Some(*n as u64)),
        _ => Err(FerriteError::Plan(
            "LIMIT must be a non-negative integer literal".into(),
        )),
    }
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
}
