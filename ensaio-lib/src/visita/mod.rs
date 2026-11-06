//! Visita — a trial run as a guest in an environment we do not own.
//!
//! A shared staging tenant is borrowed ground. A visita changes it only as a
//! declared step, observes it through its own API, always puts it back, and
//! finishes with a diff that proves it is back. The trial is "restored" only
//! when that diff is empty modulo a declared allow-list.
//!
//! The four moves, each typed:
//!
//! | move | type | seal |
//! |---|---|---|
//! | snapshot | [`snapshot::NaturalState`] — content-addressed, bookkeeping stripped, secret values hashed | stored locally, never in the target |
//! | mutate | [`crate::fixture::Fixture`] + [`spec::Action`], each with a declared cleanup | `restore-from-snapshot` derives the inverse from the capture; `environment-is-destroyed` is refused |
//! | observe | [`crate::AssertionKind`]'s operator-side kinds ([`spec::OPERATOR_KINDS`]) | every Job-creating kind is refused at load |
//! | prove | [`snapshot::RestoreDiff`] → the `natural-state-restored` verdict | judged by [`crate::Verdict::judge`]: zero objects checked is `Vacuous` |
//!
//! **Nothing lands in the target that the trial does not take back.** Every
//! read and write goes through [`command::Verb`] — no `apply`; `create` and
//! `delete` only for an `:apply-manifest` fixture, which refuses if any object
//! exists and deletes exactly what it created — and runs from the operator's
//! machine with an explicit `--context` that the spec had to name and the CLI
//! had to allow.
//!
//! **Relation to [`crate::interp::apply`].** The same border (fixtures,
//! assertions, verdicts, receipt) and the same [`crate::EnsaioEnvironment`]
//! seam; a different interpreter, because a guest trial interleaves change and
//! observation — assert, fault, assert again — which a
//! fixtures-then-assertions trial cannot express.

pub mod absorb;
pub mod carve;
pub mod cli;
pub mod command;
pub mod config;
pub mod env;
pub mod guard;
pub mod judge;
pub mod lease;
pub mod manifest;
pub mod pipeline;
pub mod reap;
pub mod run;
pub mod snapshot;
pub mod spec;
pub mod tier;

pub use env::OperatorEnvironment;
pub use run::{VisitaReport, run};
pub use snapshot::{NaturalState, RestoreDiff};
pub use spec::{Action, Selection, Step, VisitaSpec};
