//! Owned by Agent 3, alongside `ferrite-planner` and `ferrite-exec`.
//! Triggers and stored procedures. This is also where row-level security
//! lives in Ferrite: there is no separate `CREATE POLICY ... USING (...)`
//! DSL. A procedure receives the caller's `ferrite_common::Identity` and
//! decides what to allow in code, the same way a SpacetimeDB reducer
//! reads `ctx.sender()`. See workspace README §Security model.

pub struct NotYetImplemented;
