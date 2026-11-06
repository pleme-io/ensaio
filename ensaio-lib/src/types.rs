//! Typed border for Ensaio — a typed architectural-trial rig.
//!
//! Declares N ephemeral systems, wires them, runs a typed catalog of
//! assertions, records a verdict, tears down.  Every future case
//! (M2M-vs-Authservice today, whatever else tomorrow) is a new
//! `(defensaio ...)` form and, where a new assertion shape is needed,
//! a new [`AssertionKind`] variant + interpreter arm — never a
//! hand-rolled controller.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defensaio
//!   :name "admission-gate"
//!   :scope "trials"
//!   :ttl "30m"
//!   :claim "Does an Authservice-shaped gate admit a machine call with
//!           no human SSO session, before the backend evaluates its
//!           own access-token proof?"
//!   :systems ((:id "authservice-mock" :template "envoy-authservice-minimal" :breathe #t)
//!             (:id "backend-stub" :template "backend-minimal" :breathe #t))
//!   :wiring ((:from "authservice-mock" :to "backend-stub" :via "cluster-dns"))
//!   :assertions ((:id "m2m-admitted-without-sso"
//!                 :kind AuthserviceAdmission
//!                 :target "backend-stub"
//!                 :proof-channel "access-token-field"
//!                 :session "none"))
//!   :record #t)
//! ```

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::field::FieldPredicate;

// ── Typed border ───────────────────────────────────────────────────

/// One architectural trial: N systems, a wiring edge, a catalog of
/// assertions.  The top-level authored form.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defensaio")]
pub struct EnsaioSpec {
    pub name: String,
    pub scope: String,
    pub ttl: String,
    pub claim: String,
    #[serde(default)]
    pub confluence_ref: Option<String>,
    pub systems: Vec<SystemSpec>,
    #[serde(default)]
    pub wiring: Vec<WiringSpec>,
    /// Per-experiment data placed for this trial only.
    ///
    /// The layer ensaio owns: base state is SYNCED by controllers and is
    /// identical everywhere; a fixture is PLACED because one trial needs it,
    /// and two environments legitimately differ because of it. Empty by
    /// default, so every existing spec keeps its meaning.
    ///
    /// Before this existed the only way to place data was smuggling a mutation
    /// through a CustomJob assertion command — see `crate::fixture`.
    #[serde(default)]
    pub fixtures: Vec<crate::fixture::Fixture>,
    pub assertions: Vec<AssertionSpec>,
    #[serde(default)]
    pub record: bool,
}

/// One ephemeral system Ensaio stands up for the run's duration.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct SystemSpec {
    pub id: String,
    pub template: String,
    #[serde(default)]
    pub breathe: bool,
}

/// A wiring edge letting two systems reach each other for the run.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct WiringSpec {
    pub from: String,
    pub to: String,
    pub via: String,
}

/// One typed claim to test against a system.
// `rename_all = "camelCase"` matches `tatara_lisp::domain::sexp_to_json`'s
// nested-kwargs-list bridge, which lowers `:proof-channel` to the JSON
// key `"proofChannel"` (`kebab_to_camel`, not `kebab_to_snake`) before
// handing off to `serde_json::from_value`. Without this attribute every
// multi-word field here (`proof_channel`, `header_name`,
// `must_not_traverse`) silently deserializes to its `#[serde(default)]`
// — a real, confirmed gap found authoring `specs/source-zone-tag.ensaio.lisp`:
// `ensaio show` reported `1 scenario(s) valid` while quietly dropping
// `:proof-channel`/`:header-name` on every prior spec, including the
// already-committed `authservice-admission-gate.ensaio.lisp`. Top-level
// `EnsaioSpec` fields (e.g. `confluence_ref`) are unaffected — they go
// through `DeriveTataraDomain`'s own extractor, not this JSON bridge.
/// What a [`AssertionKind::PitrDataRoundtrip`] looks for in a RESTORED database.
///
/// The operator declares WHAT to find; the kind owns HOW to look. That split is
/// the difference between this and `CustomJob`, which would hand the operator a
/// shell and call it typed — and it is why the SQL lives in the interpreter arm
/// rather than in a `command:` field an author can get subtly wrong.
///
/// The row this names is written by shinka BEFORE the snapshot is taken
/// (the PITR chart's seed template). That ordering is the
/// whole point: the drill's own `/canary-create` step cannot write it, because
/// Crossplane emits every drill Job in one desired set with no intra-response
/// sequencing, so the canary and the snapshot race and the snapshot wins.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PitrMarker {
    /// The correlation id the seed wrote. Ties the row to THIS drill, so the
    /// assertion cannot pass on a marker some earlier drill left behind — which
    /// is the difference between proving a restore and proving a table exists.
    pub correlation_id: String,
    /// Database holding the marker table. Defaults to the seed's own dedicated
    /// `pitr_drill`, which is deliberately not an application schema.
    #[serde(default = "PitrMarker::default_database")]
    pub database: String,
    /// Marker table name.
    #[serde(default = "PitrMarker::default_table")]
    pub table: String,
    /// MySQL user for the read-back. The restored server carries the SOURCE's
    /// accounts, because a restore is the source's bytes — so this is the
    /// source's user, not one created afterwards.
    #[serde(default = "PitrMarker::default_user")]
    pub user: String,
    /// Secret in the probe's namespace holding the password, and its key.
    pub password_secret: String,
    #[serde(default = "PitrMarker::default_password_key")]
    pub password_key: String,
}

