//! The `Ensaio` reconcile loop — the idiomatic kube-rs `Controller`
//! (event-driven watch + reconcile + typed `Action` requeue), a
//! deliberate upgrade beyond Autorevivy's own hand-rolled poll-and-sweep
//! (see `crd.rs`'s module doc). Drives `status.phase` toward a
//! **terminal** state (`Done`/`Failed`/`TtlExceeded`) — never an
//! indefinite tick.
//!
//! Wrapped in `kube::runtime::finalizer` — a deleted-mid-flight
//! `Ensaio` gets a guaranteed cleanup pass (every provisioned system
//! scaled to zero) before Kubernetes actually removes the object.
//!
//! M2 scope, honestly bounded: `Dormant`'s "operator wakes a settled
//! run back up" path is a type the FSM carries but this pass's
//! reconciler doesn't drive automatically — the automatic flow's
//! success terminal is `Done` (which scales every provisioned system
//! to zero as part of the transition, per the scale-to-zero-not-delete
//! correction).

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event as FinalizerEvent, finalizer};
use kube::runtime::watcher::Config as WatcherConfig;
use kube::{Client, Resource, ResourceExt};

// `StepOutcome` and `ControllerError` now live in the shared `utsuroi-lifecycle`
// spine (they were the one piece of genuinely-shared code — viveiro already
// imported them from here). Re-exported so every existing
// `use crate::controller::{ControllerError, StepOutcome}` — ensaio's own tests,
// and viveiro's controller — keeps resolving unchanged.
pub use utsuroi_lifecycle::{ControllerError, StepOutcome};

use crate::crd::{Ensaio, EnsaioCrdSpec, EnsaioPhase, EnsaioStatus};
use crate::interp::EnsaioEnvironment;
use crate::k8s_env::K8sEnvironment;
use crate::nervous::PhaseEvent;
use crate::types::Verdict;

const REQUEUE_ACTIVE: Duration = Duration::from_secs(5);
const REQUEUE_IDLE: Duration = Duration::from_secs(300);
const FINALIZER_NAME: &str = "ensaio.pleme.io/cleanup";

/// Render an error together with its full `source` chain.
///
/// **Why this exists — an error that names nothing is not an error report.**
/// Measured 2026-08-10: a `WarmEnvironment` failed to reconcile every 15s for
/// eight minutes and every single log line read, in full:
///
/// ```text
/// warmenvironment reconcile error error=reconciler for object
///   Trial.v1alpha1.example.pleme.io/rd1.trials failed
/// ```
///
/// That is `kube::runtime::controller::Error`'s `Display`, which names the
/// object and the fact of failure and **drops the `source`** — where the
/// actual cause (an apiserver rejection, a typed step error, a serialization
/// fault) lives. Three call sites logged `error = %e` and so threw the cause
/// away three times; the operator was left to guess, and guessed wrong twice
/// (a CRD schema mismatch, then missing RBAC) before reaching for this.
///
/// `%e` on ANY `std::error::Error` has this defect — it is a property of
/// `Display`, not of kube-rs. So the fix is a helper every controller log site
/// uses rather than a wider match arm at one of them: the next controller to
/// log an error inherits the chain instead of rediscovering the gap.
///
/// TIER: this is diagnosis, not prevention — it cannot stop a reconcile
/// failing, it makes the failure *say what it was*. Bounded at 8 links so a
/// cyclic or pathological chain cannot flood the log.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    let mut depth = 0;
    while let Some(cause) = src {
        if depth >= 8 {
            out.push_str(" -> ...(chain truncated at 8)");
            break;
        }
        out.push_str(" -> ");
        out.push_str(&cause.to_string());
        src = cause.source();
        depth += 1;
    }
    out
}

pub const SCOPE_ENV: &str = "ENSAIO_SCOPE";

pub struct Context {
    pub client: Client,
    pub scope: String,
    /// `ServiceAccount` every `CustomJob` probe pod runs as, threaded
    /// into [`K8sEnvironment::with_probe_service_account`]. Set via the
    /// `ENSAIO_PROBE_SERVICE_ACCOUNT` env var (the chart's Deployment
    /// sets it to the controller's own SA name — see
    /// `charts/ensaio/templates/deployment.yaml`); `None` leaves probe
    /// pods on the namespace's `default` SA.
    pub probe_service_account: Option<String>,
}

