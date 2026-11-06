//! The shared reconcile-loop vocabulary — [`StepOutcome`] and
//! [`ControllerError`], relocated **verbatim** from ensaio's own
//! `crate::controller` to their real home.
//!
//! These are the single piece of genuinely *shared code* (not just shared
//! shape) in the ephemeral-lifecycle family: viveiro's two pure phase cores
//! (`step_warm` / `step_claim`) already `use crate::controller::{ControllerError,
//! StepOutcome}` — Operating Principle #1, live reuse today. Relocating them
//! here and re-exporting from ensaio removes that cross-file coupling while
//! keeping the live consumer compiling with zero body edits.
//!
//! `ControllerError` is relocated with its `Kube(#[from] kube::Error)` variant
//! intact because BOTH consumers (ensaio's own `patch_status`, viveiro's
//! `patch_warm_status` / `patch_claim_status`) rely on `?`-converting a
//! `kube::Error` through it. That single variant is the only reason this
//! otherwise-pure crate depends on `kube`.

/// What a pure phase-FSM step tells its async controller wrapper to do next:
/// wait for the next watch event, or requeue after an interval. Kept separate
/// from `kube::runtime::controller::Action` so the pure FSM cores carry no
/// kube-rs/tokio dependency and stay directly unit-testable against a mock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// Nothing to do until the next watch event (a steady/terminal phase, or a
    /// phase whose next edge is driven cross-resource).
    AwaitChange,
    /// Re-reconcile after this interval (an active phase making progress, or a
    /// retry after a held error).
    Requeue(std::time::Duration),
}

/// A controller error surfaced by a pure phase step or its async wrapper.
///
/// `Rig` is the only variant a pure FSM core ever constructs (a hard
/// environment/rig failure); `Kube` is constructed by the async wrapper via
/// `?` on a Kubernetes API call. Implements `std::error::Error` (via
/// `thiserror`) so it satisfies `kube::runtime::finalizer::Error<E>`'s bound.
#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    /// A Kubernetes API error (`patch_status`, watch, apply, …), converted via
    /// `?`. The one reason this crate depends on `kube`.
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    /// A rig/environment failure a pure step chose to hard-fail on (e.g. a
    /// wiring edge that can't be applied).
    #[error("rig error: {0}")]
    Rig(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn step_outcome_is_copy_and_comparable() {
        let a = StepOutcome::Requeue(Duration::from_secs(5));
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(StepOutcome::AwaitChange, a);
    }

    #[test]
    fn controller_error_rig_renders_its_message() {
        let e = ControllerError::Rig("wiring edge failed".to_string());
        assert_eq!(e.to_string(), "rig error: wiring edge failed");
    }

    #[test]
    fn controller_error_is_std_error() {
        // Proves the `finalizer::Error<ControllerError>` bound is satisfiable
        // (the trait object constructs, i.e. the type is `std::error::Error`).
        let e = ControllerError::Rig("x".into());
        let _dyn: &dyn std::error::Error = &e;
    }
}