impl PitrMarker {
    fn default_database() -> String {
        "pitr_drill".into()
    }
    fn default_table() -> String {
        "marker".into()
    }
    fn default_user() -> String {
        "root".into()
    }
    fn default_password_key() -> String {
        "root-password".into()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AssertionSpec {
    pub id: String,
    pub kind: AssertionKind,
    pub target: String,
    /// The namespace this assertion inspects, when it is not the rig's own.
    ///
    /// **Why this exists.** Every assertion before `EphemeralStorageOnly` probed
    /// a *system Ensaio stood up*, so the rig's namespace was always the right
    /// place to look. A qualification asks a different question — "is THAT
    /// environment sound?" — and the environment lives in its own
    /// `viveiro-env-<hash>` namespace. Without this field the check ran against
    /// the rig and would have reported on the wrong namespace: not an error, a
    /// confident answer about something nobody asked about, which is worse.
    ///
    /// `None` keeps the historical behaviour (the rig's namespace), so every
    /// existing assertion is unchanged.
    #[serde(default)]
    pub target_namespace: Option<String>,
    #[serde(default)]
    pub proof_channel: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub must_not_traverse: Option<Vec<String>>,
    /// The marker a [`AssertionKind::PitrDataRoundtrip`] verifies. Required by
    /// that kind and ignored by every other, mirroring how `header_name` serves
    /// only `HeaderSurvives`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<PitrMarker>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Container `args`, appended **after** `command` on the probe's main
    /// container (mirrors a Pod container's `command` + `args` split). The
    /// absorbed-test shape (an absorbed-test Job manifest)
    /// splits the test binary (`command`) from its flags (`args`); this field
    /// lets an [`AssertionKind::AmqpRoundtrip`] or `CustomJob` do the same.
    /// Ignored by non-Job kinds.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// A KEP-753 native-sidecar container for a two-container probe Job — the
    /// shape a single-container `CustomJob` structurally cannot express (see
    /// an absorbed-test Job manifest's header). Rendered as an
    /// `initContainer` with `restartPolicy: Always`, started first and kept
    /// running for the probe's whole lifetime, gating the main container's
    /// start on the sidecar's `startupProbe`. This is what makes the muda
    /// **absorbed-test** client-plane roundtrip (a socat loopback relay
    /// alongside the unmodified upstream AMQP test binary) expressible as a
    /// typed [`AssertionKind::AmqpRoundtrip`]. Ignored by every kind except
    /// `AmqpRoundtrip`/`CustomJob`.
    #[serde(default)]
    pub sidecar: Option<SidecarSpec>,
    /// Extra environment variables injected into a `CustomJob` probe
    /// pod, appended **after** the Downward-API `NODE_NAME` var
    /// (`k8s_env.rs`). This is the sanctioned channel for a probe
    /// credential: a `secret_key_ref` entry keeps the secret out of
    /// `command` (which is committed to git) and out of the probe image
    /// — the zero-plaintext discipline. Ignored by non-`CustomJob`
    /// kinds. Tier-honest: this makes the *preferred* path exist; an
    /// author can still bake a literal secret into `command`, so the
    /// no-inline-credential rule is a lint (CI-caught), **only-mitigated**,
    /// not truly-unrepresentable.
    #[serde(default)]
    pub env: Option<Vec<EnvVarSpec>>,
    /// Namespace the `CustomJob` probe Job is created in. `None` (the
    /// default) uses the rig's own namespace. Set it to the *target
    /// environment's* namespace so the probe (a) can reach that env's
    /// `<svc>.<ns>.svc.cluster.local` names and (b) sits inside that
    /// namespace's own network policy, sidestepping a cross-namespace
    /// default-deny `NetworkPolicy` (same-namespace traffic is allowed).
    /// Ignored by non-`CustomJob` kinds.
    #[serde(default)]
    pub probe_namespace: Option<String>,
    /// Parameters of the operator-side kinds (`crate::visita`). Flattened, so
    /// on the wire and in `(defensaio …)`/`(defvisita …)` they are ordinary
    /// keys beside the ones above; grouped in Rust so a new operator-side
    /// parameter is not a breaking change to every literal of this struct.
    #[serde(flatten)]
    pub operator: OperatorParams,
}

/// Parameters read only by the operator-side kinds ([`AssertionKind::RolloutSurgeBound`],
/// [`AssertionKind::LogWindow`], [`AssertionKind::MetricInRange`],
/// [`AssertionKind::HpaBehavior`]). They run from the operator's machine
/// through the target's own API; each field is ignored by every kind that does
/// not name it, the same convention as `header_name` and `marker`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorParams {
    /// Kube context the assertion reads. Required by every operator-side kind:
    /// there is no ambient default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Deployment / HPA names (`RolloutSurgeBound`, `HpaBehavior`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
    /// Pod label selector (`LogWindow`, `MetricInRange`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Literal substrings (`LogWindow`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patterns: Option<Vec<String>>,
    /// `absent` (default) or `present` (`LogWindow`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence: Option<Presence>,
    /// Humantime window: the rollout watch budget, or the log look-back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Humantime sampling interval (`RolloutSurgeBound`, default 2s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_surge: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unavailable: Option<i64>,
    /// Container port serving metrics — the service's own conf port, which is
    /// not always the `prometheus.io/port` annotation (`MetricInRange`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Metrics path, default `/metrics` (`MetricInRange`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Image tag prefix of the build under test. `RolloutSurgeBound` waits
    /// for, and judges only, generations on it; `LogWindow` and
    /// `MetricInRange` read only pods running it. Without it, an observation
    /// made before the pipeline deploys judges the old build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_image: Option<String>,
    /// `RolloutSurgeBound`: start recording when THIS fixture begins placement
    /// (the pin, the dispatch), not when the observe step runs. A pipeline's
    /// deploy step waits for health, so an observe that starts after it sees
    /// only the finished generation — and a bound judged over no transition
    /// proves nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_from: Option<String>,
    /// `LogWindow` with `presence present`: each pattern must match at least
    /// this many lines (default 1). How an A/B's side A proves it reproduced
    /// the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_count: Option<u64>,
    /// `LogWindow` with `presence present`: `true` holds when ANY pattern
    /// reaches `min_count` (the failure showed up in one of its forms);
    /// default `false` requires every pattern to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_any: Option<bool>,
    /// `ExecMetric`: which statistic is judged (default `all`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stat: Option<Stat>,
    /// `ExecMetric`: the pod the command runs in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<String>,
    /// `ExecMetric`: the container the command runs in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// The `behavior.scaleUp` policy every named HPA must carry (`HpaBehavior`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_up: Option<HpaPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::field::one_or_many"
    )]
    pub wait_until: Option<Vec<FieldPredicate>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Vec<FieldPredicate>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jobs: Option<Vec<JobExpect>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobExpect {
    pub job: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::field::one_or_many_strings"
    )]
    pub log_contains: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::field::one_or_many_strings"
    )]
    pub annotation_contains: Option<Vec<String>>,
}

