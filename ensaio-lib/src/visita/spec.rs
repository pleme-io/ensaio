//! `(defvisita …)` — the authored form of a guest trial.
//!
//! ```lisp
//! (defvisita
//!   :name "hpa-surge-change"
//!   :claim "The new strategy rolls with at most one extra pod and none unavailable."
//!   :natural-state ((:context "stage" :namespace "default"
//!                    :kinds ("deployment" "hpa" "configmap")
//!                    :names ("svc-*")))
//!   :allow ()
//!   :fixtures ((:name "pin" :effect mutate :cleanup restore-from-snapshot
//!               :action (:annotate (:context "argo" :namespace "argocd"
//!                                   :resource "secret" :name "stage-cluster"
//!                                   :key "branch-pin" :value "my-branch"))))
//!   :assertions ((:id "rollout" :kind RolloutSurgeBound :target "svc"
//!                 :context "stage" :target-namespace "default"
//!                 :names ("svc-deployment") :max-surge 1 :max-unavailable 0
//!                 :window "20m"))
//!   :steps ((:place "pin") (:observe "rollout"))
//!   :settle "10m")
//! ```
//!
//! Steps are DECLARED and run in order, because a guest trial interleaves
//! change and observation (assert, fault, assert again) in a way a flat
//! fixtures-then-assertions trial cannot express. Undo is the reverse of what
//! was actually placed, and it always runs.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::fixture::{Cleanup, Effect, Fixture, placement_order};
use crate::types::{AssertionKind, AssertionSpec};

/// One guest trial.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defvisita")]
pub struct VisitaSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub claim: String,
    /// What "back to natural" means: the objects captured before the first
    /// write and compared after the last undo.
    pub natural_state: Vec<Selection>,
    /// Extra `Kind/name-glob:path-glob` entries the restore diff ignores, on
    /// top of [`super::snapshot::DEFAULT_ALLOW`].
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub fixtures: Vec<Fixture>,
    #[serde(default)]
    pub assertions: Vec<AssertionSpec>,
    pub steps: Vec<Step>,
    /// Fixtures placed AFTER every undo and BEFORE the diff — the part of
    /// putting an environment back that is not the inverse of any one step.
    ///
    /// The case that needs it: a branch pin plus a pipeline dispatch. The
    /// dispatch's effect is undone by syncing the app to its natural revision,
    /// and that sync is only meaningful once the pin is gone — but undo runs in
    /// reverse placement order, so as an inverse it would run while still
    /// pinned. Declared here, it runs after the unpin.
    #[serde(default)]
    pub restore: Vec<String>,
    /// A probe on the pods the trial loads (a broker, say). If one stays
    /// unready past `max_unready`, restarts, or comes within
    /// `liveness_margin` of its liveness kill, the run stops placing and
    /// observing, undoes AT ONCE (the tripped pod's fixtures first, then every
    /// other in-place resize), and ends
    /// Inconclusive with the reason. A throttled pod is never left behind.
    #[serde(default)]
    pub guard: Option<Guard>,
    /// Branches that must contain their trunk. A pin or dispatch of a branch
    /// that is BEHIND its trunk deploys an older trunk and silently reverts
    /// whatever landed since — so each is read at run start (refusing the run
    /// before anything is written) and again immediately before every
    /// placement that names the branch (trunk can move mid-run).
    #[serde(default)]
    pub branch_contains: Vec<BranchContains>,
    /// Assertion ids that GATE the run: any of them not held makes the whole
    /// receipt Inconclusive (every other assertion is routed to Skipped). An
    /// A/B's "side A reproduced the failure" belongs here — if A did not
    /// reproduce, B passing proves nothing.
    #[serde(default)]
    pub preconditions: Vec<String>,
    /// How long the restore may take to converge before the diff is final
    /// (humantime, e.g. `"10m"`). Default: one immediate comparison.
    #[serde(default)]
    pub settle: Option<String>,
}

/// A set of objects that make up part of the natural state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    #[serde(default)]
    pub context: String,
    pub namespace: String,
    /// kubectl resource names, e.g. `deployment`, `hpa`, `secret`,
    /// `applications.argoproj.io`.
    pub kinds: Vec<String>,
    /// Name globs to keep. Empty keeps everything the read returned. Every
    /// pattern must match at least one object or the capture is refused.
    #[serde(default)]
    pub names: Vec<String>,
    /// Label selector passed to the read.
    #[serde(default)]
    pub selector: Option<String>,
    /// `status` paths (dotted) that ARE natural state for these objects — an
    /// Argo Application's `status.sync.revision`, say. Everything else under
    /// `status` is stripped.
    #[serde(default)]
    pub keep_status: Vec<String>,
}

/// "`branch` contains every commit of `trunk`" in `repo` (`owner/name`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BranchContains {
    pub repo: String,
    pub branch: String,
    pub trunk: String,
}

impl Action {
    /// Whether this action deploys or pins `branch` by name: an annotation
    /// value, a dispatch ref or input, a sync revision.
    #[must_use]
    pub fn names_branch(&self, branch: &str) -> bool {
        match self {
            Self::Annotate { value, .. } => value == branch,
            Self::DispatchWorkflow {
                git_ref, inputs, ..
            } => git_ref == branch || inputs.iter().any(|i| i.value == branch),
            Self::ArgoSync { revision, .. } => revision == branch,
            Self::Patch { .. }
            | Self::ResizePod { .. }
            | Self::RolloutRestart { .. }
            | Self::RabbitmqCloseConnections { .. }
            | Self::ApplyManifest { .. }
            | Self::Exec { .. }
            | Self::CarveNamespace { .. }
            | Self::Fence { .. }
            | Self::CarveNode { .. }
            | Self::ArgocdPause { .. }
            | Self::DeleteRunnerPod { .. } => false,
        }
    }
}

