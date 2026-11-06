//! [`TeardownPolicy`] — the one genuinely-typed reap-policy sum in the
//! ephemeral-lifecycle family, lifted verbatim from kenshi
//! (`kenshi/src/crd/test_environment.rs`).
//!
//! kenshi's own `should_teardown(&self) -> bool` reads the CR's single
//! `status.phase` directly. The generalization takes an explicit [`Outcome`]
//! so the policy is decoupled from any one domain's phase enum — the faithful
//! lift, not a re-implementation.

use serde::{Deserialize, Serialize};

/// When to tear down an ephemeral resource once it reaches a terminal outcome.
/// Shape and defaults are kenshi's verbatim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeardownPolicy {
    /// Always tear down (default).
    #[default]
    Always,
    /// Only tear down on success.
    OnSuccess,
    /// Only tear down on failure.
    OnFailure,
    /// Never tear down (manual cleanup required). MODULARIZE-DON'T-DELETE: an
    /// explicit "keep it around" is a policy, not the absence of one.
    Never,
}

/// A terminal outcome of a lifecycle, reduced to the one bit
/// [`TeardownPolicy`] needs. This is deliberately *not* the witness-carrying
/// `Terminal::{Accepted(ReapProof)|Rejected(FailureEvidence)}` of Gate-0 #7 —
/// that sealed constructor (whose good arm *requires* a reap proof, making
/// "Done without evidence" unrepresentable) is the named-M1 destination.
/// At M0, `Outcome` is a plain discriminant so `should_teardown` is faithful
/// and testable without prematurely claiming a seal this crate does not ship.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// Reached a terminal-good state.
    Succeeded,
    /// Reached a terminal-bad state.
    Failed,
}

impl TeardownPolicy {
    /// Whether a lifecycle ending in `outcome` should be torn down under this
    /// policy — the exhaustive generalization of kenshi's `should_teardown`.
    #[must_use]
    pub fn should_teardown(self, outcome: Outcome) -> bool {
        match self {
            Self::Always => true,
            Self::OnSuccess => outcome == Outcome::Succeeded,
            Self::OnFailure => outcome == Outcome::Failed,
            Self::Never => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_always_like_kenshi() {
        assert_eq!(TeardownPolicy::default(), TeardownPolicy::Always);
    }

    #[test]
    fn always_tears_down_regardless_of_outcome() {
        assert!(TeardownPolicy::Always.should_teardown(Outcome::Succeeded));
        assert!(TeardownPolicy::Always.should_teardown(Outcome::Failed));
    }

    #[test]
    fn never_tears_down_regardless_of_outcome() {
        assert!(!TeardownPolicy::Never.should_teardown(Outcome::Succeeded));
        assert!(!TeardownPolicy::Never.should_teardown(Outcome::Failed));
    }

    #[test]
    fn on_success_only_on_success() {
        assert!(TeardownPolicy::OnSuccess.should_teardown(Outcome::Succeeded));
        assert!(!TeardownPolicy::OnSuccess.should_teardown(Outcome::Failed));
    }

    #[test]
    fn on_failure_only_on_failure() {
        assert!(TeardownPolicy::OnFailure.should_teardown(Outcome::Failed));
        assert!(!TeardownPolicy::OnFailure.should_teardown(Outcome::Succeeded));
    }
}
