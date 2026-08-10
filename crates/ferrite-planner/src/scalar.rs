//! The scalar functions Ferrite evaluates per row.
//!
//! The set is deliberately closed and deliberately small: it is exactly
//! what an audit of the PawChat sources showed the application calls (see
//! `docs/pawchat-sql-audit.md`), plus `upper`, which falls out of the same
//! case folding `lower` and `COLLATE NOCASE` need. A function outside this
//! list is a plan error naming it, never a silent null.
//!
//! Semantics follow SQLite rather than PostgreSQL wherever the two differ,
//! because the queries being served were written against SQLite. The
//! divergences that remain are listed in the audit document.

use ferrite_common::{DataType, FerriteError};

/// A scalar function the executor knows how to evaluate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    /// First non-null argument, or null. Variadic, at least one argument.
    Coalesce,
    /// ASCII-and-Unicode lowercasing.
    Lower,
    /// ASCII-and-Unicode uppercasing.
    Upper,
    /// The case fold `COLLATE NOCASE` lowers to. Unlike [`ScalarFunc::Lower`]
    /// it passes non-text values through untouched, matching a collation's
    /// no-op behaviour on a non-text operand.
    Nocase,
    /// `substr(s, start [, len])`, 1-based, SQLite's negative-start rule
    /// included.
    Substr,
    /// Uppercase hexadecimal rendering of the argument's bytes.
    Hex,
    /// `randomblob(n)` — n random bytes.
    Randomblob,
    /// `datetime(when [, modifier…])` as `YYYY-MM-DD HH:MM:SS`, UTC.
    Datetime,
    /// `date(when [, modifier…])` as `YYYY-MM-DD`, UTC.
    Date,
}

impl ScalarFunc {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "coalesce" => ScalarFunc::Coalesce,
            "lower" => ScalarFunc::Lower,
            "upper" => ScalarFunc::Upper,
            "nocase" => ScalarFunc::Nocase,
            "substr" | "substring" => ScalarFunc::Substr,
            "hex" => ScalarFunc::Hex,
            "randomblob" => ScalarFunc::Randomblob,
            "datetime" => ScalarFunc::Datetime,
            "date" => ScalarFunc::Date,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            ScalarFunc::Coalesce => "coalesce",
            ScalarFunc::Lower => "lower",
            ScalarFunc::Upper => "upper",
            ScalarFunc::Nocase => "nocase",
            ScalarFunc::Substr => "substr",
            ScalarFunc::Hex => "hex",
            ScalarFunc::Randomblob => "randomblob",
            ScalarFunc::Datetime => "datetime",
            ScalarFunc::Date => "date",
        }
    }

    /// The inclusive argument count range, `None` upper bound for variadic.
    fn arity(self) -> (usize, Option<usize>) {
        match self {
            ScalarFunc::Coalesce => (1, None),
            ScalarFunc::Datetime | ScalarFunc::Date => (1, None),
            ScalarFunc::Substr => (2, Some(3)),
            _ => (1, Some(1)),
        }
    }

    /// Reject a call whose argument count this function cannot take, at
    /// plan time rather than on the first row.
    pub fn check_arity(self, given: usize) -> Result<(), FerriteError> {
        let (min, max) = self.arity();
        if given < min || max.is_some_and(|max| given > max) {
            return Err(FerriteError::Plan(format!(
                "{}() takes {}, not {given}",
                self.name(),
                match (min, max) {
                    (min, Some(max)) if min == max => format!("{min} argument(s)"),
                    (min, Some(max)) => format!("{min} to {max} arguments"),
                    (min, None) => format!("at least {min} argument(s)"),
                }
            )));
        }
        Ok(())
    }

    /// The type this function returns. `Coalesce` and `Nocase` take the
    /// type of their argument, so they are resolved by the caller, which
    /// is the only place that can infer it.
    pub fn result_type(self, first_arg: DataType) -> DataType {
        match self {
            ScalarFunc::Coalesce | ScalarFunc::Nocase => first_arg,
            ScalarFunc::Lower
            | ScalarFunc::Upper
            | ScalarFunc::Substr
            | ScalarFunc::Hex
            | ScalarFunc::Randomblob
            | ScalarFunc::Datetime
            | ScalarFunc::Date => DataType::Text,
        }
    }

    /// Whether the result can be null for non-null inputs. Only
    /// `coalesce` narrows nullability; everything else propagates it.
    pub fn is_null_preserving(self) -> bool {
        !matches!(self, ScalarFunc::Coalesce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arity_is_checked_at_plan_time() {
        assert!(ScalarFunc::Lower.check_arity(1).is_ok());
        assert!(ScalarFunc::Lower.check_arity(2).is_err());
        assert!(ScalarFunc::Substr.check_arity(2).is_ok());
        assert!(ScalarFunc::Substr.check_arity(3).is_ok());
        assert!(ScalarFunc::Substr.check_arity(4).is_err());
        assert!(ScalarFunc::Coalesce.check_arity(7).is_ok());
        assert!(ScalarFunc::Coalesce.check_arity(0).is_err());
    }

    #[test]
    fn every_name_round_trips() {
        for func in [
            ScalarFunc::Coalesce,
            ScalarFunc::Lower,
            ScalarFunc::Upper,
            ScalarFunc::Nocase,
            ScalarFunc::Substr,
            ScalarFunc::Hex,
            ScalarFunc::Randomblob,
            ScalarFunc::Datetime,
            ScalarFunc::Date,
        ] {
            assert_eq!(ScalarFunc::parse(func.name()), Some(func));
        }
        assert_eq!(ScalarFunc::parse("strftime"), None);
    }
}
