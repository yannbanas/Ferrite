//! The names an expression may refer to.
//!
//! With a single relation per statement a [`Schema`] was enough: a column
//! reference was a name and the schema had one. A join produces a row made
//! of two relations' columns, where the same name can appear twice, so
//! binding needs to know which relation every position came from — that is
//! what a [`Scope`] adds on top of a schema.

use ferrite_common::{ColumnDef, FerriteError, Schema};

use crate::expr::ColumnRef;

/// One column of a row, with the relation names it answers to. A column
/// carries both its table name and the table's alias when there is one, so
/// `users.id` and `u.id` resolve to the same position.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeColumn {
    pub qualifiers: Vec<String>,
    pub column: ColumnDef,
}

/// The columns of one row, in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scope {
    pub columns: Vec<ScopeColumn>,
}

impl Scope {
    /// A scope where column references are illegal (`INSERT ... VALUES`,
    /// `CALL` arguments); resolving anything against it is the correct
    /// `ColumnNotFound`.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A scope over one relation, reachable through any of `qualifiers`.
    pub fn for_relation(schema: &Schema, qualifiers: &[String]) -> Self {
        Self {
            columns: schema
                .columns
                .iter()
                .map(|column| ScopeColumn {
                    qualifiers: qualifiers.to_vec(),
                    column: column.clone(),
                })
                .collect(),
        }
    }

    /// A scope over an unqualified row, i.e. the output of a projection or
    /// an aggregate, where no relation name is in play any more.
    pub fn anonymous(schema: &Schema) -> Self {
        Self::for_relation(schema, &[])
    }

    /// The two sides of a join, left columns first.
    pub fn concat(mut left: Scope, right: Scope) -> Self {
        left.columns.extend(right.columns);
        left
    }

    /// Every column becomes nullable, as the null-extended side of an
    /// outer join is.
    pub fn nullable(mut self) -> Self {
        for entry in &mut self.columns {
            entry.column.nullable = true;
        }
        self
    }

    pub fn len(&self) -> usize {
        self.columns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Positions a reference could bind to. More than one means the
    /// reference is ambiguous, none that it is unknown.
    fn candidates(&self, reference: &ColumnRef) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.column.name == reference.name
                    && match &reference.qualifier {
                        None => true,
                        Some(qualifier) => entry.qualifiers.contains(qualifier),
                    }
            })
            .map(|(position, _)| position)
            .collect()
    }

    /// `true` when the reference names exactly one column here. Used by the
    /// rule engine to decide which side of a join a predicate belongs to,
    /// where an ambiguous name must not count as belonging to either.
    pub fn can_resolve(&self, reference: &ColumnRef) -> bool {
        self.candidates(reference).len() == 1
    }

    pub fn resolve(&self, reference: &ColumnRef) -> Result<usize, FerriteError> {
        match self.candidates(reference).as_slice() {
            [position] => Ok(*position),
            [] => Err(FerriteError::ColumnNotFound(reference.to_string())),
            _ => Err(FerriteError::Plan(format!(
                "column reference {reference} is ambiguous"
            ))),
        }
    }

    pub fn column(&self, position: usize) -> Result<&ColumnDef, FerriteError> {
        self.columns
            .get(position)
            .map(|entry| &entry.column)
            .ok_or_else(|| FerriteError::Plan(format!("column {position} out of range")))
    }

    /// The positions of every column reachable through `qualifier`, in
    /// order — what a `t.*` select item expands to.
    pub fn positions_for(&self, qualifier: &str) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.qualifiers.iter().any(|q| q == qualifier))
            .map(|(position, _)| position)
            .collect()
    }

    pub fn schema(&self) -> Schema {
        Schema {
            columns: self.columns.iter().map(|c| c.column.clone()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_common::DataType;

    fn schema(names: &[&str]) -> Schema {
        Schema {
            columns: names
                .iter()
                .map(|name| ColumnDef {
                    name: (*name).to_string(),
                    data_type: DataType::Int8,
                    nullable: false,
                })
                .collect(),
        }
    }

    fn joined() -> Scope {
        Scope::concat(
            Scope::for_relation(&schema(&["id", "name"]), &["users".into(), "u".into()]),
            Scope::for_relation(&schema(&["id", "body"]), &["posts".into()]),
        )
    }

    #[test]
    fn an_alias_resolves_to_the_same_position_as_the_table_name() {
        let scope = joined();
        assert_eq!(
            scope.resolve(&ColumnRef::qualified("u", "id")).unwrap(),
            scope.resolve(&ColumnRef::qualified("users", "id")).unwrap()
        );
    }

    #[test]
    fn a_name_present_on_both_sides_is_ambiguous_unqualified() {
        let scope = joined();
        assert!(matches!(
            scope.resolve(&ColumnRef::new("id")),
            Err(FerriteError::Plan(_))
        ));
        assert_eq!(scope.resolve(&ColumnRef::new("body")).unwrap(), 3);
        assert!(!scope.can_resolve(&ColumnRef::new("id")));
    }

    #[test]
    fn an_unknown_name_is_a_column_not_found() {
        assert!(matches!(
            joined().resolve(&ColumnRef::new("nope")),
            Err(FerriteError::ColumnNotFound(_))
        ));
    }

    #[test]
    fn the_null_extended_side_of_a_join_makes_its_columns_nullable() {
        let scope = Scope::for_relation(&schema(&["id"]), &["posts".into()]).nullable();
        assert!(scope.column(0).unwrap().nullable);
    }

    #[test]
    fn a_qualified_wildcard_expands_to_one_relation() {
        assert_eq!(joined().positions_for("posts"), vec![2, 3]);
    }
}