/// One environment variable injected into a `CustomJob` probe pod.
/// Carries **either** a literal `value` **or** a `secret_key_ref` (a
/// K8s `secretKeyRef`) — a credential MUST arrive via `secret_key_ref`
/// so it never enters `command`/git/the image.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnvVarSpec {
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub secret_key_ref: Option<SecretKeyRef>,
}

/// A reference to one key of a K8s `Secret` — mirrors
/// `core/v1 EnvVarSource.secretKeyRef` exactly (a roundtrip probe's
/// proven shape).
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    pub name: String,
    pub key: String,
}

/// A KEP-753 native-sidecar container for a two-container probe Job — the
/// relay half of the muda absorbed-test shape
/// (an absorbed-test Job manifest). Rendered as an
/// `initContainer` with `restartPolicy: Always` (so kubelet starts it first,
/// keeps it running for the Pod's whole lifetime, and gates the main
/// container's start on its `startupProbe`), NOT a peer container in
/// `spec.containers` — that ordering is exactly what lets a socat loopback
/// relay be ready before the AMQP test binary dials `127.0.0.1:5672`.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SidecarSpec {
    /// The sidecar container's name. Defaults to `"relay"` when unset.
    #[serde(default)]
    pub name: Option<String>,
    /// The sidecar container image (e.g. the socat relay).
    pub image: String,
    /// The sidecar's `command` (entrypoint). `None` uses the image default.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// The sidecar's `args`, appended after `command`.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// The `exec` `startupProbe` command that gates the MAIN container's start
    /// on this sidecar being ready. **`exec`, never `tcpSocket`** — a
    /// `tcpSocket`/`httpGet` probe's `host: 127.0.0.1` resolves from kubelet's
    /// own context (the node's loopback), never the pod's, so it can never
    /// confirm a socket bound to the pod-local `127.0.0.1` (the real bug the
    /// absorbed-test YAML's own comment documents). `None` renders no
    /// `startupProbe`.
    #[serde(default)]
    pub startup_probe_command: Option<Vec<String>>,
}

