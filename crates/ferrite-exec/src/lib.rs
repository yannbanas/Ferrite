//! Owned by Agent 3, alongside `ferrite-planner` and `ferrite-proc`.
//! Single-threaded (v1) executor: walks a physical plan, pulling rows
//! through `ferrite_common::StorageEngine`/`Catalog`, invoking
//! `ferrite-proc` for triggers on insert/update/delete.

pub struct NotYetImplemented;
