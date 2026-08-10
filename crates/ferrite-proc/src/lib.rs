//! Owned by Agent 3, alongside `ferrite-planner` and `ferrite-exec`.
//! Triggers and stored procedures. This is also where row-level security
//! lives in Ferrite: there is no `CREATE POLICY ... USING (...)` DSL. A
//! procedure receives the caller's `ferrite_common::Identity` and decides
//! what to allow in code, the same way a SpacetimeDB reducer reads
//! `ctx.sender()`. See `docs/architecture.md` §Modèle de sécurité and this
//! crate's README.
//!
//! Three pieces:
//!
//! - [`ProcContext`] — what a procedure or trigger knows: the caller's
//!   identity, the roles it holds, the transaction, and (for triggers) the
//!   table, the event and the row's previous version.
//! - [`Trigger`] — `BEFORE INSERT`/`UPDATE`/`DELETE` body returning a
//!   [`ProcDecision`], or `Err(FerriteError::PermissionDenied(..))` to
//!   refuse the statement.
//! - [`Procedure`] — a named body invoked explicitly, gated by a declared
//!   [`ferrite_common::Permission`].
//!
//! ```
//! use ferrite_common::{Identity, Permission, Role, Value};
//! use ferrite_proc::{ProcRegistry, Procedure};
//!
//! let mut registry = ProcRegistry::new();
//! registry.register_procedure(Procedure::new("ping", Permission::Execute, |_ctx, _args| {
//!     Ok(Value::Text("pong".into()))
//! }));
//!
//! let caller = Identity([1u8; 32]);
//! let ctx = registry.context(caller, 1);
//! assert!(registry.call(&ctx, "ping", &[]).is_err()); // deny by default
//!
//! registry.grant_role(caller, Role { name: "runner".into(), permissions: vec![Permission::Execute] });
//! let ctx = registry.context(caller, 1);
//! assert_eq!(registry.call(&ctx, "ping", &[]).unwrap(), Value::Text("pong".into()));
//! ```

mod context;
mod procedure;
mod registry;
mod trigger;

pub use context::ProcContext;
pub use procedure::{Procedure, ProcedureFn};
pub use registry::ProcRegistry;
pub use trigger::{ProcDecision, Trigger, TriggerEvent, TriggerFn};
