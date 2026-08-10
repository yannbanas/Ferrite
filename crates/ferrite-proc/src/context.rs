//! What a procedure or trigger is handed when it runs.

use ferrite_common::{FerriteError, Identity, Permission, Role, Row, TableId, TxnId};

use crate::trigger::TriggerEvent;

/// Everything a procedure body may know about its invocation.
///
/// The caller's [`Identity`] is the anchor of Ferrite's access control:
/// there is no declarative policy language, so a procedure that wants to
/// restrict access reads `ctx.sender()` and the caller's roles, exactly as
/// a SpacetimeDB reducer reads `ctx.sender()`.
///
/// Constructed by the executor, never by procedure code — the borrowed
/// role slice makes it impossible for a procedure to grant itself
/// anything.
#[derive(Debug, Clone)]
pub struct ProcContext<'a> {
    identity: Identity,
    roles: &'a [Role],
    txn: TxnId,
    table: Option<TableId>,
    table_name: Option<&'a str>,
    event: Option<TriggerEvent>,
    old_row: Option<&'a Row>,
}

impl<'a> ProcContext<'a> {
    pub fn new(identity: Identity, roles: &'a [Role], txn: TxnId) -> Self {
        Self {
            identity,
            roles,
            txn,
            table: None,
            table_name: None,
            event: None,
            old_row: None,
        }
    }

    pub fn with_table(mut self, table: TableId, name: &'a str) -> Self {
        self.table = Some(table);
        self.table_name = Some(name);
        self
    }

    pub fn with_event(mut self, event: TriggerEvent) -> Self {
        self.event = Some(event);
        self
    }

    /// Attach the pre-image of the row being changed. Set for `UPDATE` and
    /// `DELETE` triggers, absent for `INSERT`.
    pub fn with_old_row(mut self, row: &'a Row) -> Self {
        self.old_row = Some(row);
        self
    }

    /// Identity of whoever issued the statement.
    pub fn sender(&self) -> Identity {
        self.identity
    }

    pub fn roles(&self) -> &[Role] {
        self.roles
    }

    pub fn txn(&self) -> TxnId {
        self.txn
    }

    pub fn table(&self) -> Option<TableId> {
        self.table
    }

    pub fn table_name(&self) -> Option<&str> {
        self.table_name
    }

    pub fn event(&self) -> Option<TriggerEvent> {
        self.event
    }

    pub fn old_row(&self) -> Option<&Row> {
        self.old_row
    }

    /// Deny by default: a permission is held only if some role grants it
    /// explicitly, or grants [`Permission::Admin`], which implies all of
    /// them. An identity with no roles holds nothing.
    pub fn has_permission(&self, permission: Permission) -> bool {
        self.roles.iter().any(|role| {
            role.permissions
                .iter()
                .any(|held| *held == permission || *held == Permission::Admin)
        })
    }

    /// [`Self::has_permission`] as a `Result`, logging the refusal.
    /// `docs/architecture.md` asks for structured logging on every
    /// permission denial, and this is the single place they funnel through.
    pub fn require(&self, permission: Permission) -> Result<(), FerriteError> {
        if self.has_permission(permission) {
            return Ok(());
        }
        let Identity(bytes) = self.identity;
        tracing::warn!(
            identity = %hex(&bytes),
            ?permission,
            table = self.table_name.unwrap_or("-"),
            "permission denied"
        );
        Err(FerriteError::PermissionDenied(format!(
            "identity {} lacks {:?}",
            hex(&bytes),
            permission
        )))
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(name: &str, permissions: Vec<Permission>) -> Role {
        Role {
            name: name.into(),
            permissions,
        }
    }

    #[test]
    fn an_identity_with_no_roles_holds_nothing() {
        let ctx = ProcContext::new(Identity::ANONYMOUS, &[], 1);
        assert!(!ctx.has_permission(Permission::Select));
        assert!(matches!(
            ctx.require(Permission::Select),
            Err(FerriteError::PermissionDenied(_))
        ));
    }

    #[test]
    fn a_granted_permission_passes() {
        let roles = vec![role(
            "reader",
            vec![Permission::Connect, Permission::Select],
        )];
        let ctx = ProcContext::new(Identity([1u8; 32]), &roles, 1);
        assert!(ctx.require(Permission::Select).is_ok());
        assert!(ctx.require(Permission::Insert).is_err());
    }

    #[test]
    fn admin_implies_every_permission() {
        let roles = vec![role("superuser", vec![Permission::Admin])];
        let ctx = ProcContext::new(Identity([2u8; 32]), &roles, 1);
        for permission in [
            Permission::Connect,
            Permission::CreateTable,
            Permission::Select,
            Permission::Insert,
            Permission::Update,
            Permission::Delete,
            Permission::Execute,
        ] {
            assert!(ctx.require(permission).is_ok(), "{permission:?}");
        }
    }

    #[test]
    fn permissions_are_the_union_of_every_role() {
        let roles = vec![
            role("reader", vec![Permission::Select]),
            role("writer", vec![Permission::Insert]),
        ];
        let ctx = ProcContext::new(Identity([3u8; 32]), &roles, 1);
        assert!(ctx.has_permission(Permission::Select));
        assert!(ctx.has_permission(Permission::Insert));
        assert!(!ctx.has_permission(Permission::Delete));
    }

    #[test]
    fn the_denial_message_names_the_identity_and_the_permission() {
        let ctx = ProcContext::new(Identity([0xab; 32]), &[], 7);
        let Err(FerriteError::PermissionDenied(message)) = ctx.require(Permission::Delete) else {
            panic!("expected a denial");
        };
        assert!(message.contains("abab"));
        assert!(message.contains("Delete"));
    }
}