/// Start the controller. Runs until the watch stream ends (normally:
/// forever, or until the process is signalled).
///
/// # Errors
///
/// Refuses to start when [`SCOPE_ENV`] is unset or empty: a controller with no
/// scope gate would reconcile every `Ensaio` in the cluster.
pub async fn run(client: Client) -> anyhow::Result<()> {
    let scope = scope_from_env()?;
    let ensaios: Api<Ensaio> = Api::all(client.clone());
    let probe_service_account = std::env::var("ENSAIO_PROBE_SERVICE_ACCOUNT").ok();
    tracing::info!(%scope, "ensaio-controller starting");
    let ctx = Arc::new(Context {
        client,
        scope,
        probe_service_account,
    });

    Controller::new(ensaios, WatcherConfig::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(o) => tracing::debug!(?o, "reconciled"),
                Err(e) => tracing::warn!(error = %error_chain(&e), "reconcile error"),
            }
        })
        .await;
    Ok(())
}

/// The scope gate this controller enforces, read from [`SCOPE_ENV`].
///
/// # Errors
///
/// When the variable is unset or blank.
pub fn scope_from_env() -> anyhow::Result<String> {
    scope_from(std::env::var(SCOPE_ENV).ok())
}

fn scope_from(raw: Option<String>) -> anyhow::Result<String> {
    match raw.map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() => Ok(s),
        _ => anyhow::bail!(
            "{SCOPE_ENV} is unset: the controller has no scope gate and refuses to start"
        ),
    }
}

fn error_policy(
    _obj: Arc<Ensaio>,
    _err: &kube::runtime::finalizer::Error<ControllerError>,
    _ctx: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(15))
}

async fn reconcile(
    obj: Arc<Ensaio>,
    ctx: Arc<Context>,
) -> Result<Action, kube::runtime::finalizer::Error<ControllerError>> {
    let ns = obj.namespace().unwrap_or_default();
    let api: Api<Ensaio> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&api, FINALIZER_NAME, obj, |event| async {
        match event {
            FinalizerEvent::Apply(obj) => apply_reconcile(obj, ctx.clone()).await,
            FinalizerEvent::Cleanup(obj) => cleanup(obj, ctx.clone()).await,
        }
    })
    .await
}

/// Advance `status` by exactly one phase-FSM step, dispatching every
/// side effect (provision/wait-ready/wire/run-assertion/teardown)
/// through `env` and firing exactly one typed [`PhaseEvent`] publish per
/// transition committed (the reactive-nervous-system extension point).
///
/// Generic over `E: EnsaioEnvironment` — no kube-rs/tokio dependency —
/// so this is the unit-testable core of the M2 reconcile loop
/// (`tests/controller_phase_test.rs` drives it directly with `MockEnv`
/// to prove the publish fires at the right points, the same way the
/// crate already tests provision/wire/assert/teardown). The real
/// controller ([`apply_reconcile`], below) is the only production
/// caller, always with a concrete [`K8sEnvironment`].
///
/// # Errors
///
/// `ControllerError::Rig` only for the one path that already hard-fails
/// today (a wiring edge that can't be applied) — preserved unchanged
/// from the pre-refactor `apply_reconcile` behavior: that failure never
/// touches `status` and never fires a publish, since no transition is
/// actually committed.
pub fn step_phase<E: EnsaioEnvironment>(
    env: &E,
    ns: &str,
    name: &str,
    spec: &EnsaioCrdSpec,
    status: &mut EnsaioStatus,
) -> Result<StepOutcome, ControllerError> {
    match status.phase {
        EnsaioPhase::Pending => {
            for sys in &spec.systems {
                match env.provision_system(sys) {
                    Ok(endpoint) => {
                        status.systems_provisioned.push(sys.id.clone());
                        tracing::info!(system = %sys.id, %endpoint, "provisioned");
                    }
                    Err(e) => {
                        status.phase = EnsaioPhase::Failed;
                        status.message = Some(format!("provision {}: {e}", sys.id));
                        publish_phase_event(env, ns, name, status);
                        return Ok(StepOutcome::AwaitChange);
                    }
                }
            }
            status.phase = EnsaioPhase::Provisioning;
        }
        EnsaioPhase::Provisioning => {
            let all_ready = spec.systems.iter().all(|sys| {
                let endpoint = format!("{}.{}.svc.cluster.local", sys.id, ns);
                env.wait_ready(&endpoint).is_ok()
            });
            status.phase = if all_ready {
                EnsaioPhase::Ready
            } else {
                EnsaioPhase::Provisioning
            };
        }
        EnsaioPhase::Ready => {
            for w in &spec.wiring {
                env.wire(w).map_err(ControllerError::Rig)?;
            }
            status.phase = EnsaioPhase::Wiring;
        }
        EnsaioPhase::Wiring => {
            status.phase = EnsaioPhase::Asserting;
        }
        EnsaioPhase::Asserting => {
            let mut results = Vec::new();
            for a in &spec.assertions {
                let verdict = env
                    .run_assertion(a)
                    .unwrap_or_else(|e| Verdict::Errored { message: e });
                results.push(crate::types::AssertionResult {
                    assertion_id: a.id.clone(),
                    kind: a.kind,
                    verdict,
                });
            }
            status.results = results;
            status.confirmations += 1;
            status.phase = EnsaioPhase::Settled;
        }
        EnsaioPhase::Settled => {
            if status.confirmations < spec.confirm_after_runs {
                status.phase = EnsaioPhase::Asserting;
            } else {
                teardown_all(env, status, ns);
                status.phase = EnsaioPhase::Done;
                status.message = Some(format!(
                    "{}/{} confirmations complete",
                    status.confirmations, spec.confirm_after_runs
                ));
            }
        }
        EnsaioPhase::Dormant
        | EnsaioPhase::Done
        | EnsaioPhase::Failed
        | EnsaioPhase::TtlExceeded
        | EnsaioPhase::Retired => {
            publish_phase_event(env, ns, name, status);
            return Ok(StepOutcome::AwaitChange);
        }
    }

    publish_phase_event(env, ns, name, status);
    let requeue = if status.phase.is_terminal() {
        REQUEUE_IDLE
    } else {
        REQUEUE_ACTIVE
    };
    Ok(StepOutcome::Requeue(requeue))
}

