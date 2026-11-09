//! Ensaio (Portuguese, trial/rehearsal/assay) — the typed
//! architectural-trial primitive.
//!
//! A TYPED-SPEC + INTERPRETER TRIPLET: a typed Rust border
//! ([`types::EnsaioSpec`]), an authored tatara-lisp surface
//! (`(defensaio ...)`, see `specs/*.ensaio.lisp`), and a working
//! interpreter ([`interp::apply`]) behind a mockable
//! [`interp::EnsaioEnvironment`] trait. Case 1: does an
//! Authservice-shaped gate admit a machine-to-machine call before the
//! backend's own proof is evaluated — across every native access method,
//! not one.
//!
//! M2: [`crd::Ensaio`] + [`controller::run`] — a real kube-rs
//! `Controller` converging `status.phase` toward a terminal state,
//! and [`k8s_env::K8sEnvironment`] — a real, honestly-bounded
//! [`EnsaioEnvironment`] backing it.

pub mod cli;
pub mod controller;
pub mod crd;
pub mod error;
pub mod field;
pub mod fixture;
pub mod interp;
pub mod k8s_env;
pub mod loader;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
/// The reactive-nervous-system extension point — [`nervous::PhaseEvent`],
/// published once per `Ensaio` phase transition via
/// [`interp::EnsaioEnvironment::publish_phase_event`].
pub mod nervous;
/// The PROVE-axis attestation object — [`receipt::CertificationReceipt`],
/// a typed, content-addressed projection over an [`EnsaioOutcome`]'s
/// assertion results that partitions gating preconditions from
/// non-gating known gaps and reserves the word "certified" for what
/// earns it. Mirrors a bootstrap tool's `ApplyReceipt.ReceiptSHA`.
pub mod receipt;
pub mod types;
/// Visita — a trial run as a guest in an environment we do not own:
/// snapshot, declared reversible steps, operator-side observation, undo, and a
/// restore diff that has to come back empty. Nothing lands in the target.
pub mod visita;

pub use error::EnsaioError;
pub use field::{FieldCheck, FieldPath, FieldPredicate, FieldTest, Scalar};
pub use interp::{EnsaioEnvironment, apply, unimplemented_kind};
pub use loader::{load_all, load_one};
pub use nervous::PhaseEvent;
pub use receipt::{
    AssertionRecord, CertVerdict, CertificationInput, CertificationReceipt, KnownGap, ReceiptError,
};
pub use types::{
    AssertionKind, AssertionResult, AssertionSpec, Domain, EmptyWitness, EnsaioOutcome, EnsaioSpec,
    EnvVarSpec, HpaPolicy, NonEmpty, Observation, Observations, OperatorParams, Presence,
    SecretKeyRef, SidecarSpec, Stat, SystemSpec, Verdict, VerdictError, WiringSpec,
};
