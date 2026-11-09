//! The `Ensaio` Kubernetes CRD — the M2 declaration surface. Rendered
//! from an `EnsaioSpec` (the lisp-facing typed border, `types.rs`);
//! `status.phase` is the kubectl-queryable verdict surface.
//!
//! Ensaio's controller is modeled on Autorevivy's CRD/RBAC/scope-gate
//! conventions (`group.pleme.io/v1`, `#[kube(namespaced, status)]`, a
//! `spec.scope` runtime gate, RBAC narrow-write) but runs
//! the idiomatic kube-rs `Controller` event-driven reconcile loop —
//! Autorevivy's own reconciler is a hand-rolled poll-and-sweep on a
//! fixed interval (300s default), the wrong fit for a bounded, TTL'd
//! run someone is actively iterating on.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::{AssertionResult, AssertionSpec, EnsaioSpec, SystemSpec, WiringSpec};

/// The `Ensaio` custom resource's spec — the K8s-facing counterpart of
/// [`EnsaioSpec`] (the lisp-facing typed border). Convert with
/// [`EnsaioCrdSpec::from_spec`].
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "ensaio.pleme.io",
    version = "v1alpha1",
    kind = "Ensaio",
    plural = "ensaios",
    shortname = "ens",
    namespaced,
    status = "EnsaioStatus",
    printcolumn = r#"{"name":"Phase", "type":"string", "jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Claim", "type":"string", "jsonPath":".spec.claim"}"#,
    printcolumn = r#"{"name":"Age", "type":"date", "jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct EnsaioCrdSpec {
    /// Must equal the controller's `ENSAIO_SCOPE` — the same scope gate Autorevivy uses.
    /// Enforced at runtime (the controller ignores any other value)
    /// and by RBAC (the controller's `ClusterRole` grants write only
    /// inside the deployment's own namespaces).
    pub scope: String,
    pub ttl: String,
    pub claim: String,
    #[serde(default)]
    pub confluence_ref: Option<String>,
    pub systems: Vec<SystemSpec>,
    #[serde(default)]
    pub wiring: Vec<WiringSpec>,
    pub assertions: Vec<AssertionSpec>,
    #[serde(default)]
    pub record: bool,
    /// The stability gate (borrowed from breathe's `confirmAfterSeconds`
    /// pattern): re-run the assertion catalog this many times, all
    /// agreeing, before the controller commits to a final verdict.
    #[serde(default = "default_confirm_after_runs")]
    pub confirm_after_runs: u32,
}

fn default_confirm_after_runs() -> u32 {
    1
}

impl EnsaioCrdSpec {
    #[must_use]
    pub fn from_spec(spec: &EnsaioSpec, confirm_after_runs: u32) -> Self {
        Self {
            scope: spec.scope.clone(),
            ttl: spec.ttl.clone(),
            claim: spec.claim.clone(),
            confluence_ref: spec.confluence_ref.clone(),
            systems: spec.systems.clone(),
            wiring: spec.wiring.clone(),
            assertions: spec.assertions.clone(),
            record: spec.record,
            confirm_after_runs,
        }
    }
}

/// The `Ensaio` custom resource's status — the kubectl-queryable
/// verdict surface (`kubectl get ensaio -o jsonpath='{.status.phase}'`).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnsaioStatus {
    #[serde(default)]
    pub phase: EnsaioPhase,
    #[serde(default)]
    pub results: Vec<AssertionResult>,
    /// How many consecutive assertion passes have completed toward
    /// `spec.confirm_after_runs`.
    #[serde(default)]
    pub confirmations: u32,
    #[serde(default)]
    pub systems_provisioned: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// The lifecycle FSM. Converges toward a **terminal** phase
/// (`Done`/`Failed`/`TTLExceeded`) — never an indefinite tick, the
/// structural difference from a Viggy `(defpromessa)` steady-state
/// promise.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
pub enum EnsaioPhase {
    #[default]
    Pending,
    Provisioning,
    Ready,
    Wiring,
    Asserting,
    /// Warm, between-iteration — every provisioned system stays up;
    /// re-running assertions from here costs no re-provisioning.
    Settled,
    /// Scaled to zero (destination: a breathe `ReplicaBand` floor;
    /// today, an honest interim — see `k8s_env.rs`). Cost-free,
    /// instantly wakeable back to `Provisioning`.
    Dormant,
    Done,
    Failed,
    TtlExceeded,
    /// The declaration itself removed. Rare, deliberate — never the
    /// default per-run action.
    Retired,
}

impl EnsaioPhase {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::TtlExceeded | Self::Retired
        )
    }

    /// The wire token used in the reactive-nervous-system subject
    /// (`ensaio.<name>.<phase>`, see `crate::nervous::PhaseEvent`) — a
    /// typed surface mirroring `escuta::ChangeOp::as_str()`, never a
    /// `format!()` of `Debug` output.
    #[must_use]
    pub fn as_subject_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
            Self::Wiring => "wiring",
            Self::Asserting => "asserting",
            Self::Settled => "settled",
            Self::Dormant => "dormant",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::TtlExceeded => "ttl-exceeded",
            Self::Retired => "retired",
        }
    }
}
