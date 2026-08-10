//! Triggers. v1 covers `BEFORE INSERT`/`BEFORE UPDATE`/`BEFORE DELETE`
//! only — `AFTER` triggers need the executor to defer work to commit time,
//! which the single-threaded v1 executor does not do yet.

use std::fmt;
use std::sync::Arc;

use ferrite_common::{FerriteError, Row, TableId};

use crate::context::ProcContext;

/// Which row operation is firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
}

/// What a `BEFORE` trigger decides about the row it was handed.
///
/// Refusing an operation outright is *not* a variant: a trigger refuses by
/// returning `Err(FerriteError::PermissionDenied(..))`, which aborts the
/// whole statement. `Skip` is the softer option — the row is silently left
/// alone and the statement continues.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcDecision {
    /// Proceed with the row unchanged.
    Allow,
    /// Proceed with this row instead (audit columns, normalization,
    /// masking a value the caller may not write).
    Replace(Row),
    /// Leave this row alone and move to the next one.
    Skip,
}

/// A trigger body.
///
/// `row` is the subject of the operation: the new row for `INSERT` and
/// `UPDATE`, the row being removed for `DELETE`. The previous version, for
/// `UPDATE`/`DELETE`, is on the context as
/// [`ProcContext::old_row`](crate::ProcContext::old_row).
pub type TriggerFn =
    dyn Fn(&ProcContext, &Row) -> Result<ProcDecision, FerriteError> + Send + Sync + 'static;

/// A registered trigger: a name (for diagnostics) plus its body.
#[derive(Clone)]
pub struct Trigger {
    pub name: String,
    pub table: TableId,
    pub event: TriggerEvent,
    pub(crate) body: Arc<TriggerFn>,
}

impl Trigger {
    pub fn new<F>(name: impl Into<String>, table: TableId, event: TriggerEvent, body: F) -> Self
    where
        F: Fn(&ProcContext, &Row) -> Result<ProcDecision, FerriteError> + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            table,
            event,
            body: Arc::new(body),
        }
    }

    pub fn call(&self, ctx: &ProcContext, row: &Row) -> Result<ProcDecision, FerriteError> {
        (self.body)(ctx, row)
    }
}

impl fmt::Debug for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Trigger")
            .field("name", &self.name)
            .field("table", &self.table)
            .field("event", &self.event)
            .finish_non_exhaustive()
    }
}