/// Build a [`PhaseEvent`] from the current status and publish it through
/// `env` — the one call site every phase-transition commit routes
/// through (`step_phase`'s four commit points, plus the TTL-exceeded
/// early exit in `apply_reconcile`). A publish failure is logged and
/// swallowed, never propagated: the reactive nervous system is an
/// observability fan-out, not a correctness dependency of the reconcile
/// loop (mirrors `EnsaioEnvironment::record`'s own no-fatal-effect
/// framing).
fn publish_phase_event<E: EnsaioEnvironment>(env: &E, ns: &str, name: &str, status: &EnsaioStatus) {
    let event = PhaseEvent {
        run: name.to_string(),
        namespace: ns.to_string(),
        phase: status.phase,
        message: status.message.clone(),
    };
    if let Err(e) = env.publish_phase_event(&event) {
        tracing::warn!(
            run = %name,
            phase = ?status.phase,
            error = %e,
            "reactive nervous system publish failed (non-fatal)"
        );
    }
}

/// Build a `K8sEnvironment` for one namespace, carrying the context's
/// configured probe `ServiceAccount` (if any) — the single place both
/// `apply_reconcile` and `cleanup` construct an environment, so the two
/// call sites can't drift on this wiring.
fn env_for(ctx: &Context, ns: &str) -> K8sEnvironment {
    let env = K8sEnvironment::new(ctx.client.clone(), ns.to_string());
    match &ctx.probe_service_account {
        Some(sa) => env.with_probe_service_account(sa.clone()),
        None => env,
    }
}

/// The normal reconcile path — drives `status.phase` forward one step.
async fn apply_reconcile(obj: Arc<Ensaio>, ctx: Arc<Context>) -> Result<Action, ControllerError> {
    let ns = obj.namespace().unwrap_or_default();
    let name = obj.name_any();

    // Scope gate — the same runtime check Autorevivy applies, defense
    // in depth alongside RBAC (the controller's ClusterRole grants write
    // only inside the deployment's own namespaces).
    if obj.spec.scope != ctx.scope {
        tracing::warn!(%name, scope = %obj.spec.scope, gate = %ctx.scope, "ignoring Ensaio outside the scope gate");
        return Ok(Action::await_change());
    }

    let mut status = obj.status.clone().unwrap_or_default();
    let env = env_for(&ctx, &ns);

    if !status.phase.is_terminal() && ttl_exceeded(&obj) {
        status.phase = EnsaioPhase::TtlExceeded;
        status.message = Some("TTL exceeded before reaching a terminal state".into());
        publish_phase_event(&env, &ns, &name, &status);
        patch_status(&ctx.client, &ns, &name, &status).await?;
        return Ok(Action::await_change());
    }

    let outcome = step_phase(&env, &ns, &name, &obj.spec, &mut status)?;
    patch_status(&ctx.client, &ns, &name, &status).await?;
    Ok(match outcome {
        StepOutcome::AwaitChange => Action::await_change(),
        StepOutcome::Requeue(d) => Action::requeue(d),
    })
}