/// Closed set of assertion kinds.  Each variant is one interpreter
/// arm (`crate::interp::apply`) and one CATALOG REFLECTION row —
/// growth is a new variant + arm, never a branch inside an existing
/// one.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema)]
pub enum AssertionKind {
    /// Is this environment **actually ephemeral** — does it hold no storage
    /// that could outlive it?
    ///
    /// The typed form of `theory/RAMDISK.md` §IV: every volume in the target
    /// namespace is a *bounded, memory-backed* `emptyDir`, and no
    /// `PersistentVolumeClaim` exists at all. That is the precondition for the
    /// doctrine's real claim — that declaration is the only surviving source of
    /// truth — because a drive is where undeclared state hides.
    ///
    /// **What it proves and what it does not.** It proves nothing durable
    /// *can* survive the namespace. It does NOT prove nothing *did* survive:
    /// that needs the spawn → mutate → respawn → gone sequence
    /// (`EPHEMERAL-ENVIRONMENT-PLAN.md` Phase 2), which is a procedure across
    /// two environment lifetimes and cannot be one assertion against one live
    /// namespace. Read a pass as "no drive exists here", never as "no state
    /// survived".
    ///
    /// `target` is unused — the claim is about the namespace, not one workload,
    /// which is the whole point of the namespace being the boundary
    /// (`theory/DENY-ALL.md` §II).
    EphemeralStorageOnly,
    /// Is every workload in the target namespace **actually up** — every pod
    /// Ready, and none stuck in `CrashLoopBackOff` / `ImagePullBackOff` /
    /// `Error`?
    ///
    /// **The gap this closes: a Ready that was reading the wrong thing.**
    /// `EnsaioEnvironment::wait_ready` (`k8s_env.rs`) asks a *Deployment* for
    /// `status.availableReplicas > 0`. That answers "is at least one replica of
    /// this ONE Deployment up", and an environment is a namespace full of
    /// workloads — so a namespace where the broker is fine and three
    /// microservices are crash-looping passes `wait_ready` and reports Ready.
    /// The verdict was not wrong about its own subject; it was answering a
    /// narrower question than the reader believed, which is the more dangerous
    /// failure. This kind asks the whole-namespace question directly, and it
    /// carries the pod count inside the verdict so "everything is Ready" can
    /// never again mean "the one thing I looked at was Ready".
    ///
    /// **A completed Job pod is HEALTHY, not a failure.** kubelet flips every
    /// condition — `Ready` included — to `False` once a pod's containers exit,
    /// so a namespace's finished bootstrap/migration Jobs all read
    /// `Ready: False` forever. Classifying on the `Ready` condition alone would
    /// therefore falsify every environment that ever ran a Job, which is every
    /// spawned application environment (bootstrap, schema-apply, bucket seeding).
    /// `phase: Succeeded` is checked FIRST, before any condition is read.
    ///
    /// **An empty namespace is a FALSIFICATION, not a pass and not
    /// `Vacuous`.** Zero pods is not "nothing was in scope" — the namespace was
    /// in scope and it was examined; what was found is that an environment
    /// which is supposed to be running has nothing running in it. That is
    /// evidence *against* health, so it is a finding. The alternative reading
    /// (`Vacuous` → `Inconclusive`) would let a namespace that failed to spawn
    /// at all report as merely unproven.
    ///
    /// **What a pass does NOT prove.** Pod-level `Ready` is kubelet's verdict:
    /// for a container with no `readinessProbe` it means only "the process
    /// started". So a held `EnvironmentHealth` proves nothing is crash-looping,
    /// pull-failing, or stuck — it does not prove any service *works*. That
    /// proof is [`AssertionKind::AmqpRoundtrip`]'s and `CustomJob`'s job.
    ///
    /// `target` is unused — like `EphemeralStorageOnly`, the claim is about the
    /// namespace as the boundary, not about one workload.
    EnvironmentHealth,
    /// Does a request with a given proof-channel + session state get
    /// admitted past an Authservice-shaped gate before the target
    /// evaluates its own proof?
    AuthserviceAdmission,
    /// Does a client TLS certificate survive to the target's own TLS
    /// terminator, or does an intermediate ingress consume it?
    TlsTerminationPassthrough,
    /// Does a named HTTP header's value reach the target unmodified?
    HeaderSurvives,
    /// Does traffic from `from` reach `target` without traversing a
    /// named intermediate?
    NetworkPath,
    /// Run an arbitrary typed probe image; assert its exit/verdict.
    CustomJob,
    /// The AMQP CLIENT-plane roundtrip (connect / auth / publish / consume /
    /// ack against `:5672`) — the message-bus behaviour the read-only
    /// management-API `GET`s (`CustomJob`) cannot reach. Rendered as the muda
    /// **two-container absorbed-test** shape: a KEP-753 native-sidecar relay
    /// ([`AssertionSpec::sidecar`]) alongside the main probe container running
    /// the unmodified upstream AMQP test binary (`image` + `command` +
    /// `args`). The distinct kind exists because the client-plane roundtrip is
    /// *structurally* a two-container Pod — a shape single-container
    /// `CustomJob` cannot express (see
    /// an absorbed-test Job manifest).
    AmqpRoundtrip,
    /// Did a row written to the SOURCE database before a snapshot come back out
    /// of the RESTORE?
    ///
    /// The one assertion that makes a PITR drill mean anything. Everything else
    /// a drill can report — the snapshot bound, the PVC bound, the StatefulSet
    /// ran — is true of a restore that produced an empty database.
    ///
    /// **It reads a row shinka seeded BEFORE the snapshot**
    /// (the PITR chart's seed template), not one the drill
    /// wrote itself. The drill's own `/canary-create` step cannot do this job:
    /// Crossplane emits every drill Job into one desired set with no
    /// intra-response sequencing, so the canary and the VolumeSnapshot are
    /// created together and the snapshot wins essentially always. Ordering has to
    /// come from controller convergence, which is what a reconciled
    /// `DatabaseMigration` provides and a Job race does not.
    ///
    /// The check is deliberately stricter than "a row exists": it matches on the
    /// drill's own `correlationId` and recomputes the payload's SHA-256, so an
    /// earlier drill's leftover marker fails, and so does a row that came back
    /// truncated or re-encoded rather than intact.
    PitrDataRoundtrip,
    /// Did every new generation of the named Deployments roll with at most
    /// `max_surge` extra pods and at most `max_unavailable` missing, sampled
    /// every `interval` over `window` — and finish?
    ///
    /// Operator-side (`crate::visita`): reads Deployment status through the
    /// target's API, creates nothing there. The baseline is the generation in
    /// the trial's natural state, so a rollout that began before observation
    /// started is still judged. No new generation in the window is `Vacuous`.
    RolloutSurgeBound,
    /// Are the literal `patterns` absent (or present) in the pods' logs for
    /// `window` (or since the trial began)? Zero lines read is `Vacuous`.
    LogWindow,
    /// Is `metric` exposed by every selected pod on `port`/`path` (through the
    /// apiserver's pod proxy), inside `[min, max]` when given? An unreadable
    /// pod is named as unreadable, never as absent.
    MetricInRange,
    /// Does every named HPA carry the `scale_up` policy?
    HpaBehavior,
    /// Does a declared, read-only command run in one pod report a number
    /// inside `[min, max]` — every sample (`stat all`), or the window's
    /// largest (`max`) or smallest (`min`)? Recorded from a fixture like a
    /// rollout, so a broker's run queue under starvation is on record, not
    /// sampled once after the fact. Zero parseable samples is `Vacuous`.
    ExecMetric,
    #[schemars(
        description = "Does one object, read through the API, satisfy every :expect predicate once its :wait-until predicates hold, within :timeout? A timeout is Falsified with the last observed values; an unreadable or forbidden API is Skipped as unobserved."
    )]
    ObjectFields,
    #[schemars(
        description = "Do the named jobs of a run this trial dispatched (:run-of) reach their expected :conclusion, and do their logs and annotations contain the expected texts, within :timeout? A job still running at the timeout is Falsified with its last state; an unreadable run is Skipped as unobserved."
    )]
    WorkflowJobs,
    /// Did every declared step of a visita run? Computed by the runner, never
    /// authored. A trial that aborted — a placement refused, the guard tripped
    /// — answered nothing, so this is a gating precondition: `Skipped` with
    /// the abort reason makes the receipt Inconclusive even when the restore
    /// came back clean.
    StepsCompleted,
    /// Did every pod the guard watched stay Ready, with no restart and no
    /// liveness-failure run near its kill, for the whole run? A trip is
    /// Falsified, its finding naming the pod, the signal and the time. Computed by the runner from the guard's probes, never
    /// authored; a gating precondition.
    GuardHeld,
    /// Is the borrowed environment back to its captured natural state?
    ///
    /// Never authored: the visita runner computes it after undo from the
    /// restore diff. Subjects are the objects checked; every missing, extra or
    /// differing object (modulo the allow-list) and every failed undo is a
    /// finding.
    NaturalStateRestored,
}

/// Which statistic of an [`AssertionKind::ExecMetric`] record is judged.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Stat {
    /// Every sample must be in range.
    All,
    /// The window's largest sample must be in range (e.g. "saturation reached").
    Max,
    /// The window's smallest sample must be in range.
    Min,
}

/// Whether [`AssertionKind::LogWindow`]'s patterns must be absent or present.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Presence {
    Absent,
    Present,
}

/// One HPA `behavior.scaleUp` policy, as [`AssertionKind::HpaBehavior`]
/// expects to find it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HpaPolicy {
    /// `Pods` or `Percent`.
    pub kind: String,
    pub value: i64,
    pub period_seconds: i64,
}

impl AssertionKind {
    /// Every variant, once. CATALOG REFLECTION's enumeration surface —
    /// co-located with the enum so adding a variant here is visually
    /// forced to touch the same file as this list; `tests/catalog_reflection.rs`
    /// fails if the two drift (a new variant added to the enum but not
    /// here, or vice versa).
    pub const ALL: [AssertionKind; 19] = [
        AssertionKind::EphemeralStorageOnly,
        AssertionKind::EnvironmentHealth,
        AssertionKind::AuthserviceAdmission,
        AssertionKind::TlsTerminationPassthrough,
        AssertionKind::HeaderSurvives,
        AssertionKind::NetworkPath,
        AssertionKind::CustomJob,
        AssertionKind::AmqpRoundtrip,
        AssertionKind::PitrDataRoundtrip,
        AssertionKind::RolloutSurgeBound,
        AssertionKind::LogWindow,
        AssertionKind::MetricInRange,
        AssertionKind::HpaBehavior,
        AssertionKind::ExecMetric,
        AssertionKind::ObjectFields,
        AssertionKind::WorkflowJobs,
        AssertionKind::StepsCompleted,
        AssertionKind::GuardHeld,
        AssertionKind::NaturalStateRestored,
    ];
}

