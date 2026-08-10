//! Owned by Agent 3, alongside `ferrite-planner` and `ferrite-proc`.
//! Single-threaded (v1) executor: walks a physical plan, pulling rows
//! through `ferrite_common::StorageEngine`/`Catalog`, invoking
//! `ferrite-proc` for triggers on insert/update/delete and for permission
//! checks.
//!
//! A [`Session`] binds the engines to one caller's
//! [`ferrite_common::Identity`]. Every statement is authorized against
//! that identity before it touches storage, and every row written by
//! `INSERT`/`UPDATE`/`DELETE` passes through the matching `BEFORE`
//! triggers first — which is where Ferrite's access control lives, since
//! there is no declarative RLS (see `ferrite-proc`'s README).

mod aggregate;
mod eval;
mod executor;
mod index;
mod scalar;
mod subquery;

pub use eval::{compare, eval, eval_predicate, like_matches};
pub use executor::{
    Limits, QueryResult, Session, Tuple, DEFAULT_MAX_ROWS, DEFAULT_STATEMENT_TIMEOUT,
};
pub use index::IndexProvider;
pub use scalar::{call, cast};
