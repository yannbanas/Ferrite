//! Grouping and the five aggregate functions.
//!
//! One hash pass over the input, keeping groups in first-appearance order
//! so the result is deterministic without a sort. `docs/architecture.md`
//! cuts parallel execution for v1, so there is no partial-aggregate merge
//! step to design around.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use ferrite_common::{DataType, FerriteError, Row, Value};
use ferrite_planner::{AggregateFunc, PhysAggregate, PhysExpr};

use crate::eval::{compare, eval};
use crate::executor::{value_key, Tuple};

/// One output row per distinct group-key tuple: the key columns in order,
/// then one column per aggregate.
pub(crate) fn run(
    input: &[Tuple],
    group_by: &[PhysExpr],
    aggregates: &[PhysAggregate],
) -> Result<Vec<Tuple>, FerriteError> {
    let mut groups: Vec<(Vec<Value>, Vec<Accumulator>)> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();

    for tuple in input {
        let key = group_by
            .iter()
            .map(|expr| eval(expr, &tuple.row))
            .collect::<Result<Vec<_>, _>>()?;
        let position = match positions.entry(value_key(&key)) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let position = groups.len();
                entry.insert(position);
                groups.push((key, aggregates.iter().map(Accumulator::new).collect()));
                position
            }
        };
        for (accumulator, call) in groups[position].1.iter_mut().zip(aggregates) {
            let value = call.arg.as_ref().map(|e| eval(e, &tuple.row)).transpose()?;
            accumulator.push(value)?;
        }
    }

    // `SELECT count(*) FROM empty_table` is one row holding zero, not zero
    // rows — but `GROUP BY` over no rows really is empty.
    if groups.is_empty() && group_by.is_empty() {
        groups.push((
            Vec::new(),
            aggregates.iter().map(Accumulator::new).collect(),
        ));
    }

    groups
        .into_iter()
        .map(|(key, accumulators)| {
            let mut values = key;
            for accumulator in accumulators {
                values.push(accumulator.finish());
            }
            Ok(Tuple {
                rid: None,
                row: Row::new(values),
            })
        })
        .collect()
}

struct Accumulator {
    func: AggregateFunc,
    /// The values already counted, for a `DISTINCT` aggregate.
    seen: Option<HashSet<String>>,
    count: i64,
    integral: i64,
    fractional: f64,
    is_float: bool,
    any: bool,
    extreme: Option<Value>,
}

impl Accumulator {
    fn new(call: &PhysAggregate) -> Self {
        Self {
            func: call.func,
            seen: call.distinct.then(HashSet::new),
            count: 0,
            integral: 0,
            fractional: 0.0,
            is_float: false,
            any: false,
            extreme: None,
        }
    }

    /// `None` is the `*` of `count(*)`: it counts the row itself, so it
    /// bypasses both the null check and `DISTINCT`.
    fn push(&mut self, value: Option<Value>) -> Result<(), FerriteError> {
        let Some(value) = value else {
            self.count += 1;
            return Ok(());
        };
        if value.is_null() {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen {
            if !seen.insert(format!("{value:?}")) {
                return Ok(());
            }
        }
        self.count += 1;
        self.any = true;

        match self.func {
            AggregateFunc::Count => {}
            AggregateFunc::Sum | AggregateFunc::Avg => match value {
                Value::Int4(v) => self.add(i64::from(v))?,
                Value::Int8(v) => self.add(v)?,
                Value::Float8(v) => {
                    self.is_float = true;
                    self.fractional += v;
                }
                other => {
                    return Err(FerriteError::TypeMismatch {
                        expected: DataType::Float8,
                        actual: other.data_type().expect("nulls were handled above"),
                    })
                }
            },
            AggregateFunc::Min | AggregateFunc::Max => {
                let replace = match &self.extreme {
                    None => true,
                    Some(current) => {
                        let ordering = compare(&value, current)?.expect("neither side is null");
                        match self.func {
                            AggregateFunc::Min => ordering.is_lt(),
                            _ => ordering.is_gt(),
                        }
                    }
                };
                if replace {
                    self.extreme = Some(value);
                }
            }
        }
        Ok(())
    }

    fn add(&mut self, value: i64) -> Result<(), FerriteError> {
        self.integral = self
            .integral
            .checked_add(value)
            .ok_or_else(|| FerriteError::Exec("sum() overflowed a BIGINT".to_string()))?;
        Ok(())
    }

    fn total(&self) -> f64 {
        self.integral as f64 + self.fractional
    }

    fn finish(self) -> Value {
        match self.func {
            AggregateFunc::Count => Value::Int8(self.count),
            _ if !self.any => Value::Null,
            AggregateFunc::Sum if self.is_float => Value::Float8(self.total()),
            AggregateFunc::Sum => Value::Int8(self.integral),
            AggregateFunc::Avg => Value::Float8(self.total() / self.count as f64),
            AggregateFunc::Min | AggregateFunc::Max => self.extreme.unwrap_or(Value::Null),
        }
    }
}
