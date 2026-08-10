//! Registry of triggers, procedures and role grants.

use std::collections::HashMap;

use ferrite_common::{FerriteError, Identity, Permission, Role, Row, TableId, TxnId, Value};

use crate::context::ProcContext;
use crate::procedure::Procedure;
use crate::trigger::{ProcDecision, Trigger, TriggerEvent};

/// Everything the executor needs to enforce access control and run user
/// code. Built once at startup and shared read-only across sessions.
#[derive(Debug, Default)]
pub struct ProcRegistry {
    grants: HashMap<Identity, Vec<Role>>,
    triggers: HashMap<(TableId, TriggerEvent), Vec<Trigger>>,
    procedures: HashMap<String, Procedure>,
}

impl ProcRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Give `identity` a role. Roles accumulate; permissions are the union
    /// across all of them.
    pub fn grant_role(&mut self, identity: Identity, role: Role) {
        self.grants.entry(identity).or_default().push(role);
    }

    /// Roles held by `identity` — an empty slice for anyone unknown, which
    /// is what makes the model deny-by-default.
    pub fn roles_for(&self, identity: Identity) -> &[Role] {
        self.grants.get(&identity).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Build a context for `identity` under `txn`. The borrowed role slice
    /// comes from this registry, so procedure code cannot fabricate one.
    pub fn context(&self, identity: Identity, txn: TxnId) -> ProcContext<'_> {
        ProcContext::new(identity, self.roles_for(identity), txn)
    }

    /// Check a statement-level permission for `identity`.
    pub fn authorize(
        &self,
        identity: Identity,
        txn: TxnId,
        permission: Permission,
    ) -> Result<(), FerriteError> {
        self.context(identity, txn).require(permission)
    }

    pub fn register_trigger(&mut self, trigger: Trigger) {
        self.triggers
            .entry((trigger.table, trigger.event))
            .or_default()
            .push(trigger);
    }

    /// Convenience wrapper over [`Trigger::new`] +
    /// [`Self::register_trigger`].
    pub fn register_before<F>(
        &mut self,
        name: impl Into<String>,
        table: TableId,
        event: TriggerEvent,
        body: F,
    ) where
        F: Fn(&ProcContext, &Row) -> Result<ProcDecision, FerriteError> + Send + Sync + 'static,
    {
        self.register_trigger(Trigger::new(name, table, event, body));
    }

    pub fn triggers_for(&self, table: TableId, event: TriggerEvent) -> &[Trigger] {
        self.triggers
            .get(&(table, event))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Run every `BEFORE` trigger registered for `(table, event)`, in
    /// registration order.
    ///
    /// A `Replace` is visible to the triggers that run after it, so an
    /// audit trigger and a validation trigger compose. `Skip` short-
    /// circuits. An `Err` propagates immediately and the executor turns it
    /// into a failed statement — that is how a trigger refuses.
    pub fn fire_before(
        &self,
        ctx: &ProcContext,
        event: TriggerEvent,
        row: &Row,
    ) -> Result<ProcDecision, FerriteError> {
        let Some(table) = ctx.table() else {
            return Ok(ProcDecision::Allow);
        };

        let mut replaced: Option<Row> = None;
        for trigger in self.triggers_for(table, event) {
            let current = replaced.as_ref().unwrap_or(row);
            match trigger.call(ctx, current)? {
                ProcDecision::Allow => {}
                ProcDecision::Skip => return Ok(ProcDecision::Skip),
                ProcDecision::Replace(new) => replaced = Some(new),
            }
        }

        Ok(match replaced {
            Some(row) => ProcDecision::Replace(row),
            None => ProcDecision::Allow,
        })
    }

    pub fn register_procedure(&mut self, procedure: Procedure) {
        self.procedures.insert(procedure.name.clone(), procedure);
    }

    pub fn procedure(&self, name: &str) -> Option<&Procedure> {
        self.procedures.get(name)
    }

    /// Look up, authorize, then run. The permission check happens before
    /// the body, so a denied call has no side effects.
    pub fn call(
        &self,
        ctx: &ProcContext,
        name: &str,
        args: &[Value],
    ) -> Result<Value, FerriteError> {
        let procedure = self
            .procedures
            .get(name)
            .ok_or_else(|| FerriteError::Exec(format!("no such procedure: {name}")))?;
        ctx.require(procedure.required_permission)?;
        procedure.call(ctx, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const TABLE: TableId = 7;

    fn role(name: &str, permissions: Vec<Permission>) -> Role {
        Role {
            name: name.into(),
            permissions,
        }
    }

    fn writer() -> Identity {
        Identity([1u8; 32])
    }

    fn intruder() -> Identity {
        Identity([9u8; 32])
    }

    fn row(n: i64) -> Row {
        Row::new(vec![Value::Int8(n), Value::Text("x".into())])
    }

    #[test]
    fn an_unknown_identity_has_no_roles() {
        let registry = ProcRegistry::new();
        assert!(registry.roles_for(intruder()).is_empty());
        assert!(registry
            .authorize(intruder(), 1, Permission::Select)
            .is_err());
    }

    #[test]
    fn granted_roles_accumulate() {
        let mut registry = ProcRegistry::new();
        registry.grant_role(writer(), role("reader", vec![Permission::Select]));
        registry.grant_role(writer(), role("inserter", vec![Permission::Insert]));

        assert!(registry.authorize(writer(), 1, Permission::Select).is_ok());
        assert!(registry.authorize(writer(), 1, Permission::Insert).is_ok());
        assert!(registry.authorize(writer(), 1, Permission::Delete).is_err());
    }

    #[test]
    fn a_trigger_refuses_by_returning_permission_denied() {
        let mut registry = ProcRegistry::new();
        registry.register_before("owner_only", TABLE, TriggerEvent::Insert, |ctx, _row| {
            if ctx.sender() == writer() {
                Ok(ProcDecision::Allow)
            } else {
                Err(FerriteError::PermissionDenied("not the owner".into()))
            }
        });

        let ctx = registry.context(writer(), 1).with_table(TABLE, "notes");
        assert_eq!(
            registry
                .fire_before(&ctx, TriggerEvent::Insert, &row(1))
                .unwrap(),
            ProcDecision::Allow
        );

        let ctx = registry.context(intruder(), 1).with_table(TABLE, "notes");
        assert!(matches!(
            registry.fire_before(&ctx, TriggerEvent::Insert, &row(1)),
            Err(FerriteError::PermissionDenied(_))
        ));
    }

    #[test]
    fn a_trigger_can_rewrite_the_row_and_the_next_trigger_sees_it() {
        let mut registry = ProcRegistry::new();
        registry.register_before("stamp", TABLE, TriggerEvent::Insert, |_ctx, row| {
            let mut new = row.clone();
            new.values[1] = Value::Text("stamped".into());
            Ok(ProcDecision::Replace(new))
        });
        registry.register_before(
            "assert_stamped",
            TABLE,
            TriggerEvent::Insert,
            |_ctx, row| {
                assert_eq!(row.values[1], Value::Text("stamped".into()));
                Ok(ProcDecision::Allow)
            },
        );

        let ctx = registry.context(writer(), 1).with_table(TABLE, "notes");
        let decision = registry
            .fire_before(&ctx, TriggerEvent::Insert, &row(1))
            .unwrap();

        assert_eq!(
            decision,
            ProcDecision::Replace(Row::new(vec![
                Value::Int8(1),
                Value::Text("stamped".into())
            ]))
        );
    }

    #[test]
    fn skip_short_circuits_the_remaining_triggers() {
        let ran = Arc::new(AtomicUsize::new(0));
        let mut registry = ProcRegistry::new();
        registry.register_before("skipper", TABLE, TriggerEvent::Delete, |_ctx, _row| {
            Ok(ProcDecision::Skip)
        });
        let counter = Arc::clone(&ran);
        registry.register_before("never", TABLE, TriggerEvent::Delete, move |_ctx, _row| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(ProcDecision::Allow)
        });

        let ctx = registry.context(writer(), 1).with_table(TABLE, "notes");
        assert_eq!(
            registry
                .fire_before(&ctx, TriggerEvent::Delete, &row(1))
                .unwrap(),
            ProcDecision::Skip
        );
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn triggers_are_scoped_to_their_table_and_event() {
        let mut registry = ProcRegistry::new();
        registry.register_before("insert_only", TABLE, TriggerEvent::Insert, |_ctx, _row| {
            Err(FerriteError::PermissionDenied("nope".into()))
        });

        let ctx = registry.context(writer(), 1).with_table(TABLE, "notes");
        assert_eq!(
            registry
                .fire_before(&ctx, TriggerEvent::Update, &row(1))
                .unwrap(),
            ProcDecision::Allow
        );

        let ctx = registry.context(writer(), 1).with_table(TABLE + 1, "other");
        assert_eq!(
            registry
                .fire_before(&ctx, TriggerEvent::Insert, &row(1))
                .unwrap(),
            ProcDecision::Allow
        );
    }

    #[test]
    fn a_procedure_needs_its_declared_permission() {
        let mut registry = ProcRegistry::new();
        registry.register_procedure(Procedure::new(
            "double",
            Permission::Execute,
            |_ctx, args| match args.first() {
                Some(Value::Int8(n)) => Ok(Value::Int8(n * 2)),
                _ => Err(FerriteError::Exec("double(int8) expected".into())),
            },
        ));
        registry.grant_role(writer(), role("runner", vec![Permission::Execute]));

        let ctx = registry.context(writer(), 1);
        assert_eq!(
            registry.call(&ctx, "double", &[Value::Int8(21)]).unwrap(),
            Value::Int8(42)
        );

        let ctx = registry.context(intruder(), 1);
        assert!(matches!(
            registry.call(&ctx, "double", &[Value::Int8(21)]),
            Err(FerriteError::PermissionDenied(_))
        ));
    }

    #[test]
    fn a_denied_procedure_never_runs_its_body() {
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ran);
        let mut registry = ProcRegistry::new();
        registry.register_procedure(Procedure::new(
            "side_effect",
            Permission::Execute,
            move |_ctx, _args| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Null)
            },
        ));

        let ctx = registry.context(intruder(), 1);
        assert!(registry.call(&ctx, "side_effect", &[]).is_err());
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_procedure_can_authorize_per_row_in_its_own_body() {
        let mut registry = ProcRegistry::new();
        registry.register_procedure(Procedure::new(
            "read_own_note",
            Permission::Execute,
            |ctx, args| {
                let owner = match args.first() {
                    Some(Value::Uuid(id)) => *id,
                    _ => return Err(FerriteError::Exec("read_own_note(uuid) expected".into())),
                };
                let Identity(bytes) = ctx.sender();
                if u128::from_be_bytes(bytes[..16].try_into().unwrap()) == owner {
                    Ok(Value::Text("secret".into()))
                } else {
                    Err(FerriteError::PermissionDenied("not your note".into()))
                }
            },
        ));
        registry.grant_role(writer(), role("runner", vec![Permission::Execute]));
        registry.grant_role(intruder(), role("runner", vec![Permission::Execute]));

        let owner = u128::from_be_bytes([1u8; 16]);

        let ctx = registry.context(writer(), 1);
        assert_eq!(
            registry
                .call(&ctx, "read_own_note", &[Value::Uuid(owner)])
                .unwrap(),
            Value::Text("secret".into())
        );

        let ctx = registry.context(intruder(), 1);
        assert!(matches!(
            registry.call(&ctx, "read_own_note", &[Value::Uuid(owner)]),
            Err(FerriteError::PermissionDenied(_))
        ));
    }

    #[test]
    fn calling_an_unknown_procedure_is_an_execution_error() {
        let registry = ProcRegistry::new();
        let ctx = registry.context(writer(), 1);
        assert!(matches!(
            registry.call(&ctx, "nope", &[]),
            Err(FerriteError::Exec(_))
        ));
    }
}
