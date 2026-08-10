# ferrite-proc

Triggers, stored procedures, **and Ferrite's access-control model**. The
third item is not a side effect of the first two — it is why this crate
exists in the shape it does.

## Why there is no declarative row-level security

Postgres solves per-row access with a policy language:

```sql
CREATE POLICY notes_owner ON notes USING (owner = current_user);
```

Ferrite does not have that, on purpose (`docs/architecture.md` §Modèle de
sécurité). A policy DSL is a second expression language with its own
parser, planner integration, and semantics for how policies compose across
roles, commands and inheritance — a large surface, and one that stops
short exactly when a rule needs to be slightly more than a boolean
expression over the row.

Since Ferrite keeps triggers and stored procedures anyway, and they can
already see the caller, the same job is done with one mechanism instead of
two: **user code receives the caller's identity and decides in code.** This
is the SpacetimeDB reducer model — a reducer reads `ctx.sender()` and
decides — rather than the Postgres model.

The trade is explicit: rules are Rust, not SQL, so they are not
introspectable through a catalog view and not editable at runtime through
DDL. In exchange there is one engine to build, one place to audit, and no
ceiling on what a rule can express.

## What a rule looks like

```rust
use ferrite_common::{FerriteError, Permission};
use ferrite_proc::{ProcContext, ProcDecision, ProcRegistry, TriggerEvent};

let mut registry = ProcRegistry::new();

registry.register_before("notes_owner", notes, TriggerEvent::Update, |ctx, row| {
    ctx.require(Permission::Update)?;
    let old = ctx.old_row().expect("BEFORE UPDATE carries the pre-image");
    if old.values[0] == ferrite_common::Value::Uuid(owner_of(ctx.sender())) {
        Ok(ProcDecision::Allow)
    } else {
        Err(FerriteError::PermissionDenied("not your note".into()))
    }
});
```

The signature is fixed:

```rust
fn(&ProcContext, &Row) -> Result<ProcDecision, FerriteError>
```

`row` is the subject of the operation — the new row for `INSERT`/`UPDATE`,
the row being removed for `DELETE`. The pre-image lives on the context
(`ctx.old_row()`) so the signature stays the same for all three events.

`ProcContext` exposes `sender()`, `roles()`, `has_permission()`,
`require()`, `txn()`, `table()`, `table_name()`, `event()` and
`old_row()`. It is built by the executor from the registry's own role
table and holds a borrowed role slice, so procedure code cannot grant
itself anything.

## ProcDecision

| Variant | Meaning |
| --- | --- |
| `Allow` | proceed with the row unchanged |
| `Replace(Row)` | proceed with this row instead (audit columns, normalization, masking) |
| `Skip` | leave this row alone, continue the statement |
| *(`Err(PermissionDenied)`)* | refuse: the whole statement fails |

Refusal is an `Err`, not a variant — a rejected write must abort the
statement rather than silently doing nothing, which is what `Skip` is for.
The executor re-validates a `Replace`d row against the table schema before
writing, so a trigger cannot smuggle a malformed row past type and
nullability checks.

Triggers on the same `(table, event)` run in registration order and a
`Replace` is visible to the ones after it, so validation and audit
triggers compose. `Skip` short-circuits the rest.

## Permissions

`ferrite_common::Role` is a name plus a flat `Vec<Permission>`. No
per-column grants, no `WITH GRANT OPTION` chains, no role hierarchy. An
identity's permissions are the union across its roles; `Permission::Admin`
implies every other one.

**Deny by default**: an identity the registry has never seen holds no
roles and therefore no permissions. `Identity::ANONYMOUS` is not special —
it is simply an identity nobody granted anything to. Every denial goes
through `ProcContext::require`, which emits a `tracing::warn!` carrying the
identity and the permission, as `docs/architecture.md` requires for
security events.

Two layers apply, and both must pass:

1. **Statement level** — the executor calls `authorize(identity, txn, ..)`
   before touching storage: `Select` for queries, `Insert`/`Update`/
   `Delete` for writes.
2. **Row level** — `BEFORE` triggers, which is where anything conditional
   on the row's contents belongs.

Stored procedures declare the permission they need at registration
(`Permission::Execute` for a plain one). `ProcRegistry::call` checks it
before running the body, so a denied call has no side effects.

## Known limits

- `BEFORE` triggers only. `AFTER` needs the executor to defer work to
  commit time, which the v1 executor does not do.
- Row-level triggers only; no statement-level triggers.
- Procedures are native Rust closures registered at startup. There is no
  procedural language and no `CREATE FUNCTION` — v1 cuts the extension
  system, so no PL/pgSQL and no PL/Python.
- A procedure cannot yet issue nested statements: it receives the context
  and its arguments, not a handle back into the executor. That handle is
  the obvious next addition and would let a procedure read rows to make its
  decision.