// ── The witness ────────────────────────────────────────────────────

/// One thing a probe actually examined — an HTTP response line, a probe
/// pod's log line, a job's terminal status. The **atom of a verdict's
/// subject set**.
///
/// A newtype rather than a bare `String` so a subject set can never be
/// confused with a bag of prose: the type says *these are the things that
/// were looked at*.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[serde(transparent)]
pub struct Observation(String);

impl Observation {
    /// Record one examined thing.
    pub fn new(what: impl Into<String>) -> Self {
        Self(what.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Observation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A collection that is **non-empty by construction**.
///
/// `head` is a real field, so there is no empty inhabitant to name: an
/// empty `NonEmpty` is a *type* error, not a failed constructor. That is
/// the whole difference between this and a checked `Vec` — a checked
/// collection buys a `Result::Err` (a mitigation); this buys an absence.
///
/// Fields are private and there is no public struct-literal path, and the
/// mutating operations that would walk a constructed value back to empty
/// (`pop`, `drain`, `clear`, `truncate`, `retain`, `remove`, `DerefMut`,
/// `Default`) are all deliberately absent — otherwise the invariant would
/// hold at construction and nowhere else.
///
/// Shape and vocabulary mirror `gen-verdict`'s `NonEmpty<T>`
/// (`gen/crates/gen-verdict/src/subjects.rs`) so adopting that crate later
/// is a type-alias swap rather than a rewrite. See this module's
/// `## Why this is local` note for why it is not adopted today.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "Vec<T>", bound(deserialize = "T: serde::Deserialize<'de>"))]
pub struct NonEmpty<T> {
    head: T,
    tail: Vec<T>,
}

/// The subject set a verdict was derived from — *the things that were
/// examined*. `gen-verdict`'s `Subjects<S>` in ensaio's one concrete
/// domain.
pub type Observations = NonEmpty<Observation>;

/// Raised when a sequence that must carry at least one element carried
/// none. The **one** runtime door into [`NonEmpty`]: bytes arrive from
/// wires and files as sequences, and a parse boundary is exactly where a
/// `Result::Err` is the honest answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("expected at least one element, got an empty sequence")]
pub struct EmptyWitness;

impl<T> NonEmpty<T> {
    /// Scope a candidate set. Returns the **sum**, never `Option` —
    /// `Option` invites `unwrap_or_default()`, which is precisely the
    /// silent round-up this type exists to remove. A named
    /// [`Domain::Empty`] arm forces the caller to *say* what emptiness
    /// means in its domain.
    pub fn scope(items: impl IntoIterator<Item = T>) -> Domain<T> {
        let mut it = items.into_iter();
        match it.next() {
            Some(head) => Domain::Populated(Self {
                head,
                tail: it.collect(),
            }),
            None => Domain::Empty,
        }
    }

    /// How many things were examined. A projection of the value — never a
    /// field a caller sets, so the count and the claim share one origin.
    #[must_use]
    pub fn count(&self) -> std::num::NonZeroUsize {
        // SAFETY-free: `head` is a real field, so the length is ≥ 1 by
        // construction and the `NonZeroUsize` is total, not checked.
        std::num::NonZeroUsize::new(1 + self.tail.len()).unwrap_or(std::num::NonZeroUsize::MIN)
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(&self.head).chain(self.tail.iter())
    }

    /// The only fallible ingress, and the one serde uses.
    ///
    /// # Errors
    ///
    /// [`EmptyWitness`] when `items` is empty.
    pub fn try_from_vec(items: Vec<T>) -> Result<Self, EmptyWitness> {
        match Self::scope(items) {
            Domain::Populated(ne) => Ok(ne),
            Domain::Empty => Err(EmptyWitness),
        }
    }
}

impl<T> TryFrom<Vec<T>> for NonEmpty<T> {
    type Error = EmptyWitness;
    fn try_from(items: Vec<T>) -> Result<Self, Self::Error> {
        Self::try_from_vec(items)
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T> {
    type Item = &'a T;
    type IntoIter = Box<dyn Iterator<Item = &'a T> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<T: Serialize> Serialize for NonEmpty<T> {
    /// Serializes as a plain sequence — the wire shape of a `Vec<T>`, so
    /// no consumer has to learn a new encoding. The *deserialize* side is
    /// what rejects an empty one.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.iter())
    }
}

/// The CRD's schema for a subject set: an array with `minItems: 1`, so the
/// non-emptiness invariant is stated to the apiserver too and not only to
/// the Rust compiler.
///
/// Implemented concretely for `Observations` rather than generically: a
/// generic `schema_name()` would have to compose a string, and `format!()`
/// is banned outside `Display`/`Debug` impls, typed log/error macros and
/// typed AST renderers (★★ TYPED EMISSION).
impl JsonSchema for Observations {
    fn schema_name() -> String {
        "Observations".to_owned()
    }

    // `generator`, not `gen`: `gen` is a reserved keyword in edition 2024,
    // which is also why the module below is spelled `r#gen`.
    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::Array.into()),
            array: Some(Box::new(schemars::schema::ArrayValidation {
                items: Some(generator.subschema_for::<Observation>().into()),
                min_items: Some(1),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

/// The result of scoping a candidate subject set. A **sum**, never
/// `Option` — see [`NonEmpty::scope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Domain<T> {
    Populated(NonEmpty<T>),
    Empty,
}

// ── Verdict + outcome ──────────────────────────────────────────────

// What one assertion is entitled to say about the subject set it
// examined.
//
// # The law this type is
//
// `theory/UNREPRESENTABILITY.md` §II.4, the **derived-verdict law**:
//
// > A verdict is derived from the subject set it claims about, and
// > carries the witness of that derivation.
//
// **Clause 1 — derivation.** [`Verdict::judge`] is the sole classifier
// for the three arms that make a claim. `Held` and `Falsified` are
// `#[non_exhaustive]`, so no code outside this crate can *name* them:
// `Verdict::Held { .. }` is a compile error (`E0639`) rather than a
// discouraged pattern.
//
// **Clause 2 — witness.** [`Verdict::Held`] cannot be named without an
// [`Observations`], and an `Observations` cannot be obtained without
// having held at least one observation. A pass over an empty set
// therefore has no expressible form. [`Verdict::Vacuous`] and
// [`Verdict::Unreached`] are **distinct arms** — never folded into a
// pass, never folded into each other.
//
// # What emptiness MEANS for ensaio
//
// §II.4's clause 2 deliberately does **not** say "empty fails" — it says
// emptiness must be *sayable*, so its meaning is decided once, visibly,
// per domain. ensaio's product is a verdict about an architecture, and it
// already reserves a name for *neither proven nor disproven*. So:
//
// > **An assertion that examined zero subjects is [`Verdict::Vacuous`],
// > and a `Vacuous` assertion routes the run to
// > [`CertVerdict::Inconclusive`](crate::CertVerdict::Inconclusive) —
// > never `Certified`, and never `NotCertified`.**
//
// It is not a failure (`NotCertified` requires real, witnessed evidence
// *against* the claim) and it is not a pass. It is the same honest
// position ensaio already takes for [`Verdict::Skipped`], reached by a
// different route: `Skipped` means *the substrate wasn't fit to ask*,
// `Vacuous` means *we asked, and there was nothing to look at*. Both are
// legitimate outcomes of a real trial; neither may be reported as "held".
//
// # The two arms that make no claim by a different route
//
// [`Verdict::Errored`] (the rig broke) and [`Verdict::Skipped`] (a gating
// precondition was unmet) are ensaio-specific no-claim arms, retained
// verbatim — MODULARIZE-DON'T-DELETE. They are not derivable from a
// subject set because they describe a state *before* classification, so
// `judge` never produces them and they carry no witness.
//
// # Why this is local, and not `gen-verdict`
//
// `gen/crates/gen-verdict` ships this exact law as a fleet primitive, and
// its re-deriving `#[serde(try_from)]` border is directly valuable here
// because this verdict lands in `status` on a Kubernetes CR. It is
// **not adopted today for two independent, mechanical reasons**:
//
// 1. `gen-verdict` does not implement [`schemars::JsonSchema`] (its
//    dependencies are `serde` + `thiserror` + `gen-macros`, deliberately),
//    and this type is reachable from the `Ensaio` CRD's `status.results`,
//    which requires it. A generic `Verdict<S, F>` in a CRD would also
//    have to carry `JsonSchema` bounds through `NonEmpty` and `Domain`.
// 2. `gen-verdict` is not published, and ensaio's flake takes exactly one
//    input (`substrate`), so a cross-repo path dependency would not build
//    under Nix.
//
// The arm names, the classifier name, the `Subjects`/`Findings` roles,
// the `Domain` sum and the wire tags are therefore **mirrored exactly**,
// so the migration is `pub type Verdict = gen_verdict::Verdict<Observation,
// Observation>` plus a `JsonSchema` impl, not a rewrite. Contributing
// `JsonSchema` upstream is the load-bearing fix; `gen` is owned by
// another agent and is not edited from here.
//
// # Tier — graded, not rounded
//
// | Claim | Tier |
// |---|---|
// | a pass without a non-empty witness, outside this crate | **truly-unrepresentable** — `#[non_exhaustive]` + no empty `Observations` inhabitant |
// | a pass without a non-empty witness, *inside* this crate | **only-mitigated** — `#[non_exhaustive]` is a crate boundary; every in-crate site is routed through `judge`, by convention |
// | a forged verdict arriving over the wire | **parse-time-rejected** — `#[serde(try_from)]` re-derives and returns `Err` |
// | the subject set is the *right* set | **only-mitigated** — non-emptiness is silent about scope (§II.3's E ceiling) |
//
// # Why the wire shape is externally tagged
//
// `gen-verdict` uses `#[serde(tag = "verdict")]`. **That shape cannot be
// expressed in a Kubernetes CRD**, and this is a hard constraint rather
// than a preference: `kube::core::schema::StructuralSchemaRewriter`
// (kube-core 0.99) *panics* while flattening an internally-tagged enum —
//
// ```text
// Property "verdict" has the schema … enum_values: Some([String("falsified")])
// but was already defined as … enum_values: Some([String("held")]) in another
// subschema. The schemas for a property used in multiple subschemas must be
// identical
// ```
//
// — because a Kubernetes *structural* schema forbids one property being
// defined differently across `oneOf` branches. So the wire stays
// externally tagged (`{"Held":{"subjects":[…]}}`), which is also the shape
// the CRD already in the cluster declares. The arm names, the classifier,
// the witness roles and the re-derivation are what mirror `gen-verdict`;
// the tagging is a CRD-imposed local decision, and any future CRD-hosted
// adopter of `gen-verdict` will have to make the same one.
/// What one assertion is entitled to say about the subject set it
/// examined.
///
/// `Held` requires a non-empty witness of what was examined; `Vacuous`
/// (examined nothing) and `Unreached` (never ran) are distinct arms and
/// are never a pass. Derived only by [`Verdict::judge`]. See the
/// implementation notes above this type for the full rationale
/// (`theory/UNREPRESENTABILITY.md` §II.4).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(try_from = "VerdictWire")]
pub enum Verdict {
    /// The claim holds over a **non-empty** examined subject set.
    ///
    /// `subjects` is the witness. It is not a count, and not a count that
    /// happens to be non-zero: it is the examined values themselves.
    #[non_exhaustive]
    Held { subjects: Observations },
    /// The claim fails over a non-empty examined subject set, with at
    /// least one finding. A falsified verdict with zero findings would be
    /// a held verdict wearing the wrong tag, so `findings` is non-empty by
    /// the same construction as `subjects`.
    #[non_exhaustive]
    Falsified {
        subjects: Observations,
        findings: Observations,
    },
    /// The rig itself broke — the probe could not be run to a conclusion.
    /// Makes no claim about the architecture.
    Errored { message: String },
    /// The claim was neither proven nor disproven — a precondition it
    /// depends on (e.g. the broker's mgmt API being reachable) was
    /// unmet, so the substrate wasn't fit to test it. Distinct from
    /// `Falsified` (real, witnessed evidence *against* the claim): a
    /// `Skipped` assertion never contributes a falsification to a
    /// certificate — it routes to `Inconclusive`, the honest verdict
    /// when the environment couldn't answer the question.
    Skipped { reason: String },
    /// The assertion ran and **nothing was in scope**. Not a pass, not a
    /// falsification — the distinct arm §II.4's clause 2 requires, and the
    /// answer to "the assertion held over forty subjects" and "the
    /// assertion held over zero" having been the same value.
    Vacuous {},
    /// The assertion **never ran**. No claim of any kind is available.
    ///
    /// The honest [`Default`], so a verdict field that was never assigned
    /// reads as "no claim" rather than as a pass.
    Unreached {},
}

impl Default for Verdict {
    /// [`Verdict::Unreached`] — the only arm that both claims nothing and
    /// says why.
    fn default() -> Self {
        Self::Unreached {}
    }
}

impl Verdict {
    /// **The one classifier.** The three claim-making arms are chosen here
    /// and nowhere else.
    ///
    /// The ladder, and why each rung is where it is:
    ///
    /// 1. an empty domain ⇒ [`Verdict::Vacuous`] — checked **first**, so
    ///    no finding count and no caller intent can promote an empty scope
    ///    to a claim;
    /// 2. subjects, no findings ⇒ [`Verdict::Held`];
    /// 3. subjects and findings ⇒ [`Verdict::Falsified`].
    ///
    /// Findings over an empty domain are `Vacuous` and the findings are
    /// dropped: a finding is a statement *about a subject*, so findings
    /// over a set that was never examined are not findings. `judge` stays
    /// total rather than panicking; the behaviour is pinned by test.
    #[must_use]
    pub fn judge(domain: Domain<Observation>, findings: Vec<Observation>) -> Self {
        // Every arm is named. There is deliberately no `_` here: a
        // wildcard is the mechanism §II.3's subclass D describes for
        // rounding a new case up into an existing one, and a classifier is
        // the last place to leave that door open.
        match (domain, Observations::scope(findings)) {
            (Domain::Empty, Domain::Empty | Domain::Populated(_)) => Self::Vacuous {},
            (Domain::Populated(subjects), Domain::Empty) => Self::Held { subjects },
            (Domain::Populated(subjects), Domain::Populated(findings)) => {
                Self::Falsified { subjects, findings }
            }
        }
    }

    /// Convenience over [`Verdict::judge`] for the common probe shape: a
    /// set of observations and a pass/fail bit that the probe itself
    /// determined. `false` makes the observations double as the findings —
    /// for a probe, what was seen *is* the evidence against.
    #[must_use]
    pub fn from_probe(observations: Vec<Observation>, passed: bool) -> Self {
        let findings = if passed {
            Vec::new()
        } else {
            observations.clone()
        };
        Self::judge(Observations::scope(observations), findings)
    }

    #[must_use]
    pub fn held(&self) -> bool {
        matches!(self, Verdict::Held { .. })
    }

    #[must_use]
    pub fn falsified(&self) -> bool {
        matches!(self, Verdict::Falsified { .. })
    }

    /// The examined subject set, when there was one.
    ///
    /// `None` for the four arms that examined nothing. Safe as an `Option`
    /// precisely because there is no default witness to fall back to:
    /// `unwrap_or_default()` does not typecheck on `Option<&Observations>`.
    #[must_use]
    pub fn subjects(&self) -> Option<&Observations> {
        match self {
            Self::Held { subjects, .. } | Self::Falsified { subjects, .. } => Some(subjects),
            Self::Errored { .. } | Self::Skipped { .. } | Self::Vacuous {} | Self::Unreached {} => {
                None
            }
        }
    }

    /// How many subjects the verdict is about — `0` for the no-claim arms,
    /// which is the honest number and says so.
    #[must_use]
    pub fn subject_count(&self) -> usize {
        self.subjects().map_or(0, |s| s.count().get())
    }

    /// The verdict's stable class tag — the content-address component, and
    /// the wire tag. Exhaustive over the closed arm set with **no `_` arm**,
    /// so a future seventh variant is a compile error here rather than
    /// being silently rounded into a neighbour (§II.3's subclass D).
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Held { .. } => "held",
            Self::Falsified { .. } => "falsified",
            Self::Errored { .. } => "errored",
            Self::Skipped { .. } => "skipped",
            Self::Vacuous {} => "vacuous",
            Self::Unreached {} => "unreached",
        }
    }
}

// ── The wire border: re-derive, and reject rather than normalize ───────

/// The deserialization shape.
///
/// Because the wire is externally tagged, the current and the pre-§II.4
/// forms share an outer tag and differ only in *fields*: `subjects` /
/// `findings` (current) versus a single `evidence` string (legacy). So one
/// wire enum accepts both, and `evidence.is_some()` selects the migration
/// path — no `#[serde(untagged)]` fallback, and no loss of the parse error.
///
/// Every claim-making arm carries **both** sequences, so a record which
/// *declares* one arm while *carrying* the shape of another is visible to
/// the re-derivation rather than silently discarded as an unknown field.
///
/// The legacy path is accepted deliberately: a `kube::runtime::Controller`
/// watch decodes **every** object in its stream, so a border that rejected
/// the old shape would wedge the trials controller on pre-existing CRs
/// rather than merely refusing a forgery. It is not a hole — it re-derives
/// like any other record, so a legacy `Held` whose `evidence` is empty
/// (the exact defect this change closes) **demotes to
/// [`Verdict::Vacuous`] on read**.
#[derive(Deserialize)]
enum VerdictWire {
    Held {
        #[serde(default)]
        subjects: Vec<Observation>,
        #[serde(default)]
        findings: Vec<Observation>,
        /// Pre-§II.4 only. Present ⇒ this is a legacy record.
        #[serde(default)]
        evidence: Option<String>,
    },
    Falsified {
        #[serde(default)]
        subjects: Vec<Observation>,
        #[serde(default)]
        findings: Vec<Observation>,
        #[serde(default)]
        evidence: Option<String>,
    },
    Errored {
        message: String,
    },
    Skipped {
        reason: String,
    },
    Vacuous {},
    Unreached {},
}

/// Failure modes of the verdict parse boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum VerdictError {
    /// A serialized verdict declared an arm its own witness does not
    /// derive — a forged or corrupted record.
    ///
    /// Rejected rather than silently corrected. Correcting it would hide
    /// that something shipped a record claiming more than it examined,
    /// which is the exact event this type exists to surface.
    #[error(
        "verdict declares `{declared}` but its own witness derives `{derived}` \
         ({subjects} subject(s), {findings} finding(s))"
    )]
    DeclaredVerdictContradictsWitness {
        declared: &'static str,
        derived: &'static str,
        subjects: usize,
        findings: usize,
    },
}

impl TryFrom<VerdictWire> for Verdict {
    type Error = VerdictError;

