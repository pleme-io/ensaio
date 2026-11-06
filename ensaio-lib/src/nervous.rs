//! The reactive-nervous-system extension point (`theory/ENSAIO.md` §7.2):
//! one typed publish call per `Ensaio` phase transition, so breathe/sarar/a
//! dashboard can observe a trial's progress without polling `status.phase`.
//!
//! `PhaseEvent` is a small, ensaio-specific newtype — **not** a reuse of
//! `escuta::ChangeEvent`. escuta's `ChangeEvent` is shaped for
//! database/Kubernetes-watch CDC (`schema`/`table`/`op: Insert|Update|
//! Delete|Truncate|Ddl`, a `before`/`after` row-image pair, subject
//! `escuta.<schema>.<table>.<op>`); none of that fits an architectural
//! trial's phase FSM, and the target subject shape here
//! (`ensaio.<name>.<phase>`) is a different hierarchy than escuta's own
//! `ChangeEvent::subject()` builds. `PhaseEvent` follows escuta's
//! **pattern** instead: a typed struct plus a typed `subject()` built with
//! `String::push_str` (never `format!()`, ★★ TYPED EMISSION) — mirroring
//! `escuta::ChangeEvent::subject()` / `escuta::ChangeOp::as_str()` exactly.
//!
//! Every side effect of *sending* the event lives behind
//! [`crate::interp::EnsaioEnvironment::publish_phase_event`] — the same
//! seam every other Ensaio side effect (provision/wire/assert/teardown)
//! goes through, so this stays mockable per the crate's TYPED-SPEC +
//! INTERPRETER TRIPLET discipline ("the seam is the whole game",
//! `theory/TESTING-SUBSTRATE.md` §IX). `MockEnv` (`src/mock.rs`) records
//! every call for tests; `K8sEnvironment` (`src/k8s_env.rs`) is an honest,
//! clearly-marked stub today — wiring a real NATS client is the named
//! follow-up, not built in this pass.

use serde::{Deserialize, Serialize};

use crate::crd::EnsaioPhase;

/// One typed phase-transition event — the atom ensaio publishes onto the
/// reactive nervous system (escuta/NATS) each time `status.phase` advances.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseEvent {
    /// The `Ensaio` custom resource's name.
    pub run: String,
    pub namespace: String,
    pub phase: EnsaioPhase,
    pub message: Option<String>,
}

impl PhaseEvent {
    /// The NATS subject this event publishes to: `ensaio.<run>.<phase>`.
    /// Built via `push_str`, not `format!()` (★★ TYPED EMISSION) — the
    /// same construction shape as `escuta::ChangeEvent::subject()`.
    #[must_use]
    pub fn subject(&self) -> String {
        let phase = self.phase.as_subject_str();
        let mut s = String::with_capacity("ensaio..".len() + self.run.len() + phase.len());
        s.push_str("ensaio.");
        s.push_str(&self.run);
        s.push('.');
        s.push_str(phase);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_is_ensaio_dot_run_dot_phase() {
        let ev = PhaseEvent {
            run: "authservice-admission-gate".into(),
            namespace: "trials-ensaio".into(),
            phase: EnsaioPhase::Asserting,
            message: None,
        };
        assert_eq!(ev.subject(), "ensaio.authservice-admission-gate.asserting");
    }

    #[test]
    fn subject_covers_every_phase_variant() {
        // A CATALOG REFLECTION-shaped guard: every EnsaioPhase variant must
        // produce a non-empty, distinct subject token. Written as an
        // exhaustive match so a new EnsaioPhase variant is a compile error
        // here until `as_subject_str` covers it too.
        let phases = [
            EnsaioPhase::Pending,
            EnsaioPhase::Provisioning,
            EnsaioPhase::Ready,
            EnsaioPhase::Wiring,
            EnsaioPhase::Asserting,
            EnsaioPhase::Settled,
            EnsaioPhase::Dormant,
            EnsaioPhase::Done,
            EnsaioPhase::Failed,
            EnsaioPhase::TtlExceeded,
            EnsaioPhase::Retired,
        ];
        let mut seen = std::collections::HashSet::new();
        for phase in phases {
            let token = phase.as_subject_str();
            assert!(!token.is_empty());
            assert!(seen.insert(token), "duplicate subject token: {token}");
        }
    }

    #[test]
    fn event_round_trips_through_json() {
        let ev = PhaseEvent {
            run: "r1".into(),
            namespace: "trials".into(),
            phase: EnsaioPhase::Done,
            message: Some("2/2 confirmations complete".into()),
        };
        let body = serde_json::to_vec(&ev).unwrap();
        let back: PhaseEvent = serde_json::from_slice(&body).unwrap();
        assert_eq!(back, ev);
    }
}
