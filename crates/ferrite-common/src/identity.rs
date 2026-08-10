use serde::{Deserialize, Serialize};

/// A caller's identity, derived from their public key — same shape as
/// SpacetimeDB's `Identity`. There is no separate row-level-security
/// policy language in Ferrite: procedures and triggers (see
/// `ferrite-proc`) receive the caller's `Identity` and decide access in
/// code, the same way a SpacetimeDB reducer reads `ctx.sender()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity(pub [u8; 32]);

impl Identity {
    pub const ANONYMOUS: Identity = Identity([0u8; 32]);
}

/// A named bundle of `Permission`s. Deliberately not a full Postgres-style
/// grant graph (no per-column grants, no `WITH GRANT OPTION` chains) —
/// roles are flat and explicit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Permission {
    Connect,
    CreateTable,
    Select,
    Insert,
    Update,
    Delete,
    Execute,
    Admin,
}