    fn try_from(wire: VerdictWire) -> Result<Self, Self::Error> {
        let (declared, subjects, findings): (&'static str, Vec<Observation>, Vec<Observation>) =
            match wire {
                // No claim, no witness — nothing to re-derive.
                VerdictWire::Errored { message } => return Ok(Self::Errored { message }),
                VerdictWire::Skipped { reason } => return Ok(Self::Skipped { reason }),
                VerdictWire::Vacuous {} => return Ok(Self::Vacuous {}),
                VerdictWire::Unreached {} => return Ok(Self::Unreached {}),

                // ── The legacy migration ──────────────────────────────
                //
                // Re-derived and ACCEPTED, never rejected — deliberately
                // unlike the current-shape arms below, which must agree
                // with their own witness or fail. The asymmetry is the
                // whole point:
                //
                //  * A *current* record was written by code that already
                //    knew this law. A disagreement there is a forgery or a
                //    corruption, and refusing it is the correct answer.
                //  * A *legacy* record was written by code that did not.
                //    Its tag is not a trustworthy claim, it is a value
                //    being migrated. Rejecting it would wedge the trials
                //    controller on precisely the CRs the buggy code wrote
                //    — the watch decodes every object in the stream —
                //    which punishes the cluster for our defect.
                //
                // So the old single `evidence` string becomes the one
                // observation it always was, and `judge` re-classifies. An
                // EMPTY legacy evidence scopes to nothing and therefore
                // DEMOTES to `Vacuous`: the migration corrects exactly the
                // records that were lying, on read, without a wedge.
                VerdictWire::Held {
                    evidence: Some(evidence),
                    ..
                } => {
                    return Ok(Self::judge(
                        Observations::scope(legacy_observations(evidence)),
                        Vec::new(),
                    ));
                }
                VerdictWire::Falsified {
                    evidence: Some(evidence),
                    ..
                } => {
                    // For a legacy falsification the one evidence string is
                    // both what was examined and what was found against.
                    let obs = legacy_observations(evidence);
                    return Ok(Self::judge(Observations::scope(obs.clone()), obs));
                }

                VerdictWire::Held {
                    subjects,
                    findings,
                    evidence: None,
                } => ("held", subjects, findings),
                VerdictWire::Falsified {
                    subjects,
                    findings,
                    evidence: None,
                } => ("falsified", subjects, findings),
            };

        let subject_len = subjects.len();
        let finding_len = findings.len();
        let derived = Self::judge(Observations::scope(subjects), findings);
        if derived.tag() == declared {
            Ok(derived)
        } else {
            Err(VerdictError::DeclaredVerdictContradictsWitness {
                declared,
                derived: derived.tag(),
                subjects: subject_len,
                findings: finding_len,
            })
        }
    }
}

/// A legacy `evidence` string as a subject set: one observation, or none
/// when the string carried nothing.
fn legacy_observations(evidence: String) -> Vec<Observation> {
    if evidence.trim().is_empty() {
        Vec::new()
    } else {
        vec![Observation::new(evidence)]
    }
}

/// One assertion's recorded result.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct AssertionResult {
    pub assertion_id: String,
    pub kind: AssertionKind,
    pub verdict: Verdict,
}

/// One fixture the trial deliberately left behind.
///
/// Typed rather than a rendered line, because the reason is load-bearing:
/// [`crate::fixture::Cleanup::Retained`] requires saying why, so "we never
/// cleaned this up" cannot masquerade as a decision — and a report that flattens
/// the reason into prose gives that requirement back up.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct FixtureResidue {
    pub fixture: String,
    pub reason: String,
}

