//! Stored procedures: Rust closures registered under a name and invoked
//! explicitly (`CALL name(args)`).
//!
//! There is no procedural *language* in Ferrite v1 — `docs/architecture.md`
//! cuts the extension system, so no PL/pgSQL and no PL/Python. A procedure
//! is native code registered at startup by whoever embeds the engine.

use std::fmt;
use std::sync::Arc;

use ferrite_common::{FerriteError, Permission, Value};

use crate::context::ProcContext;

/// A procedure body: arguments in, one value out.
pub type ProcedureFn =
    dyn Fn(&ProcContext, &[Value]) -> Result<Value, FerriteError> + Send + Sync + 'static;

/// A registered procedure and the permission needed to run it.
#[derive(Clone)]
pub struct Procedure {
    pub name: String,
    /// Checked before the body runs. Defaults to [`Permission::Execute`];
    /// a procedure that touches data should ask for something narrower or
    /// re-check inside its body.
    pub required_permission: Permission,
    pub(crate) body: Arc<ProcedureFn>,
}

impl Procedure {
    pub fn new<F>(name: impl Into<String>, required_permission: Permission, body: F) -> Self
    where
        F: Fn(&ProcContext, &[Value]) -> Result<Value, FerriteError> + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            required_permission,
            body: Arc::new(body),
        }
    }

    /// Run the body. Callers should go through
    /// [`ProcRegistry::call`](crate::ProcRegistry::call), which enforces
    /// `required_permission` first.
    pub fn call(&self, ctx: &ProcContext, args: &[Value]) -> Result<Value, FerriteError> {
        (self.body)(ctx, args)
    }
}

impl fmt::Debug for Procedure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Procedure")
            .field("name", &self.name)
            .field("required_permission", &self.required_permission)
            .finish_non_exhaustive()
    }
}