/// The guard's subject and bound.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Guard {
    #[serde(default)]
    pub context: String,
    pub namespace: String,
    /// Pod names watched. Each must exist when the guard starts.
    pub pods: Vec<String>,
    /// Longest a watched pod may be not-Ready (humantime), e.g. `60s`.
    pub max_unready: String,
    /// Probe interval (humantime), default `5s`.
    #[serde(default)]
    pub interval: Option<String>,
    /// How many liveness failures short of a container's `failureThreshold`
    /// the guard trips, default 2. Read from each watched pod's own probe; a
    /// container without one gives no liveness signal.
    #[serde(default)]
    pub liveness_margin: Option<u32>,
}

/// One step of the trial.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// Place the named fixture.
    Place(String),
    /// Run the named assertion.
    Observe(String),
    /// Pause (humantime).
    Wait(String),
    /// Undo the named fixture NOW, by its declared cleanup — the end of a
    /// phase's load, before the next phase begins. It leaves the undo stack.
    Undo(String),
    /// A phase boundary: a label in the evidence and the report.
    Phase(String),
}

/// What a fixture does, typed. The argv is derived from this; nobody writes it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Action {
    /// Set one annotation. With `restore-from-snapshot` the inverse puts back
    /// the captured value, or removes the key if the natural state had none.
    Annotate {
        #[serde(default)]
        context: String,
        namespace: String,
        resource: String,
        name: String,
        key: String,
        value: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        absorb: bool,
    },
    /// A JSON merge patch. With `restore-from-snapshot` the inverse is derived
    /// from the captured object: every key the patch touches is put back to
    /// its natural value (or `null` if it was absent).
    Patch {
        #[serde(default)]
        context: String,
        namespace: String,
        resource: String,
        name: String,
        patch: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        absorb: bool,
    },
    /// `gh workflow run`. Not invertible from a snapshot: its inverse is a
    /// different action (typically [`Action::ArgoSync`] to the natural
    /// revision), declared as `(:inverse …)`.
    ///
    /// Placement is AWAITED: the run id is read from the dispatch's output
    /// (the run URL), then the run is polled until it reaches `await` — a job,
    /// or a step within one — or completes. A pipeline builds before it
    /// deploys, so returning at dispatch lets every following observation
    /// look at the old build. A failing job or step aborts placement unless it
    /// is named in `accept_failure` (a check that fails by design, such as a
    /// per-region version check on a one-region pin).
    ///
    /// Its only valid cleanup is [`Cleanup::Stop`]: a run left in flight
    /// outlives the trial and can undo the restore.
    DispatchWorkflow {
        repo: String,
        workflow: String,
        git_ref: String,
        #[serde(default)]
        inputs: Vec<WorkflowInput>,
        /// What "placed" means. `None` waits for the run to complete.
        #[serde(default, rename = "await")]
        until: Option<Await>,
        /// Job or step names (substring, case-insensitive) whose failure is
        /// expected and does not abort placement.
        #[serde(default)]
        accept_failure: Vec<String>,
        /// Bound on the wait (humantime), default `60m`.
        #[serde(default)]
        timeout: Option<String>,
        /// Poll interval (humantime), default `20s`.
        #[serde(default)]
        poll: Option<String>,
    },
    /// Ask ArgoCD to sync an Application to a revision, through the
    /// Application's own `operation` field — the same write the argocd CLI
    /// makes, with the app's sync options carried.
    ArgoSync {
        #[serde(default)]
        context: String,
        namespace: String,
        application: String,
        revision: String,
        #[serde(default)]
        sync_options: Vec<String>,
        #[serde(default)]
        initiated_by: Option<String>,
    },
    /// Resize one container of one pod IN PLACE through the `resize`
    /// subresource — CPU starvation of a running pod without a restart.
    ///
    /// Refused before any write if the container's `resizePolicy` for cpu
    /// would restart it (unless `allow_restart`), and refused to finish
    /// placement if the kubelet never reports the new resources in
    /// `status.containerStatuses` within `timeout`. The shape of the resize
    /// (policy check, QoS-preserving block, body, convergence) is
    /// `breathe_resize`'s, shared with breathe. Its only valid cleanup is
    /// `restore-from-snapshot`: the pod's declared resources are put back from
    /// the natural state, and awaited the same way.
    ResizePod {
        #[serde(default)]
        context: String,
        namespace: String,
        pod: String,
        container: String,
        #[serde(default)]
        cpu_limit: Option<String>,
        #[serde(default)]
        cpu_request: Option<String>,
        #[serde(default)]
        allow_restart: bool,
        #[serde(default)]
        timeout: Option<String>,
    },
    /// `kubectl rollout restart` of several Deployments at once — the shape of
    /// a fleet-wide restart storm. Transient: the restore sync and the
    /// allow-listed `restartedAt` cover it.
    RolloutRestart {
        #[serde(default)]
        context: String,
        namespace: String,
        deployments: Vec<String>,
    },
    /// Close the RabbitMQ connections that the selected pods hold on one vhost.
    ///
    /// The kind owns the Erlang: pod IPs are read at fault time (an HPA may
    /// have replaced pods since the last listing), each becomes the
    /// IPv4-mapped tuple `peer_host` reports (`10.1.2.3` is
    /// `{0,0,0,0,0,65535,2561,515}`; a plain `{10,1,2,3}` matches
    /// nothing), and the eval runs ONCE because `connection_info_all` is
    /// cluster-wide. It returns the count closed, and **zero is refused**: a
    /// fault that closed nothing must not be read as a recovery.
    RabbitmqCloseConnections {
        #[serde(default)]
        context: String,
        broker_namespace: String,
        broker_pod: String,
        #[serde(default = "default_rabbit_container")]
        broker_container: String,
        vhost: String,
        app_namespace: String,
        app_selector: String,
    },
    CarveNamespace {
        #[serde(default)]
        context: String,
        name: String,
        #[serde(default)]
        quota: Vec<Quantity>,
        #[serde(default)]
        limit_default: Vec<Quantity>,
        #[serde(default)]
        limit_default_request: Vec<Quantity>,
        #[serde(default)]
        timeout: Option<String>,
    },
    Fence {
        #[serde(default)]
        context: String,
        namespace: String,
        #[serde(default)]
        egress: Vec<Peer>,
        #[serde(default)]
        ingress: Vec<Peer>,
        #[serde(default)]
        timeout: Option<String>,
    },
    CarveNode {
        #[serde(default)]
        context: String,
        selector: String,
        taint_key: String,
        #[serde(default)]
        taint_value: Option<String>,
        label_key: String,
        label_value: String,
    },
    ArgocdPause {
        #[serde(default)]
        context: String,
        namespace: String,
        application: String,
    },
    /// Delete the pod that runs one job of a run this trial dispatched.
    ///
    /// The pod is found from the job's own `runner_name` in the run, never
    /// from a selector, so it can only be the pod running our job. Refused,
    /// nothing written, unless the job is `in_progress` on a runner that is a
    /// pod in `namespace`. With `wait` the job is polled until it is running
    /// (a retry job starts only after its predecessor ends). Its only valid
    /// cleanup is `transient`: the pod's controller replaces it.
    DeleteRunnerPod {
        #[serde(default)]
        context: String,
        namespace: String,
        run_of: String,
        job: String,
        #[serde(default)]
        wait: Option<String>,
    },
    /// Create the objects in a manifest — a `file` (relative to the spec) or
    /// inline `yaml` — in one context and namespace.
    ///
    /// Refused before any write if ANY of them already exists: the fixture
    /// owns only what it created. The identities created (with their `uid`)
    /// are recorded in `steps.jsonl`, and its only valid cleanup is
    /// [`Cleanup::OwnInverse`]: delete exactly those objects and wait, bounded
    /// by `timeout` (default `120s`), until they are gone. `await_available`
    /// names Deployments that must report `Available` before it is placed.
    ApplyManifest {
        #[serde(default)]
        context: String,
        namespace: String,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        yaml: Option<String>,
        #[serde(default)]
        await_available: Vec<String>,
        #[serde(default)]
        timeout: Option<String>,
    },
    /// A mutating command in one container: of `pod`, or of the first Ready
    /// pod (by name) behind `selector` — every Ready one with `all`.
    ///
    /// A non-zero exit fails the placement, which aborts the trial and undoes
    /// it. With `undo_argv` its cleanup is [`Cleanup::OwnInverse`]: that argv
    /// runs in every pod the exec was attempted in. Without it the exec is
    /// irreversible and must be declared [`Cleanup::Transient`] — refused at
    /// load otherwise, so no mutation is silently left behind.
    Exec {
        #[serde(default)]
        context: String,
        namespace: String,
        #[serde(default)]
        pod: Option<String>,
        #[serde(default)]
        selector: Option<String>,
        container: String,
        #[serde(default)]
        all: bool,
        argv: Vec<String>,
        #[serde(default)]
        undo_argv: Option<Vec<String>>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Quantity {
    pub name: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum Peer {
    Dns,
    ApiServer,
    Service { namespace: String, name: String },
    Namespace(String),
}

fn default_rabbit_container() -> String {
    "rabbitmq".into()
}

/// The point in a dispatched run that counts as "placed": a job (every step
/// of it) or one step within it has completed. Names match by
/// case-insensitive substring, because a reusable workflow's job shows up as
/// `caller / callee`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Await {
    pub job: String,
    #[serde(default)]
    pub step: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkflowInput {
    pub name: String,
    pub value: String,
}

impl Action {
    /// Whether [`Cleanup::RestoreFromSnapshot`] can derive this action's
    /// inverse.
    #[must_use]
    pub fn snapshot_invertible(&self) -> bool {
        matches!(
            self,
            Self::Annotate { .. } | Self::Patch { .. } | Self::ResizePod { .. }
        )
    }

    /// The object this action writes, when it writes one object.
    #[must_use]
    pub fn target(&self) -> Option<(&str, &str, &str, &str)> {
        match self {
            Self::Annotate {
                context,
                namespace,
                resource,
                name,
                ..
            }
            | Self::Patch {
                context,
                namespace,
                resource,
                name,
                ..
            } => Some((context, namespace, resource, name)),
            Self::ResizePod {
                context,
                namespace,
                pod,
                ..
            } => Some((context, namespace, "pod", pod)),
            Self::RolloutRestart { .. }
            | Self::DispatchWorkflow { .. }
            | Self::ArgoSync { .. }
            | Self::RabbitmqCloseConnections { .. }
            | Self::ApplyManifest { .. }
            | Self::Exec { .. }
            | Self::CarveNamespace { .. }
            | Self::Fence { .. }
            | Self::CarveNode { .. }
            | Self::ArgocdPause { .. }
            | Self::DeleteRunnerPod { .. } => None,
        }
    }

    #[must_use]
    pub fn contexts(&self) -> Vec<&str> {
        match self {
            Self::Annotate { context, .. }
            | Self::Patch { context, .. }
            | Self::ResizePod { context, .. }
            | Self::RolloutRestart { context, .. }
            | Self::ArgoSync { context, .. }
            | Self::RabbitmqCloseConnections { context, .. }
            | Self::ApplyManifest { context, .. }
            | Self::Exec { context, .. }
            | Self::CarveNamespace { context, .. }
            | Self::Fence { context, .. }
            | Self::CarveNode { context, .. }
            | Self::ArgocdPause { context, .. }
            | Self::DeleteRunnerPod { context, .. } => vec![context],
            Self::DispatchWorkflow { .. } => vec![],
        }
    }

    pub fn fill_context(&mut self, ctx: &str) {
        match self {
            Self::Annotate { context, .. }
            | Self::Patch { context, .. }
            | Self::ResizePod { context, .. }
            | Self::RolloutRestart { context, .. }
            | Self::ArgoSync { context, .. }
            | Self::RabbitmqCloseConnections { context, .. }
            | Self::ApplyManifest { context, .. }
            | Self::Exec { context, .. }
            | Self::CarveNamespace { context, .. }
            | Self::Fence { context, .. }
            | Self::CarveNode { context, .. }
            | Self::ArgocdPause { context, .. }
            | Self::DeleteRunnerPod { context, .. } => {
                if context.is_empty() {
                    *context = ctx.to_string();
                }
            }
            Self::DispatchWorkflow { .. } => {}
        }
    }

    #[must_use]
    pub fn scope(&self) -> Vec<String> {
        match self {
            Self::Annotate { namespace, .. }
            | Self::Patch { namespace, .. }
            | Self::ResizePod { namespace, .. }
            | Self::RolloutRestart { namespace, .. }
            | Self::ArgoSync { namespace, .. }
            | Self::ApplyManifest { namespace, .. }
            | Self::Exec { namespace, .. }
            | Self::Fence { namespace, .. }
            | Self::ArgocdPause { namespace, .. }
            | Self::DeleteRunnerPod { namespace, .. } => vec![format!("ns/{namespace}")],
            Self::RabbitmqCloseConnections {
                broker_namespace,
                app_namespace,
                ..
            } => {
                vec![
                    format!("ns/{broker_namespace}"),
                    format!("ns/{app_namespace}"),
                ]
            }
            Self::CarveNamespace { name, .. } => vec![format!("ns/{name}")],
            Self::CarveNode { selector, .. } => vec![format!("node/{selector}")],
            Self::DispatchWorkflow { repo, .. } => vec![format!("repo/{repo}")],
        }
    }
}

/// Kinds a visita can run: each reads through the target's API from the
/// operator's machine. Every other kind creates a probe Job in the target.
pub const OPERATOR_KINDS: [AssertionKind; 7] = [
    AssertionKind::RolloutSurgeBound,
    AssertionKind::LogWindow,
    AssertionKind::MetricInRange,
    AssertionKind::HpaBehavior,
    AssertionKind::ExecMetric,
    AssertionKind::ObjectFields,
    AssertionKind::WorkflowJobs,
];

impl VisitaSpec {
    /// Resolve every relative manifest `:file` against `dir` — the directory
    /// of the spec file — so a spec means the same thing from any working
    /// directory.
    #[must_use]
    pub fn relative_to(mut self, dir: &std::path::Path) -> Self {
        let resolve = |a: &mut Option<Action>| {
            if let Some(Action::ApplyManifest { file: Some(f), .. }) = a
                && std::path::Path::new(f.as_str()).is_relative()
            {
                *f = dir.join(&*f).display().to_string();
            }
        };
        for f in &mut self.fixtures {
            resolve(&mut f.action);
            if let Cleanup::Inverse(inv) = &mut f.cleanup {
                resolve(&mut inv.action);
            }
        }
        self
    }

    pub fn fill_target_context(&mut self, ctx: &str) {
        for sel in &mut self.natural_state {
            if sel.context.is_empty() {
                sel.context = ctx.to_string();
            }
        }
        if let Some(g) = &mut self.guard
            && g.context.is_empty()
        {
            g.context = ctx.to_string();
        }
        for f in &mut self.fixtures {
            if let Some(a) = &mut f.action {
                a.fill_context(ctx);
            }
            if let Cleanup::Inverse(inv) = &mut f.cleanup
                && let Some(a) = &mut inv.action
            {
                a.fill_context(ctx);
            }
        }
        for a in &mut self.assertions {
            if a.operator.context.as_deref().is_none_or(str::is_empty) {
                a.operator.context = Some(ctx.to_string());
            }
        }
    }

    #[must_use]
    pub fn scope(&self) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = self
            .natural_state
            .iter()
            .map(|s| format!("ns/{}", s.namespace))
            .collect();
        for f in &self.fixtures {
            if let Some(a) = &f.action {
                out.extend(a.scope());
            }
        }
        out
    }

    /// Every kube context the trial touches, deduplicated.
    #[must_use]
    pub fn contexts(&self) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = self
            .natural_state
            .iter()
            .map(|s| s.context.clone())
            .collect();
        if let Some(g) = &self.guard {
            out.insert(g.context.clone());
        }
        let mut fixture_ctx = |f: &Fixture| {
            if let Some(a) = &f.action {
                out.extend(a.contexts().into_iter().map(str::to_string));
            }
        };
        for f in &self.fixtures {
            fixture_ctx(f);
            if let Cleanup::Inverse(inv) = &f.cleanup {
                fixture_ctx(inv);
            }
        }
        for a in &self.assertions {
            if let Some(c) = &a.operator.context {
                out.insert(c.clone());
            }
        }
        out
    }

    /// Everything decidable from the text, refused before anything runs.
    ///
    /// # Errors
    ///
    /// Every problem found, one per line — not just the first.
    pub fn validate(&self, allowed_contexts: &[String]) -> Result<(), String> {
        let mut errs: Vec<String> = Vec::new();

        if self.natural_state.is_empty() {
            errs.push(
                "no :natural-state — nothing to restore to, so nothing could be proven restored"
                    .into(),
            );
        }
        for c in self.contexts() {
            if c.is_empty() {
                errs.push(
                    "a :context is empty; name the context or give the spec a :target".into(),
                );
            } else if !allowed_contexts.contains(&c) {
                errs.push(format!(
                    "context `{c}` is not allowed; pass --allow-context {c} to name it as a target"
                ));
            }
        }
        if let Err(e) = placement_order(&self.fixtures) {
            errs.push(format!("fixtures: {e}"));
        }
        for f in &self.fixtures {
            self.validate_fixture(f, &mut errs);
            if let Some(Action::DeleteRunnerPod {
                namespace,
                run_of,
                job,
                wait,
                ..
            }) = &f.action
            {
                self.validate_delete_runner_pod(
                    f,
                    (namespace, run_of, job),
                    wait.as_deref(),
                    &mut errs,
                );
            }
        }
        let mut ids = BTreeSet::new();
        for a in &self.assertions {
            if !ids.insert(a.id.as_str()) {
                errs.push(format!("two assertions are both `{}`", a.id));
            }
            if !OPERATOR_KINDS.contains(&a.kind) {
                errs.push(format!(
                    "assertion `{}`: kind {:?} runs a probe inside the target; a visita creates nothing there",
                    a.id, a.kind
                ));
            }
            // Every parameter the kind reads is REQUIRED here. A key the
            // lisp→JSON bridge drops arrives as `None`, and without this check
            // it would surface mid-trial — after the environment was changed —
            // or, worse, as a default nobody chose.
            let o = &a.operator;
            let need: &[(&str, bool)] = match a.kind {
                AssertionKind::RolloutSurgeBound => &[
                    (":names", o.names.is_some()),
                    (":max-surge", o.max_surge.is_some()),
                    (":max-unavailable", o.max_unavailable.is_some()),
                ],
                AssertionKind::LogWindow => &[
                    (":selector", o.selector.is_some()),
                    (
                        ":patterns",
                        o.patterns.as_ref().is_some_and(|p| !p.is_empty()),
                    ),
                ],
                AssertionKind::MetricInRange => &[
                    (":selector", o.selector.is_some()),
                    (":port", o.port.is_some()),
                    (":metric", o.metric.is_some()),
                ],
                AssertionKind::HpaBehavior => &[
                    (":names", o.names.is_some()),
                    (":scale-up", o.scale_up.is_some()),
                ],
                AssertionKind::ExecMetric => &[
                    (":pod", o.pod.is_some()),
                    (":container", o.container.is_some()),
                    (
                        ":command",
                        a.command.as_ref().is_some_and(|c| !c.is_empty()),
                    ),
                    (":min or :max", o.min.is_some() || o.max.is_some()),
                ],
                AssertionKind::ObjectFields => &[
                    (":api-version", o.api_version.is_some()),
                    (":resource-kind", o.resource_kind.is_some()),
                    (":name", o.name.is_some()),
                    (":expect", o.expect.as_ref().is_some_and(|e| !e.is_empty())),
                ],
                AssertionKind::WorkflowJobs => &[
                    (":run-of", o.run_of.is_some()),
                    (":jobs", o.jobs.as_ref().is_some_and(|j| !j.is_empty())),
                ],
                _ => &[],
            };
            let mut missing: Vec<&str> =
                need.iter().filter(|(_, ok)| !ok).map(|(k, _)| *k).collect();
            if o.context.is_none() && a.kind != AssertionKind::WorkflowJobs {
                missing.insert(0, ":context");
            }
            if !missing.is_empty() {
                errs.push(format!(
                    "assertion `{}` ({:?}): missing {}",
                    a.id,
                    a.kind,
                    missing.join(", ")
                ));
            }
            if a.kind == AssertionKind::ObjectFields {
                validate_object_fields(a, &mut errs);
            }
            if a.kind == AssertionKind::WorkflowJobs {
                self.validate_workflow_jobs(a, &mut errs);
            }
        }
        let mut placed = BTreeSet::new();
        for s in &self.steps {
            match s {
                Step::Place(n) => {
                    if !self.fixtures.iter().any(|f| &f.name == n) {
                        errs.push(format!("step place `{n}`: no such fixture"));
                    }
                    if !placed.insert(n.as_str()) {
                        errs.push(format!(
                            "step place `{n}`: placed twice without an undo between"
                        ));
                    }
                }
                Step::Observe(id) => {
                    if !self.assertions.iter().any(|a| &a.id == id) {
                        errs.push(format!("step observe `{id}`: no such assertion"));
                    }
                }
                Step::Wait(d) => {
                    if humantime::parse_duration(d).is_err() {
                        errs.push(format!("step wait `{d}`: not a duration"));
                    }
                }
                Step::Undo(n) => {
                    if !placed.remove(n.as_str()) {
                        errs.push(format!("step undo `{n}`: not placed by an earlier step"));
                    }
                }
                Step::Phase(_) => {}
            }
        }
        for b in &self.branch_contains {
            let ok_repo = b
                .repo
                .split_once('/')
                .is_some_and(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/'));
            if !ok_repo || b.branch.is_empty() || b.trunk.is_empty() {
                errs.push(format!(
                    ":branch-contains {b:?}: needs :repo owner/name, :branch and :trunk"
                ));
            }
        }
        for id in &self.preconditions {
            if !self.assertions.iter().any(|a| &a.id == id) {
                errs.push(format!(":preconditions `{id}`: no such assertion"));
            }
            if !self
                .steps
                .iter()
                .any(|s| matches!(s, Step::Observe(o) if o == id))
            {
                errs.push(format!(
                    ":preconditions `{id}`: never observed, so it could never hold"
                ));
            }
        }
        if let Some(g) = &self.guard {
            if g.pods.is_empty() {
                errs.push(":guard watches no pods".into());
            }
            for d in std::iter::once(&g.max_unready).chain(g.interval.iter()) {
                if humantime::parse_duration(d).is_err() {
                    errs.push(format!(":guard `{d}`: not a duration"));
                }
            }
        }
        for a in &self.assertions {
            let Some(rf) = &a.operator.record_from else {
                continue;
            };
            if !matches!(
                a.kind,
                AssertionKind::RolloutSurgeBound | AssertionKind::ExecMetric
            ) {
                errs.push(format!(
                    "assertion `{}`: :record-from applies only to RolloutSurgeBound and ExecMetric",
                    a.id
                ));
            }
            let placed_at = self
                .steps
                .iter()
                .position(|s| matches!(s, Step::Place(n) if n == rf));
            let observed_at = self
                .steps
                .iter()
                .position(|s| matches!(s, Step::Observe(id) if id == &a.id));
            match (placed_at, observed_at) {
                (Some(p), Some(o)) if p < o => {}
                (None, _) => errs.push(format!(
                    "assertion `{}`: :record-from `{rf}` is never placed, so the record would never start",
                    a.id
                )),
                _ => errs.push(format!(
                    "assertion `{}`: :record-from `{rf}` must be placed before the assertion is observed",
                    a.id
                )),
            }
        }
        for n in &self.restore {
            if !self.fixtures.iter().any(|f| &f.name == n) {
                errs.push(format!(":restore `{n}`: no such fixture"));
            }
        }
        if let Some(s) = &self.settle {
            if humantime::parse_duration(s).is_err() {
                errs.push(format!(":settle `{s}`: not a duration"));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("\n"))
        }
    }

    fn validate_fixture(&self, f: &Fixture, errs: &mut Vec<String>) {
        let Some(action) = &f.action else {
            errs.push(format!("fixture `{}`: no :action", f.name));
            return;
        };
        match &f.cleanup {
            Cleanup::EnvironmentIsDestroyed => errs.push(format!(
                "fixture `{}`: environment-is-destroyed — a borrowed environment is never destroyed, so this would leave the change behind",
                f.name
            )),
            Cleanup::RestoreFromSnapshot => {
                if !action.snapshot_invertible() {
                    errs.push(format!(
                        "fixture `{}`: restore-from-snapshot cannot invert this action; declare (:inverse …)",
                        f.name
                    ));
                }
                if let Some((ctx, ns, _, name)) = action.target() {
                    let covered = self.natural_state.iter().any(|s| {
                        s.context == ctx
                            && s.namespace == ns
                            && (s.names.is_empty()
                                || s.names.iter().any(|p| super::snapshot::glob(p, name)))
                    });
                    if !covered {
                        errs.push(format!(
                            "fixture `{}`: restore-from-snapshot on {ctx}/{ns}/{name}, which no :natural-state selection captures",
                            f.name
                        ));
                    }
                }
            }
            Cleanup::Inverse(inv) => {
                if inv.action.is_none() {
                    errs.push(format!("fixture `{}`: its inverse has no :action", f.name));
                }
            }
            Cleanup::Transient { .. } => {
                if matches!(f.effect, Effect::Create) {
                    errs.push(format!(
                        "fixture `{}`: a created object is not transient; it stays until removed",
                        f.name
                    ));
                }
            }
            Cleanup::Retained { .. } | Cleanup::OwnInverse => {}
            Cleanup::Stop => {
                if !matches!(action, Action::DispatchWorkflow { .. }) {
                    errs.push(format!("fixture `{}`: stop applies only to a dispatched run", f.name));
                }
            }
        }
        if matches!(action, Action::ResizePod { .. })
            && !matches!(f.cleanup, Cleanup::RestoreFromSnapshot)
        {
            errs.push(format!(
                "fixture `{}`: an in-place resize must declare :cleanup restore-from-snapshot — its inverse is the pod's captured resources",
                f.name
            ));
        }
        if matches!(action, Action::RolloutRestart { .. })
            && !matches!(f.cleanup, Cleanup::Transient { .. })
        {
            errs.push(format!(
                "fixture `{}`: a rollout restart has no inverse; declare it (:transient (:reason …))",
                f.name
            ));
        }
        if let Action::ResizePod {
            cpu_limit,
            cpu_request,
            timeout,
            ..
        } = action
        {
            if cpu_limit.is_none() && cpu_request.is_none() {
                errs.push(format!(
                    "fixture `{}`: resize-pod changes nothing (no :cpu-limit or :cpu-request)",
                    f.name
                ));
            }
            for q in [cpu_limit, cpu_request].into_iter().flatten() {
                if breathe_resize_parse_cpu(q).is_none() {
                    errs.push(format!("fixture `{}`: `{q}` is not a cpu quantity", f.name));
                }
            }
            if let Some(d) = timeout {
                if humantime::parse_duration(d).is_err() {
                    errs.push(format!("fixture `{}`: `{d}` is not a duration", f.name));
                }
            }
        }
        if matches!(action, Action::DispatchWorkflow { .. }) && !matches!(f.cleanup, Cleanup::Stop)
        {
            errs.push(format!(
                "fixture `{}`: a dispatched run must declare :cleanup stop — a run left in flight outlives the trial and can undo the restore",
                f.name
            ));
        }
        validate_own_inverse(f, action, errs);
        if !f.undo_phase.is_in_order()
            && !matches!(
                f.cleanup,
                Cleanup::OwnInverse | Cleanup::Inverse(_) | Cleanup::RestoreFromSnapshot
            )
        {
            errs.push(format!(
                "fixture `{}`: :undo-phase after-restore defers an undo, and this cleanup has none to defer (a run is stopped before anything; a transient or retained change is never undone)",
                f.name
            ));
        }
        if let Action::DispatchWorkflow { timeout, poll, .. } = action {
            for d in [timeout, poll].into_iter().flatten() {
                if humantime::parse_duration(d).is_err() {
                    errs.push(format!("fixture `{}`: `{d}` is not a duration", f.name));
                }
            }
        }
    }
}

const CONCLUSIONS: [&str; 7] = [
    "success",
    "failure",
    "cancelled",
    "skipped",
    "timed_out",
    "neutral",
    "action_required",
];

impl VisitaSpec {
    fn dispatch_fixture(&self, name: &str) -> Option<&Fixture> {
        self.fixtures
            .iter()
            .find(|f| f.name == name && matches!(f.action, Some(Action::DispatchWorkflow { .. })))
    }

    fn validate_delete_runner_pod(
        &self,
        f: &Fixture,
        (namespace, run_of, job): (&str, &str, &str),
        wait: Option<&str>,
        errs: &mut Vec<String>,
    ) {
        let name = &f.name;
        if let Some(d) = wait
            && humantime::parse_duration(d).is_err()
        {
            errs.push(format!("fixture `{name}`: `{d}` is not a duration"));
        }
        if !matches!(f.cleanup, Cleanup::Transient { .. }) {
            errs.push(format!(
                "fixture `{name}`: a deleted runner pod has no inverse (its controller replaces it); declare (:transient (:reason …))"
            ));
        }
        if !matches!(f.effect, Effect::Mutate) {
            errs.push(format!(
                "fixture `{name}`: delete-runner-pod changes a running job; declare :effect mutate"
            ));
        }
        if namespace.is_empty() || job.is_empty() {
            errs.push(format!(
                "fixture `{name}`: delete-runner-pod needs :namespace and :job"
            ));
        }
        if self.dispatch_fixture(run_of).is_none() {
            errs.push(format!(
                "fixture `{name}`: :run-of `{run_of}` is not a dispatch-workflow fixture, so the pod to delete could not be tied to our own run"
            ));
        }
        let placed = |n: &str| {
            self.steps
                .iter()
                .position(|s| matches!(s, Step::Place(p) if p == n))
        };
        match (placed(run_of), placed(name)) {
            (Some(d), Some(k)) if d < k => {}
            (_, None) => {}
            _ => errs.push(format!(
                "fixture `{name}`: :run-of `{run_of}` must be placed before it"
            )),
        }
    }

    fn validate_workflow_jobs(&self, a: &AssertionSpec, errs: &mut Vec<String>) {
        let o = &a.operator;
        let id = &a.id;
        if let Some(rf) = &o.run_of {
            if self.dispatch_fixture(rf).is_none() {
                errs.push(format!(
                    "assertion `{id}`: :run-of `{rf}` is not a dispatch-workflow fixture"
                ));
            }
            let placed_at = self
                .steps
                .iter()
                .position(|s| matches!(s, Step::Place(n) if n == rf));
            let observed_at = self
                .steps
                .iter()
                .position(|s| matches!(s, Step::Observe(x) if x == id));
            if let (Some(p), Some(ob)) = (placed_at, observed_at)
                && p > ob
            {
                errs.push(format!("assertion `{id}`: :run-of `{rf}` must be placed before the assertion is observed"));
            }
            if placed_at.is_none() {
                errs.push(format!(
                    "assertion `{id}`: :run-of `{rf}` is never placed, so there is no run to read"
                ));
            }
        }
        for j in o.jobs.iter().flatten() {
            if j.job.trim().is_empty() {
                errs.push(format!("assertion `{id}`: a :jobs entry names no :job"));
            }
            let checks = j.conclusion.is_some()
                || j.log_contains.as_ref().is_some_and(|v| !v.is_empty())
                || j.annotation_contains
                    .as_ref()
                    .is_some_and(|v| !v.is_empty());
            if !checks {
                errs.push(format!(
                    "assertion `{id}`: job `{}` expects nothing (:conclusion, :log-contains or :annotation-contains)",
                    j.job
                ));
            }
            if let Some(c) = &j.conclusion
                && !CONCLUSIONS.contains(&c.as_str())
            {
                errs.push(format!(
                    "assertion `{id}`: job `{}`: `{c}` is not a job conclusion",
                    j.job
                ));
            }
            for t in j
                .log_contains
                .iter()
                .flatten()
                .chain(j.annotation_contains.iter().flatten())
            {
                if t.is_empty() {
                    errs.push(format!(
                        "assertion `{id}`: job `{}`: an empty text matches everything",
                        j.job
                    ));
                }
            }
        }
        for d in [&o.timeout, &o.interval].into_iter().flatten() {
            if humantime::parse_duration(d).is_err() {
                errs.push(format!("assertion `{id}`: `{d}` is not a duration"));
            }
        }
    }
}

/// The rules of the actions that carry their own undo, and of
/// [`Cleanup::OwnInverse`], which only they may declare.
fn validate_own_inverse(f: &Fixture, action: &Action, errs: &mut Vec<String>) {
    let carries_own = matches!(
        action,
        Action::ApplyManifest { .. }
            | Action::Exec { .. }
            | Action::CarveNamespace { .. }
            | Action::Fence { .. }
            | Action::CarveNode { .. }
            | Action::ArgocdPause { .. }
    );
    if matches!(f.cleanup, Cleanup::OwnInverse) && !carries_own {
        errs.push(format!(
            "fixture `{}`: own-inverse applies only to an action that carries its own undo (apply-manifest, an exec with :undo-argv, carve-namespace, fence, carve-node, argocd-pause)",
            f.name
        ));
    }
    validate_carve(f, action, errs);
    if let Action::Exec {
        pod,
        selector,
        all,
        argv,
        undo_argv,
        ..
    } = action
    {
        validate_exec(
            f,
            pod.is_some(),
            selector.is_some(),
            *all,
            argv,
            undo_argv.as_deref(),
            errs,
        );
    }
    if let Action::ApplyManifest {
        namespace,
        file,
        yaml,
        timeout,
        ..
    } = action
    {
        if !matches!(f.cleanup, Cleanup::OwnInverse) {
            errs.push(format!(
                "fixture `{}`: an applied manifest must declare :cleanup own-inverse — its undo deletes exactly what it created",
                f.name
            ));
        }
        if !matches!(f.effect, Effect::Create) {
            errs.push(format!(
                "fixture `{}`: apply-manifest creates; declare :effect create",
                f.name
            ));
        }
        if let Err(e) = super::manifest::source(file.as_deref(), yaml.as_deref())
            .and_then(|text| super::manifest::objects(&text, namespace))
        {
            errs.push(format!("fixture `{}`: {e}", f.name));
        }
        if let Some(d) = timeout
            && humantime::parse_duration(d).is_err()
        {
            errs.push(format!("fixture `{}`: `{d}` is not a duration", f.name));
        }
    }
}

fn validate_carve(f: &Fixture, action: &Action, errs: &mut Vec<String>) {
    let name = &f.name;
    let (word, creates) = match action {
        Action::CarveNamespace { .. } => ("carve-namespace", true),
        Action::Fence { .. } => ("fence", true),
        Action::CarveNode { .. } => ("carve-node", false),
        Action::ArgocdPause { .. } => ("argocd-pause", false),
        _ => return,
    };
    if !matches!(f.cleanup, Cleanup::OwnInverse) {
        errs.push(format!(
            "fixture `{name}`: {word} must declare :cleanup own-inverse — its undo removes exactly what it added"
        ));
    }
    match (creates, &f.effect) {
        (true, Effect::Create) | (false, Effect::Mutate) => {}
        (true, _) => errs.push(format!(
            "fixture `{name}`: {word} creates; declare :effect create"
        )),
        (false, _) => errs.push(format!(
            "fixture `{name}`: {word} changes an existing object; declare :effect mutate"
        )),
    }
    let dns_label = |s: &str| {
        !s.is_empty()
            && s.len() <= 63
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !s.starts_with('-')
            && !s.ends_with('-')
    };
    match action {
        Action::CarveNamespace {
            name: ns,
            quota,
            limit_default,
            limit_default_request,
            timeout,
            ..
        } => {
            if !dns_label(ns) {
                errs.push(format!(
                    "fixture `{name}`: namespace `{ns}` is not a DNS label"
                ));
            }
            for q in quota
                .iter()
                .chain(limit_default)
                .chain(limit_default_request)
            {
                if q.name.is_empty() || q.value.is_empty() {
                    errs.push(format!(
                        "fixture `{name}`: a quantity needs :name and :value"
                    ));
                }
            }
            if let Some(d) = timeout
                && humantime::parse_duration(d).is_err()
            {
                errs.push(format!("fixture `{name}`: `{d}` is not a duration"));
            }
        }
        Action::Fence {
            egress,
            ingress,
            timeout,
            ..
        } => {
            for p in egress.iter().chain(ingress) {
                if let Peer::Service {
                    namespace,
                    name: svc,
                } = p
                    && (namespace.is_empty() || svc.is_empty())
                {
                    errs.push(format!(
                        "fixture `{name}`: a :service peer needs :namespace and :name"
                    ));
                }
            }
            if ingress.iter().any(|p| matches!(p, Peer::ApiServer)) {
                errs.push(format!("fixture `{name}`: api-server is an egress peer; the API server is not a pod a policy can admit"));
            }
            if let Some(d) = timeout
                && humantime::parse_duration(d).is_err()
            {
                errs.push(format!("fixture `{name}`: `{d}` is not a duration"));
            }
        }
        Action::CarveNode {
            selector,
            taint_key,
            label_key,
            label_value,
            ..
        } => {
            if selector.is_empty()
                || taint_key.is_empty()
                || label_key.is_empty()
                || label_value.is_empty()
            {
                errs.push(format!("fixture `{name}`: carve-node needs :selector, :taint-key, :label-key and :label-value"));
            }
        }
        Action::ArgocdPause { application, .. } => {
            if application.is_empty() {
                errs.push(format!(
                    "fixture `{name}`: argocd-pause names no :application"
                ));
            }
        }
        _ => {}
    }
}

fn validate_exec(
    f: &Fixture,
    pod: bool,
    selector: bool,
    all: bool,
    argv: &[String],
    undo_argv: Option<&[String]>,
    errs: &mut Vec<String>,
) {
    let name = &f.name;
    if pod == selector {
        errs.push(format!(
            "fixture `{name}`: an exec names exactly one of :pod or :selector"
        ));
    }
    if all && pod {
        errs.push(format!(
            "fixture `{name}`: :all applies to a :selector; a :pod is one pod"
        ));
    }
    if argv.is_empty() {
        errs.push(format!("fixture `{name}`: :argv is empty"));
    }
    match (undo_argv, &f.cleanup) {
        (Some([]), _) => errs.push(format!("fixture `{name}`: :undo-argv is empty")),
        (Some(_), Cleanup::OwnInverse) | (None, Cleanup::Transient { .. }) => {}
        (Some(_), _) => errs.push(format!(
            "fixture `{name}`: an exec with :undo-argv must declare :cleanup own-inverse, or its undo would never run"
        )),
        (None, _) => errs.push(format!(
            "fixture `{name}`: an exec without :undo-argv cannot be undone; declare it (:transient (:reason …)) or give the :undo-argv"
        )),
    }
}

fn validate_object_fields(a: &crate::types::AssertionSpec, errs: &mut Vec<String>) {
    let o = &a.operator;
    if let Some(v) = &o.api_version {
        let parts: Vec<&str> = v.split('/').collect();
        if parts.len() > 2
            || parts
                .iter()
                .any(|p| p.is_empty() || p.contains(char::is_whitespace))
        {
            errs.push(format!(
                "assertion `{}`: :api-version `{v}` is not `group/version` or `version`",
                a.id
            ));
        }
    }
    for (key, d) in [(":timeout", &o.timeout), (":interval", &o.interval)] {
        if let Some(d) = d
            && humantime::parse_duration(d).is_err()
        {
            errs.push(format!(
                "assertion `{}`: {key} `{d}` is not a duration",
                a.id
            ));
        }
    }
    if o.wait_until.as_ref().is_some_and(Vec::is_empty) {
        errs.push(format!(
            "assertion `{}`: :wait-until is empty; omit it to judge the first answer",
            a.id
        ));
    }
}

/// Millicores of a k8s cpu quantity, by breathe's one parser.
#[must_use]
pub fn breathe_resize_parse_cpu(q: &str) -> Option<u64> {
    breathe_resize::parse("cpu", q)
}