/// The full run's typed outcome.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct EnsaioOutcome {
    pub run_id: String,
    pub results: Vec<AssertionResult>,
    pub systems_provisioned: Vec<String>,
    pub systems_torn_down: Vec<String>,
    /// Fixtures actually placed, in placement order.
    ///
    /// The distinction assertions do not have: an assertion OBSERVES, a fixture
    /// CHANGES. Recording what a run altered is the whole reason placement is a
    /// typed step instead of a mutation smuggled through a `CustomJob` command.
    #[serde(default)]
    pub fixtures_placed: Vec<String>,
    /// Fixtures undone, in cleanup order (the reverse of placement).
    ///
    /// Never assumed equal to `fixtures_placed`: a fixture whose cleanup is
    /// `EnvironmentIsDestroyed` or `Retained` is placed and never cleaned here,
    /// and a cleanup that FAILED is absent from this list rather than counted.
    #[serde(default)]
    pub fixtures_cleaned: Vec<String>,
    /// What this run leaves behind — placed fixtures declared `Retained`, with
    /// the reason each was retained.
    ///
    /// Filtered to fixtures actually PLACED: a retained fixture the run never
    /// got to leaves nothing, and reporting it would overstate the residue.
    #[serde(default)]
    pub residue: Vec<FixtureResidue>,
}

impl EnsaioOutcome {
    /// Whether every assertion held **over a non-empty catalog**.
    ///
    /// The leading `!self.results.is_empty()` is the whole point and is
    /// load-bearing: `[].iter().all(_)` is `true`, so without it a run that
    /// asserted nothing would report that everything held. That is
    /// §II.3's subclass A — the same shape as `sui`'s
    /// `ShadowReport::all_pass` returning `true` over an empty `records`.
    /// Pinned by `tests/derived_verdict_law.rs`.
    ///
    /// A [`Verdict::Vacuous`] assertion is not `held()`, so a catalog
    /// containing one cannot report `all_held` either.
    #[must_use]
    pub fn all_held(&self) -> bool {
        !self.results.is_empty() && self.results.iter().all(|r| r.verdict.held())
    }

    /// How many assertions this outcome is about — the subject-set witness
    /// at the run level. §II.3's subclass-E discipline: *a gate that cannot
    /// state what it examined cannot be audited for what it missed.*
    #[must_use]
    pub fn subject_count(&self) -> usize {
        self.results.len()
    }
}