/// The finalizer's cleanup path — runs exactly once, guaranteed,
/// before Kubernetes removes a deleted `Ensaio`. Tears down (scales to
/// zero) every provisioned system regardless of which phase the run
/// was in when it was deleted.
// `async` is required by the finalizer closure's uniform arm type even though
// this path only does synchronous env teardown (no `await`).
#[allow(clippy::unused_async)]
async fn cleanup(obj: Arc<Ensaio>, ctx: Arc<Context>) -> Result<Action, ControllerError> {
    let ns = obj.namespace().unwrap_or_default();
    let name = obj.name_any();
    let env = env_for(&ctx, &ns);

    let provisioned = obj
        .status
        .as_ref()
        .map(|s| s.systems_provisioned.clone())
        .unwrap_or_default();
    for sys_id in provisioned {
        let endpoint = format!("{sys_id}.{ns}.svc.cluster.local");
        if let Err(e) = env.teardown_system(&endpoint) {
            tracing::warn!(%name, system = %sys_id, error = %e, "cleanup teardown failed");
        }
    }
    Ok(Action::await_change())
}

fn teardown_all<E: EnsaioEnvironment>(env: &E, status: &mut EnsaioStatus, ns: &str) {
    for sys_id in status.systems_provisioned.clone() {
        let endpoint = format!("{sys_id}.{ns}.svc.cluster.local");
        if let Err(e) = env.teardown_system(&endpoint) {
            tracing::warn!(system = %sys_id, error = %e, "teardown (scale-to-zero) failed");
        }
    }
}

async fn patch_status(
    client: &Client,
    ns: &str,
    name: &str,
    status: &EnsaioStatus,
) -> Result<(), ControllerError> {
    let api: Api<Ensaio> = Api::namespaced(client.clone(), ns);
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

fn ttl_exceeded(obj: &Ensaio) -> bool {
    let Some(created) = obj.meta().creation_timestamp.as_ref() else {
        return false;
    };
    let Ok(ttl) = humantime::parse_duration(&obj.spec.ttl) else {
        tracing::warn!(ttl = %obj.spec.ttl, "unparseable ttl, treating as never-exceeded");
        return false;
    };
    let Ok(ttl) = chrono::Duration::from_std(ttl) else {
        return false;
    };
    chrono::Utc::now() > created.0 + ttl
}

#[cfg(test)]
mod error_chain_tests {
    use super::{error_chain, scope_from};
    use std::error::Error;
    use std::fmt;

    #[derive(Debug)]
    struct Link(&'static str, Option<Box<Link>>);
    impl fmt::Display for Link {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl Error for Link {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.1
                .as_ref()
                .map(|b| b.as_ref() as &(dyn Error + 'static))
        }
    }

    /// The exact defect measured live: the OUTER message alone names the
    /// object and hides the cause. `error_chain` must surface the cause.
    #[test]
    fn the_cause_is_reported_not_just_the_outer_message() {
        let e = Link(
            "reconciler for object Trial/rd1.trials failed",
            Some(Box::new(Link("ApiError: pods is forbidden", None))),
        );
        // Red-run witness: this is what `%e` produced, and it is NOT enough.
        assert!(!e.to_string().contains("forbidden"));
        let rendered = error_chain(&e);
        assert!(rendered.contains("forbidden"), "cause dropped: {rendered}");
        assert!(rendered.contains("rd1.trials"), "outer dropped: {rendered}");
    }

    #[test]
    fn the_scope_gate_is_refused_when_unset_or_blank() {
        assert!(scope_from(None).is_err());
        assert!(scope_from(Some("  ".into())).is_err());
        assert_eq!(scope_from(Some(" trials ".into())).unwrap(), "trials");
    }

    #[test]
    fn a_lone_error_renders_without_an_arrow() {
        assert_eq!(error_chain(&Link("solo", None)), "solo");
    }

    /// The bound is the point: a pathological chain must not flood the log.
    #[test]
    fn the_chain_is_bounded_at_eight_links() {
        let mut cur = Link("deepest", None);
        for _ in 0..40 {
            cur = Link("link", Some(Box::new(cur)));
        }
        let rendered = error_chain(&cur);
        assert!(rendered.contains("truncated at 8"), "unbounded: {rendered}");
        assert_eq!(rendered.matches(" -> ").count(), 9, "{rendered}");
    }
}
