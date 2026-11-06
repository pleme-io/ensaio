//! [`OperatorEnvironment`] — the [`EnsaioEnvironment`] for a borrowed cluster.
//!
//! Every read and write happens from the operator's machine through the
//! target's own API ([`super::command::Verb`]). It provisions nothing, wires
//! nothing, and refuses every assertion kind that would run a probe inside the
//! target.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::carve::{ArgoPaused, NodeCarve, PauseMode};
use super::command::{Operator, Since, Verb, now};
use super::guard::{GuardSignal, GuardTrip, LivenessFailure, LivenessProbe, LivenessStreak};
use super::judge::{self, RolloutSample};
use super::lease::{LeaseRef, LeaseRequest, RunPlan};
use super::manifest::ObjectRef;
use super::snapshot::{NaturalState, ObjectKey};
use super::spec::{Action, Peer};
use crate::fixture::{Cleanup, Fixture};
use crate::interp::EnsaioEnvironment;
use crate::nervous::PhaseEvent;
use crate::types::{AssertionKind, AssertionSpec, Presence, SystemSpec, Verdict, WiringSpec};

/// A pipeline run this trial dispatched, tracked until it is stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatched {
    pub fixture: String,
    pub repo: String,
    pub run_id: String,
}

/// The objects one `:apply-manifest` placement created, where, and how long
/// its undo may wait for them to go.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Created {
    context: String,
    namespace: String,
    objects: Vec<ObjectRef>,
    timeout: Duration,
}

/// Where one `:exec` placement ran its argv.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ExecRan {
    context: String,
    namespace: String,
    container: String,
    pods: Vec<String>,
}

/// The one container an in-place resize addresses.
struct ResizeTarget<'t> {
    context: &'t str,
    namespace: &'t str,
    pod: &'t str,
    container: &'t str,
}

/// What a record samples.
#[derive(Debug, Clone)]
enum RecordKind {
    /// Named Deployments' rollout state.
    Rollout { names: Vec<String> },
    /// A declared read-only command in one pod, parsed to a number.
    Exec {
        pod: String,
        container: String,
        argv: Vec<String>,
    },
}

/// One record in progress: what it samples, how often, since when.
#[derive(Debug, Clone)]
struct Recorder {
    assertion: String,
    context: String,
    namespace: String,
    kind: RecordKind,
    interval: Duration,
    started: Instant,
    last: Option<Instant>,
    active: bool,
}

/// One sample of an [`AssertionKind::ExecMetric`] record.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct ExecSample {
    pub at: String,
    pub value: Option<f64>,
    pub raw: String,
}

/// One probed container the guard counts liveness failures for.
#[derive(Debug, Clone)]
struct LivenessWatch {
    pod: String,
    probe: LivenessProbe,
    sole: bool,
    streak: LivenessStreak,
}

impl LivenessWatch {
    fn owns(&self, f: &LivenessFailure) -> bool {
        f.pod == self.pod
            && f.container
                .as_deref()
                .map_or(self.sole, |c| c == self.probe.container)
    }
}

/// The guard's running state.
#[derive(Debug, Clone)]
struct GuardState {
    spec: super::spec::Guard,
    interval: Duration,
    max_unready: Duration,
    margin: u32,
    last: Option<Instant>,
    restarts: BTreeMap<String, i64>,
    unready_since: BTreeMap<String, Instant>,
    liveness: Vec<LivenessWatch>,
    probes: usize,
    tripped: Option<GuardTrip>,
}

impl GuardState {
    fn trip(&mut self, pod: &str, signal: GuardSignal) {
        if self.tripped.is_none() {
            self.tripped = Some(GuardTrip {
                pod: pod.to_string(),
                signal,
                at: now(),
            });
        }
    }
}

fn restart_count(pod: &Value) -> i64 {
    pod.pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .map_or(0, |cs| {
            cs.iter()
                .filter_map(|c| c.get("restartCount").and_then(Value::as_i64))
                .sum()
        })
}

fn pod_ready(pod: &Value) -> bool {
    pod.pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|cs| {
            cs.iter().any(|c| {
                c.get("type").and_then(Value::as_str) == Some("Ready")
                    && c.get("status").and_then(Value::as_str) == Some("True")
            })
        })
}

/// The objects in a kubectl JSON answer. `kubectl get <kind> <one-name>`
/// returns the OBJECT, not a `List` — reading only `items` would see nothing
/// (measured live: a single-pod read reported the pod "not found").
#[must_use]
pub fn list_items(v: &Value) -> Vec<Value> {
    match v.get("items").and_then(Value::as_array) {
        Some(items) => items.clone(),
        None if v.get("kind").is_some() => vec![v.clone()],
        None => Vec::new(),
    }
}

/// A number out of a command's output: the whole output (a trailing Erlang
/// `.` allowed), else the first token that parses.
#[must_use]
pub fn parse_number(out: &str) -> Option<f64> {
    let t = out.trim().trim_end_matches('.');
    t.parse::<f64>().ok().or_else(|| {
        t.split(|c: char| {
            c.is_whitespace() || c == ',' || c == '{' || c == '}' || c == '[' || c == ']'
        })
        .find_map(|w| w.trim_end_matches('.').parse::<f64>().ok())
    })
}

pub struct OperatorEnvironment<'a> {
    op: Operator<'a>,
    natural: RefCell<Option<NaturalState>>,
    started_at: String,
    runs: RefCell<Vec<Dispatched>>,
    stop_timeout: Duration,
    stop_poll: Duration,
    transient_patience: Duration,
    transient_pause: Duration,
    recorders: RefCell<Vec<Recorder>>,
    recorded: RefCell<BTreeMap<String, Vec<RolloutSample>>>,
    exec_recorded: RefCell<BTreeMap<String, Vec<ExecSample>>>,
    guard: RefCell<Option<GuardState>>,
    created: RefCell<BTreeMap<String, Created>>,
    exec_ran: RefCell<BTreeMap<String, ExecRan>>,
    run_id: String,
    nodes: RefCell<BTreeMap<String, NodeCarve>>,
    paused: RefCell<BTreeMap<String, ArgoPaused>>,
    lease_request: Option<LeaseRequest>,
    lease: RefCell<Option<(LeaseRef, Instant)>>,
    plan: RefCell<Option<(std::path::PathBuf, RunPlan)>>,
}

impl<'a> OperatorEnvironment<'a> {
    #[must_use]
    pub fn new(op: Operator<'a>) -> Self {
        Self {
            op,
            natural: RefCell::new(None),
            started_at: now(),
            runs: RefCell::new(Vec::new()),
            stop_timeout: Duration::from_secs(600),
            stop_poll: Duration::from_secs(10),
            transient_patience: Duration::from_secs(45 * 60),
            transient_pause: Duration::from_secs(30),
            recorders: RefCell::new(Vec::new()),
            recorded: RefCell::new(BTreeMap::new()),
            exec_recorded: RefCell::new(BTreeMap::new()),
            guard: RefCell::new(None),
            created: RefCell::new(BTreeMap::new()),
            exec_ran: RefCell::new(BTreeMap::new()),
            run_id: super::lease::run_id("visita", chrono::Utc::now()),
            nodes: RefCell::new(BTreeMap::new()),
            paused: RefCell::new(BTreeMap::new()),
            lease_request: None,
            lease: RefCell::new(None),
            plan: RefCell::new(None),
        }
    }

    #[must_use]
    pub fn with_run(mut self, run: &str) -> Self {
        self.run_id = run.to_string();
        self
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    #[must_use]
    pub fn with_lease(mut self, req: LeaseRequest) -> Self {
        self.lease_request = Some(req);
        self
    }

    #[must_use]
    pub fn with_plan(self, dir: std::path::PathBuf, plan: RunPlan) -> Self {
        *self.plan.borrow_mut() = Some((dir, plan));
        self
    }

    #[must_use]
    pub fn ledger(&self) -> Value {
        serde_json::json!({
            "created": serde_json::to_value(&*self.created.borrow()).unwrap_or_default(),
            "execRan": serde_json::to_value(&*self.exec_ran.borrow()).unwrap_or_default(),
            "nodes": serde_json::to_value(&*self.nodes.borrow()).unwrap_or_default(),
            "paused": serde_json::to_value(&*self.paused.borrow()).unwrap_or_default(),
        })
    }

    pub fn load_ledger(&self, v: &Value) -> Result<(), String> {
        let field = |k: &str| {
            v.get(k)
                .cloned()
                .filter(|x| !x.is_null())
                .unwrap_or_else(|| serde_json::json!({}))
        };
        *self.created.borrow_mut() =
            serde_json::from_value(field("created")).map_err(|e| format!("ledger created: {e}"))?;
        *self.exec_ran.borrow_mut() =
            serde_json::from_value(field("execRan")).map_err(|e| format!("ledger execRan: {e}"))?;
        *self.nodes.borrow_mut() =
            serde_json::from_value(field("nodes")).map_err(|e| format!("ledger nodes: {e}"))?;
        *self.paused.borrow_mut() =
            serde_json::from_value(field("paused")).map_err(|e| format!("ledger paused: {e}"))?;
        Ok(())
    }

    pub fn track(&self, placed: &[&Fixture], finished: bool) {
        let ledger = self.ledger();
        let lease = self.lease.borrow().as_ref().map(|(r, _)| r.clone());
        let mut slot = self.plan.borrow_mut();
        let Some((dir, plan)) = slot.as_mut() else {
            return;
        };
        plan.placed = placed.iter().map(|f| f.name.clone()).collect();
        plan.ledger = ledger;
        plan.finished = finished;
        if lease.is_some() {
            plan.lease = lease;
        }
        if let Err(e) = super::lease::write_plan(dir, plan) {
            self.op
                .evidence()
                .note(&serde_json::json!({"at": now(), "plan": "unwritten", "error": e}));
        }
    }

    pub fn adopt_lease(&self, r: LeaseRef) {
        *self.lease.borrow_mut() = Some((r, Instant::now()));
    }

    pub fn leases(&self, ctx: &str, ns: &str) -> Result<Vec<Value>, String> {
        let v = self.op.json(
            "lease:list",
            &Verb::Get {
                context: ctx.into(),
                namespace: ns.into(),
                resources: "leases.coordination.k8s.io".into(),
                selector: Some(format!("{}=visita", super::lease::LEASE_LABEL)),
            },
        )?;
        Ok(list_items(&v))
    }

    pub fn acquire_lease(&self) -> Result<(), String> {
        let Some(req) = &self.lease_request else {
            return Ok(());
        };
        let leases = self.leases(&req.context, &req.namespace)?;
        let clash = super::lease::conflicts(&leases, req, chrono::Utc::now());
        if !clash.is_empty() {
            return Err(format!("refused before any write: {}", clash.join("; ")));
        }
        let obj = req.object(chrono::Utc::now());
        let e = self.stage_create("lease:create", &req.context, &req.namespace, &[obj])?;
        if !e.ok() {
            return Err(format!(
                "refused before any write: the lease could not be created: exit {}: {}",
                e.status,
                e.stderr.trim()
            ));
        }
        let uid = serde_json::from_str::<Value>(&e.stdout).ok().and_then(|v| {
            list_items(&v).first().and_then(|o| {
                o.pointer("/metadata/uid")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        });
        *self.lease.borrow_mut() = Some((
            LeaseRef {
                context: req.context.clone(),
                namespace: req.namespace.clone(),
                name: req.name.clone(),
                uid,
                holder: req.holder.clone(),
            },
            Instant::now(),
        ));
        self.op.evidence().note(
            &serde_json::json!({"at": now(), "lease": "acquired", "name": req.name, "holder": req.holder, "scope": req.scope}),
        );
        Ok(())
    }

    pub fn renew_lease(&self) {
        let Some(req) = &self.lease_request else {
            return;
        };
        let Some((r, at)) = self.lease.borrow().clone() else {
            return;
        };
        if at.elapsed() < Duration::from_secs((req.ttl_secs / 3).max(1)) {
            return;
        }
        let ok = self
            .act(
                "lease:renew",
                &Verb::MergePatch {
                    context: r.context.clone(),
                    namespace: r.namespace.clone(),
                    resource: "leases.coordination.k8s.io".into(),
                    name: r.name.clone(),
                    patch: req.renew_patch(chrono::Utc::now()).to_string(),
                    subresource: None,
                },
            )
            .is_ok();
        if ok {
            *self.lease.borrow_mut() = Some((r, Instant::now()));
        }
    }

    pub fn release_lease(&self) -> Result<(), String> {
        let Some((r, _)) = self.lease.borrow().clone() else {
            return Ok(());
        };
        let o = ObjectRef {
            api_version: "coordination.k8s.io/v1".into(),
            kind: "Lease".into(),
            name: r.name.clone(),
            uid: r.uid.clone(),
        };
        match self.read_object("lease:read", &r.context, &r.namespace, &o)? {
            None => {}
            Some(live)
                if !o.is(&live) || super::lease::holder(&live) != Some(r.holder.as_str()) =>
            {
                return Err(format!(
                    "lease `{}` is no longer the one this run created; left alone",
                    r.name
                ));
            }
            Some(_) => {
                let e = self.op.run(
                    "lease:release",
                    &Verb::Delete {
                        context: r.context.clone(),
                        namespace: r.namespace.clone(),
                        resource: o.resource(),
                        name: r.name.clone(),
                    },
                );
                if !e.ok() && !super::command::vanished(&e.stderr) {
                    return Err(format!(
                        "lease `{}`: delete exit {}: {}",
                        r.name,
                        e.status,
                        e.stderr.trim()
                    ));
                }
            }
        }
        *self.lease.borrow_mut() = None;
        self.op
            .evidence()
            .note(&serde_json::json!({"at": now(), "lease": "released", "name": r.name}));
        Ok(())
    }

    /// Every object the named `:apply-manifest` fixtures created and have
    /// not yet deleted, keyed as the snapshot keys it.
    #[must_use]
    pub fn held_objects(&self, fixtures: &[&Fixture]) -> Vec<ObjectKey> {
        let created = self.created.borrow();
        fixtures
            .iter()
            .filter_map(|f| created.get(&format!("place:{}", f.name)))
            .flat_map(|c| {
                c.objects.iter().map(|o| ObjectKey {
                    context: c.context.clone(),
                    namespace: c.namespace.clone(),
                    kind: o.kind.clone(),
                    name: o.name.clone(),
                })
            })
            .collect()
    }

    /// Start every record (rollout or exec metric) that declares
    /// `:record-from <fixture>`, taking the first sample now — BEFORE the
    /// fixture is placed, so the record holds the state the change starts from.
    pub fn begin_recording(&self, fixture: &str, assertions: &[AssertionSpec]) {
        for a in assertions {
            if matches!(
                a.kind,
                AssertionKind::RolloutSurgeBound | AssertionKind::ExecMetric
            ) && a.operator.record_from.as_deref() == Some(fixture)
            {
                self.start_recorder(a);
            }
        }
    }

    fn start_recorder(&self, a: &AssertionSpec) {
        if self.recorders.borrow().iter().any(|r| r.assertion == a.id) {
            return;
        }
        let interval = duration(a.operator.interval.as_deref(), Duration::from_secs(2))
            .unwrap_or(Duration::from_secs(2));
        let kind = match a.kind {
            AssertionKind::ExecMetric => RecordKind::Exec {
                pod: a.operator.pod.clone().unwrap_or_default(),
                container: a.operator.container.clone().unwrap_or_default(),
                argv: a.command.clone().unwrap_or_default(),
            },
            _ => RecordKind::Rollout {
                names: a.operator.names.clone().unwrap_or_default(),
            },
        };
        self.recorders.borrow_mut().push(Recorder {
            assertion: a.id.clone(),
            context: a.operator.context.clone().unwrap_or_default(),
            namespace: a
                .target_namespace
                .clone()
                .unwrap_or_else(|| "default".into()),
            kind,
            interval,
            started: Instant::now(),
            last: None,
            active: true,
        });
        self.op
            .evidence()
            .note(&serde_json::json!({"at": now(), "recording": a.id, "started": true}));
        self.tick();
    }

    fn stop_recorder(&self, id: &str) {
        for r in self
            .recorders
            .borrow_mut()
            .iter_mut()
            .filter(|r| r.assertion == id)
        {
            r.active = false;
        }
    }

    fn record_started(&self, id: &str) -> Option<Instant> {
        self.recorders
            .borrow()
            .iter()
            .find(|r| r.assertion == id)
            .map(|r| r.started)
    }

    /// Start the guard: read every watched pod now, fixing the restart counts
    /// the run must not exceed and, for every container with a liveness
    /// probe, the failures already on record.
    ///
    /// # Errors
    ///
    /// A watched pod that cannot be read — a guard over nothing guards nothing
    /// — or, when a watched pod has a liveness probe, events that cannot be.
    pub fn start_guard(&self, g: &super::spec::Guard) -> Result<(), String> {
        let pods = self.read_pods(&g.context, &g.namespace, &g.pods, "guard:start")?;
        let restarts: BTreeMap<String, i64> = pods
            .iter()
            .map(|(n, p)| (n.clone(), restart_count(p)))
            .collect();
        for n in &g.pods {
            if !restarts.contains_key(n) {
                return Err(format!("guard: pod `{n}` not found in {}", g.namespace));
            }
        }
        let mut liveness: Vec<LivenessWatch> = Vec::new();
        for (name, pod) in pods.iter().filter(|(n, _)| g.pods.contains(n)) {
            let probes = super::guard::liveness_probes(pod);
            let sole = probes.len() == 1;
            liveness.extend(probes.into_iter().map(|probe| LivenessWatch {
                pod: name.clone(),
                probe,
                sole,
                streak: LivenessStreak::default(),
            }));
        }
        if !liveness.is_empty() {
            let on_record = self.read_liveness(&g.context, &g.namespace, "guard:start")?;
            for w in &mut liveness {
                w.streak = LivenessStreak::baseline(on_record.iter().filter(|f| w.owns(f)));
            }
        }
        self.op.evidence().note(&serde_json::json!({
            "at": now(), "guard": "started",
            "liveness": liveness.iter().map(|w| serde_json::json!({"pod": w.pod, "probe": w.probe})).collect::<Vec<_>>(),
        }));
        *self.guard.borrow_mut() = Some(GuardState {
            spec: g.clone(),
            interval: duration(g.interval.as_deref(), Duration::from_secs(5))?,
            max_unready: duration(Some(&g.max_unready), Duration::from_secs(60))?,
            margin: g.liveness_margin.unwrap_or(2),
            last: Some(Instant::now()),
            restarts,
            unready_since: BTreeMap::new(),
            liveness,
            probes: 1,
            tripped: None,
        });
        Ok(())
    }

    fn read_liveness(
        &self,
        ctx: &str,
        ns: &str,
        label: &str,
    ) -> Result<Vec<LivenessFailure>, String> {
        let events = self.op.json(
            &format!("{label}:liveness"),
            &Verb::Events {
                context: ctx.into(),
                namespace: ns.into(),
                reason: "Unhealthy".into(),
            },
        )?;
        Ok(super::guard::liveness_failures(&events))
    }

    /// Why the guard tripped, if it did.
    #[must_use]
    pub fn guard_tripped(&self) -> Option<String> {
        self.guard_trip().map(|t| t.to_string())
    }

    /// Whether placing `f` wrote to pod `pod` in `ctx`/`ns`.
    #[must_use]
    pub fn touches_pod(&self, f: &Fixture, ctx: &str, ns: &str, pod: &str) -> bool {
        let exec = self
            .exec_ran
            .borrow()
            .get(&format!("place:{}", f.name))
            .is_some_and(|r| {
                r.context == ctx && r.namespace == ns && r.pods.iter().any(|p| p == pod)
            });
        exec || f
            .action
            .as_ref()
            .and_then(Action::target)
            .is_some_and(|(c, n, resource, name)| {
                c == ctx && n == ns && kind_of(resource) == "Pod" && name == pod
            })
    }

    /// The guard's trip, typed: which pod, which signal, when.
    #[must_use]
    pub fn guard_trip(&self) -> Option<GuardTrip> {
        self.guard.borrow().as_ref().and_then(|g| g.tripped.clone())
    }

    /// `(probes taken, pods watched)` — the guard's witness for the report.
    #[must_use]
    pub fn guard_summary(&self) -> Option<(usize, Vec<String>)> {
        self.guard
            .borrow()
            .as_ref()
            .map(|g| (g.probes, g.spec.pods.clone()))
    }

    fn read_pods(
        &self,
        ctx: &str,
        ns: &str,
        names: &[String],
        label: &str,
    ) -> Result<Vec<(String, Value)>, String> {
        let list = self.op.json(
            label,
            &Verb::GetNamed {
                context: ctx.into(),
                namespace: ns.into(),
                resource: "pod".into(),
                names: names.to_vec(),
            },
        )?;
        Ok(list_items(&list)
            .into_iter()
            .filter_map(|p| {
                p.pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .map(|n| (n.to_string(), p.clone()))
            })
            .collect())
    }

    fn probe_guard(&self) {
        let Some(g) = self.guard.borrow().clone() else {
            return;
        };
        if g.tripped.is_some() || g.last.is_some_and(|l| l.elapsed() < g.interval) {
            return;
        }
        let read = self.read_pods(
            &g.spec.context,
            &g.spec.namespace,
            &g.spec.pods,
            "guard:probe",
        );
        let liveness = (!g.liveness.is_empty())
            .then(|| self.read_liveness(&g.spec.context, &g.spec.namespace, "guard:probe"));
        let mut state = self.guard.borrow_mut();
        let Some(st) = state.as_mut() else { return };
        st.last = Some(Instant::now());
        st.probes += 1;
        // A probe that cannot read is NOT a healthy probe: it counts toward
        // unready for every watched pod, so a blind guard cannot keep a run
        // going on a starved broker.
        let pods: BTreeMap<String, Value> = read.unwrap_or_default().into_iter().collect();
        for name in st.spec.pods.clone() {
            let (ready, restarts) = pods
                .get(&name)
                .map_or((false, None), |p| (pod_ready(p), Some(restart_count(p))));
            if let Some(r) = restarts {
                let base = st.restarts.get(&name).copied().unwrap_or(r);
                if r > base {
                    st.trip(&name, GuardSignal::Restarted { from: base, to: r });
                }
            }
            if ready {
                st.unready_since.remove(&name);
            } else {
                let since = *st
                    .unready_since
                    .entry(name.clone())
                    .or_insert_with(Instant::now);
                if since.elapsed() >= st.max_unready {
                    st.trip(
                        &name,
                        GuardSignal::Unready {
                            for_secs: since.elapsed().as_secs(),
                            bound_secs: st.max_unready.as_secs(),
                        },
                    );
                }
            }
        }
        match liveness {
            Some(Ok(failures)) => {
                let margin = st.margin;
                let mut trips = Vec::new();
                for w in &mut st.liveness {
                    let own: Vec<&LivenessFailure> =
                        failures.iter().filter(|f| w.owns(f)).collect();
                    let streak = w.streak.observe(own, w.probe.period_seconds);
                    if streak >= w.probe.trips_at(margin) {
                        trips.push((
                            w.pod.clone(),
                            GuardSignal::Liveness {
                                container: w.probe.container.clone(),
                                failures: streak,
                                threshold: w.probe.failure_threshold,
                                margin,
                            },
                        ));
                    }
                }
                for (pod, signal) in trips {
                    st.trip(&pod, signal);
                }
            }
            Some(Err(why)) => {
                self.op.evidence().note(
                    &serde_json::json!({"at": now(), "guard": "liveness-unread", "reason": why}),
                );
            }
            None => {}
        }
        if let Some(trip) = &st.tripped {
            self.op.evidence().note(&serde_json::json!({"at": now(), "guard": "tripped", "trip": trip, "reason": trip.to_string()}));
        }
    }

    /// Take a sample for every active record whose interval has elapsed, and
    /// probe the guard when due. Every wait this environment makes goes
    /// through [`Self::pause`] (or [`Self::pause_unguarded`]), which calls
    /// this — so recording and guarding continue through an awaited dispatch,
    /// a resize's convergence and a `:wait` step. Cooperative, not a thread: a
    /// single slow read delays the next sample, and the samples' timestamps
    /// show it.
    pub fn tick(&self) {
        self.renew_lease();
        self.probe_guard();
        let due: Vec<usize> = self
            .recorders
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.active && r.last.is_none_or(|l| l.elapsed() >= r.interval))
            .map(|(i, _)| i)
            .collect();
        for i in due {
            let r = self.recorders.borrow()[i].clone();
            self.recorders.borrow_mut()[i].last = Some(Instant::now());
            let at = now();
            match &r.kind {
                RecordKind::Rollout { names } => {
                    let Ok(list) = self.op.json(
                        &format!("{}:sample", r.assertion),
                        &Verb::GetNamed {
                            context: r.context.clone(),
                            namespace: r.namespace.clone(),
                            resource: "deployment".into(),
                            names: names.clone(),
                        },
                    ) else {
                        continue;
                    };
                    let samples: Vec<RolloutSample> = list_items(&list)
                        .iter()
                        .filter(|d| {
                            d.pointer("/metadata/name")
                                .and_then(Value::as_str)
                                .is_some_and(|n| names.iter().any(|x| x == n))
                        })
                        .map(|d| RolloutSample::from_deployment(d, &at))
                        .collect();
                    let lines: String = samples
                        .iter()
                        .filter_map(|s| serde_json::to_string(s).ok())
                        .map(|l| l + "\n")
                        .collect();
                    let _ = self
                        .op
                        .evidence()
                        .append(&format!("rollout-{}.jsonl", r.assertion), lines.as_bytes());
                    self.recorded
                        .borrow_mut()
                        .entry(r.assertion.clone())
                        .or_default()
                        .extend(samples);
                }
                RecordKind::Exec {
                    pod,
                    container,
                    argv,
                } => {
                    let e = self.op.run(
                        &format!("{}:sample", r.assertion),
                        &Verb::ExecRead {
                            context: r.context.clone(),
                            namespace: r.namespace.clone(),
                            pod: pod.clone(),
                            container: container.clone(),
                            argv: argv.clone(),
                        },
                    );
                    let sample = ExecSample {
                        at,
                        value: if e.ok() {
                            parse_number(&e.stdout)
                        } else {
                            None
                        },
                        raw: if e.ok() {
                            e.stdout.trim().to_string()
                        } else {
                            format!("exit {}: {}", e.status, e.stderr.trim())
                        },
                    };
                    if let Ok(line) = serde_json::to_string(&sample) {
                        let _ = self.op.evidence().append(
                            &format!("exec-{}.jsonl", r.assertion),
                            format!("{line}\n").as_bytes(),
                        );
                    }
                    self.exec_recorded
                        .borrow_mut()
                        .entry(r.assertion.clone())
                        .or_default()
                        .push(sample);
                }
            }
        }
    }

    fn sleep_slice(&self, d: Duration, end: Instant) {
        let now_i = Instant::now();
        let guard_iv = self.guard.borrow().as_ref().map(|g| g.interval);
        let slice = self
            .recorders
            .borrow()
            .iter()
            .filter(|r| r.active)
            .map(|r| r.interval)
            .chain(guard_iv)
            .min()
            .unwrap_or(d)
            .max(Duration::from_millis(50));
        std::thread::sleep(slice.min(end.saturating_duration_since(now_i)));
    }

    /// Wait `d`, sampling every active record and probing the guard meanwhile.
    /// Always ticks at least once, so a zero-length wait still samples.
    ///
    /// # Errors
    ///
    /// The guard's reason, the moment it trips — every guarded wait stops.
    pub fn pause(&self, d: Duration) -> Result<(), String> {
        let end = Instant::now() + d;
        loop {
            self.tick();
            if let Some(why) = self.guard_tripped() {
                return Err(why);
            }
            if Instant::now() >= end {
                return Ok(());
            }
            self.sleep_slice(d, end);
        }
    }

    /// [`Self::pause`] that keeps going after the guard trips — for the waits
    /// that PUT THINGS BACK (stopping a run, a resize converging back), which
    /// must finish precisely when the guard has tripped.
    pub fn pause_unguarded(&self, d: Duration) {
        let end = Instant::now() + d;
        loop {
            self.tick();
            if Instant::now() >= end {
                return;
            }
            self.sleep_slice(d, end);
        }
    }

    /// How long stopping a dispatched run may take, and how often to look.
    #[must_use]
    pub fn with_stop_bounds(mut self, timeout: Duration, poll: Duration) -> Self {
        self.stop_timeout = timeout;
        self.stop_poll = poll;
        self
    }

    /// How long a GitHub read or a cancel that is rate limited or answered 5xx
    /// is retried (every `pause`) before it counts as a failure. A rate limit
    /// resets within the hour; aborting at it would orphan a dispatched run.
    #[must_use]
    pub fn with_transient_bounds(mut self, patience: Duration, pause: Duration) -> Self {
        self.transient_patience = patience;
        self.transient_pause = pause;
        self
    }

    /// Every run this trial dispatched.
    #[must_use]
    pub fn dispatched(&self) -> Vec<Dispatched> {
        self.runs.borrow().clone()
    }

    fn view(&self, step: &str, d: &Dispatched) -> Result<Value, String> {
        self.retrying(step, true, || {
            self.op.json(
                step,
                &Verb::RunView {
                    repo: d.repo.clone(),
                    run_id: d.run_id.clone(),
                },
            )
        })
    }

    /// Run a GitHub call, and while it fails because GitHub is rate limiting or
    /// answering 5xx, wait and run it again, for at most `transient_patience`.
    /// Any other failure is returned at once. `guarded` waits stop when the
    /// guard trips; the waits that put things back do not.
    fn retrying<T>(
        &self,
        step: &str,
        guarded: bool,
        mut call: impl FnMut() -> Result<T, String>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + self.transient_patience;
        let mut waits = 0usize;
        loop {
            match call() {
                Err(e) if transient(&e) && Instant::now() < deadline => {
                    waits += 1;
                    self.op.evidence().note(&serde_json::json!({
                        "at": now(), "step": step, "transient": e.lines().next().unwrap_or_default(), "wait": waits,
                    }));
                    if guarded {
                        self.pause(self.transient_pause)
                            .map_err(|why| format!("{step}: {why}"))?;
                    } else {
                        self.pause_unguarded(self.transient_pause);
                    }
                }
                other => return other,
            }
        }
    }

    /// Read whether `branch` contains every commit of `trunk`, through
    /// GitHub's compare API (`<trunk>...<branch>`: `behind_by` is how many
    /// trunk commits the branch lacks).
    ///
    /// # Errors
    ///
    /// A branch behind its trunk — naming it and by how many commits — or a
    /// compare that could not be read (never assumed to pass).
    pub fn check_branch(&self, step: &str, b: &super::spec::BranchContains) -> Result<(), String> {
        let v = self.op.json(
            &format!("{step}:branch-contains"),
            &Verb::GhApi {
                path: format!("repos/{}/compare/{}...{}", b.repo, b.trunk, b.branch),
            },
        )?;
        let behind = v.get("behind_by").and_then(Value::as_i64).ok_or_else(|| {
            format!(
                "{step}: compare {}...{} in {} carries no behind_by",
                b.trunk, b.branch, b.repo
            )
        })?;
        let ahead = v.get("ahead_by").and_then(Value::as_i64).unwrap_or(0);
        self.op.evidence().note(&serde_json::json!({
            "at": now(), "step": step, "branch_contains": b, "behind_by": behind, "ahead_by": ahead,
        }));
        if behind > 0 {
            return Err(format!(
                "{step}: branch `{}` is {behind} commit(s) behind `{}` in {} — deploying it would revert what landed on the trunk since; rebase it first",
                b.branch, b.trunk, b.repo
            ));
        }
        Ok(())
    }

    /// Stop one dispatched run: cancel it if it is still in flight, then wait
    /// (bounded) until it is not.
    ///
    /// # Errors
    ///
    /// When the run cannot be read, or is still in flight at the bound.
    pub fn stop_run(&self, d: &Dispatched) -> Result<(), String> {
        let step = format!("stop:{}", d.fixture);
        let v = self.stop_view(&step, d)?;
        if !super::pipeline::in_flight(&v) {
            return Ok(());
        }
        self.retrying(&step, false, || {
            self.act(
                &step,
                &Verb::RunCancel {
                    repo: d.repo.clone(),
                    run_id: d.run_id.clone(),
                },
            )
        })?;
        let deadline = Instant::now() + self.stop_timeout;
        loop {
            let v = self.stop_view(&step, d)?;
            if !super::pipeline::in_flight(&v) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "run {} ({}) still in flight {}s after cancel",
                    d.run_id,
                    d.fixture,
                    self.stop_timeout.as_secs()
                ));
            }
            self.pause_unguarded(self.stop_poll);
        }
    }

    fn stop_view(&self, step: &str, d: &Dispatched) -> Result<Value, String> {
        self.retrying(step, false, || {
            self.op.json(
                step,
                &Verb::RunView {
                    repo: d.repo.clone(),
                    run_id: d.run_id.clone(),
                },
            )
        })
    }

    /// Stop every run this trial dispatched. Returns the ones that would not
    /// stop — while any is listed, nothing may be put back, because the run
    /// can still change what the undo restores.
    #[must_use]
    pub fn stop_runs(&self) -> Vec<String> {
        self.dispatched()
            .iter()
            .filter_map(|d| self.stop_run(d).err())
            .collect()
    }

    fn await_run(
        &self,
        step: &str,
        d: &Dispatched,
        until: Option<&super::spec::Await>,
        accept: &[String],
        timeout: Option<&str>,
        poll: Option<&str>,
    ) -> Result<(), String> {
        let timeout = duration(timeout, Duration::from_secs(3600))?;
        let poll = duration(poll, Duration::from_secs(20))?;
        let deadline = Instant::now() + timeout;
        loop {
            let v = self.view(&format!("{step}:await"), d)?;
            match super::pipeline::progress(&v, until, accept) {
                super::pipeline::Progress::Reached => {
                    self.op.evidence().note(&serde_json::json!({
                        "at": now(), "step": step, "run": d.run_id, "reached": format!("{until:?}"),
                    }));
                    return Ok(());
                }
                super::pipeline::Progress::Failed(why) => {
                    return Err(format!("{step}: run {} {why}", d.run_id));
                }
                super::pipeline::Progress::Pending => {}
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{step}: run {} did not reach {until:?} within {}s",
                    d.run_id,
                    timeout.as_secs()
                ));
            }
            self.pause(poll).map_err(|why| format!("{step}: {why}"))?;
        }
    }

    /// Running pods behind `selector`, minus any already terminating, and —
    /// when `expect` is given — only those whose image tag starts with it.
    /// Returns the kept pod names and a note on what was left out.
    fn select_pods(
        &self,
        label: &str,
        ctx: &str,
        ns: &str,
        selector: &str,
        expect: Option<&str>,
    ) -> Result<(Vec<String>, String), String> {
        let pods = self.op.json(
            label,
            &Verb::Get {
                context: ctx.into(),
                namespace: ns.into(),
                resources: "pods".into(),
                selector: Some(selector.into()),
            },
        )?;
        let items = pods
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = items.len();
        let kept: Vec<String> = items
            .iter()
            .filter(|p| p.pointer("/metadata/deletionTimestamp").is_none())
            .filter(|p| p.pointer("/status/phase").and_then(Value::as_str) == Some("Running"))
            .filter(|p| {
                expect.is_none_or(|e| {
                    p.pointer("/spec/containers")
                        .and_then(Value::as_array)
                        .is_some_and(|cs| {
                            cs.iter().any(|c| {
                                c.get("image")
                                    .and_then(Value::as_str)
                                    .and_then(|i| i.rsplit(':').next())
                                    .is_some_and(|tag| tag.starts_with(e))
                            })
                        })
                })
            })
            .filter_map(|p| {
                p.pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        let note = format!(
            "{} of {total} pods selected (running, not terminating{})",
            kept.len(),
            expect.map_or(String::new(), |e| format!(", image `{e}*`"))
        );
        Ok((kept, note))
    }

    #[must_use]
    pub fn operator(&self) -> &Operator<'a> {
        &self.op
    }

    /// Hand the environment the captured natural state, which
    /// [`Cleanup::RestoreFromSnapshot`] inverses are derived from.
    pub fn set_natural(&self, n: NaturalState) {
        *self.natural.borrow_mut() = Some(n);
    }

    fn act(&self, step: &str, verb: &Verb) -> Result<(), String> {
        let e = self.op.run(step, verb);
        if e.ok() {
            Ok(())
        } else {
            Err(format!("{step}: exit {}: {}", e.status, e.stderr.trim()))
        }
    }

    pub fn check_absorb(&self, step: &str, a: &Action) -> Result<(), String> {
        let forward = match a {
            Action::Annotate {
                key,
                value,
                absorb: true,
                ..
            } => super::absorb::annotation_patch(key, value),
            Action::Patch {
                patch,
                absorb: true,
                ..
            } => serde_json::from_str(patch)
                .map_err(|e| format!("{step}: the patch is not JSON: {e}"))?,
            _ => return Ok(()),
        };
        let Some((ctx, ns, resource, name)) = a.target() else {
            return Ok(());
        };
        let e = self.op.run(
            &format!("{step}:absorb"),
            &Verb::GetNamed {
                context: ctx.into(),
                namespace: ns.into(),
                resource: resource.into(),
                names: vec![name.into()],
            },
        );
        let live: Value = if e.ok() {
            serde_json::from_str(&e.stdout).map_err(|err| {
                format!("{step}: refused, the live {resource}/{name} read is not JSON: {err}")
            })?
        } else if super::command::vanished(&e.stderr) {
            return Ok(());
        } else {
            return Err(format!(
                "{step}: refused, :absorb could not read the live {resource}/{name} to see what it holds: exit {}: {}",
                e.status,
                e.stderr.trim()
            ));
        };
        let d = super::absorb::displaced(&forward, &live);
        if d.is_empty() {
            return Ok(());
        }
        self.op.evidence().note(
            &serde_json::json!({"at": now(), "step": step, "refused": "displaces", "held": d}),
        );
        Err(format!(
            "refused, :absorb occupies only what the environment does not hold, and it holds {}",
            super::absorb::describe(&d)
        ))
    }

    fn place(&self, step: &str, a: &Action) -> Result<(), String> {
        match a {
            Action::Annotate {
                context,
                namespace,
                resource,
                name,
                key,
                value,
                ..
            } => self.act(
                step,
                &Verb::Annotate {
                    context: context.clone(),
                    namespace: namespace.clone(),
                    resource: resource.clone(),
                    name: name.clone(),
                    key: key.clone(),
                    value: Some(value.clone()),
                },
            ),
            Action::Patch {
                context,
                namespace,
                resource,
                name,
                patch,
                ..
            } => self.act(
                step,
                &Verb::MergePatch {
                    context: context.clone(),
                    namespace: namespace.clone(),
                    resource: resource.clone(),
                    name: name.clone(),
                    patch: patch.clone(),
                    subresource: None,
                },
            ),
            Action::DispatchWorkflow {
                repo,
                workflow,
                git_ref,
                inputs,
                until,
                accept_failure,
                timeout,
                poll,
            } => {
                let e = self.op.run(
                    step,
                    &Verb::WorkflowRun {
                        repo: repo.clone(),
                        workflow: workflow.clone(),
                        git_ref: git_ref.clone(),
                        inputs: inputs
                            .iter()
                            .map(|i| (i.name.clone(), i.value.clone()))
                            .collect(),
                    },
                );
                if !e.ok() {
                    return Err(format!("{step}: exit {}: {}", e.status, e.stderr.trim()));
                }
                // The run is in flight from here on. Without its id it can be
                // neither awaited nor stopped, so the step fails loudly — and
                // the operator is told there is an untracked run to cancel.
                let run_id = super::pipeline::run_id(&e.stdout).ok_or_else(|| {
                    format!(
                        "{step}: dispatched, but the output carries no run URL, so the run can be neither awaited nor stopped — find and cancel it by hand: {}",
                        e.stdout.trim()
                    )
                })?;
                let d = Dispatched {
                    fixture: step.to_string(),
                    repo: repo.clone(),
                    run_id,
                };
                self.runs.borrow_mut().push(d.clone());
                self.await_run(
                    step,
                    &d,
                    until.as_ref(),
                    accept_failure,
                    timeout.as_deref(),
                    poll.as_deref(),
                )
            }
            Action::ArgoSync {
                context,
                namespace,
                application,
                revision,
                sync_options,
                initiated_by,
            } => {
                // No options declared ⇒ carry the app's own, as the argocd CLI
                // would: a sync that drops `ServerSideApply=true` is a
                // different sync from the one the app normally gets.
                let sync_options = if sync_options.is_empty() {
                    let apps = self.op.json(
                        &format!("{step}:read-sync-options"),
                        &Verb::Get {
                            context: context.clone(),
                            namespace: namespace.clone(),
                            resources: "applications.argoproj.io".into(),
                            selector: None,
                        },
                    )?;
                    apps.get("items")
                        .and_then(Value::as_array)
                        .and_then(|items| {
                            items.iter().find(|a| {
                                a.pointer("/metadata/name").and_then(Value::as_str)
                                    == Some(application.as_str())
                            })
                        })
                        .ok_or_else(|| {
                            format!("{step}: no Application `{application}` in {namespace}")
                        })?
                        .pointer("/spec/syncPolicy/syncOptions")
                        .and_then(Value::as_array)
                        .map(|o| {
                            o.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    sync_options.clone()
                };
                let patch = serde_json::json!({
                    "operation": {
                        "initiatedBy": { "username": initiated_by.clone().unwrap_or_else(|| "operator".into()) },
                        "sync": { "revision": revision, "syncOptions": sync_options },
                    }
                });
                self.act(
                    step,
                    &Verb::MergePatch {
                        context: context.clone(),
                        namespace: namespace.clone(),
                        resource: "applications.argoproj.io".into(),
                        name: application.clone(),
                        patch: patch.to_string(),
                        subresource: None,
                    },
                )
            }
            Action::ResizePod {
                context,
                namespace,
                pod,
                container,
                cpu_limit,
                cpu_request,
                allow_restart,
                timeout,
            } => self.resize_place(
                step,
                &ResizeTarget {
                    context,
                    namespace,
                    pod,
                    container,
                },
                cpu_limit.as_deref(),
                cpu_request.as_deref(),
                *allow_restart,
                timeout.as_deref(),
            ),
            Action::RolloutRestart {
                context,
                namespace,
                deployments,
            } => self.act(
                step,
                &Verb::RolloutRestart {
                    context: context.clone(),
                    namespace: namespace.clone(),
                    deployments: deployments.clone(),
                },
            ),
            Action::RabbitmqCloseConnections {
                context,
                broker_namespace,
                broker_pod,
                broker_container,
                vhost,
                app_namespace,
                app_selector,
            } => {
                let pods = self.op.json(
                    &format!("{step}:read-peer-ips"),
                    &Verb::Get {
                        context: context.clone(),
                        namespace: app_namespace.clone(),
                        resources: "pods".into(),
                        selector: Some(app_selector.clone()),
                    },
                )?;
                let ips: Vec<String> = pods
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|p| p.pointer("/status/podIP").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if ips.is_empty() {
                    return Err(format!(
                        "{step}: `{app_selector}` selects no pod with an IP"
                    ));
                }
                let tuples = ips
                    .iter()
                    .map(|i| judge::ipv4_mapped_tuple(i))
                    .collect::<Result<Vec<_>, _>>()?;
                let expr = judge::close_connections_expr(&tuples, vhost);
                let e = self.op.run(
                    step,
                    &Verb::Exec {
                        context: context.clone(),
                        namespace: broker_namespace.clone(),
                        pod: broker_pod.clone(),
                        container: broker_container.clone(),
                        argv: vec!["rabbitmqctl".into(), "eval".into(), expr],
                    },
                );
                if !e.ok() {
                    return Err(format!("{step}: exit {}: {}", e.status, e.stderr.trim()));
                }
                let closed: u64 = e.stdout.trim().parse().unwrap_or(0);
                self.op.evidence().note(&serde_json::json!({
                    "at": now(), "step": step, "fault": "rabbitmq-close-connections",
                    "vhost": vhost, "peers": ips, "closed": closed,
                }));
                if closed == 0 {
                    return Err(format!(
                        "{step}: closed 0 connections on {vhost} from {ips:?}; a fault that did nothing is not evidence of recovery"
                    ));
                }
                Ok(())
            }
            Action::ApplyManifest {
                context,
                namespace,
                file,
                yaml,
                await_available,
                timeout,
            } => {
                let objects = super::manifest::source(file.as_deref(), yaml.as_deref())
                    .and_then(|text| super::manifest::objects(&text, namespace))
                    .map_err(|e| format!("{step}: {e}"))?;
                let timeout = duration(timeout.as_deref(), Duration::from_secs(120))?;
                self.create_objects(step, context, namespace, &objects, timeout)?;
                self.await_available(step, context, namespace, await_available, timeout)
            }
            Action::Exec {
                context,
                namespace,
                pod,
                selector,
                container,
                all,
                argv,
                ..
            } => {
                let pods = match (pod, selector) {
                    (Some(p), _) => vec![p.clone()],
                    (None, Some(sel)) => self.ready_pods(step, context, namespace, sel, *all)?,
                    (None, None) => {
                        return Err(format!("{step}: an exec names no :pod or :selector"));
                    }
                };
                let mut ran = ExecRan {
                    context: context.clone(),
                    namespace: namespace.clone(),
                    container: container.clone(),
                    pods: Vec::new(),
                };
                for p in &pods {
                    ran.pods.push(p.clone());
                    self.exec_ran
                        .borrow_mut()
                        .insert(step.to_string(), ran.clone());
                    self.act(
                        step,
                        &Verb::Exec {
                            context: context.clone(),
                            namespace: namespace.clone(),
                            pod: p.clone(),
                            container: container.clone(),
                            argv: argv.clone(),
                        },
                    )?;
                }
                self.op.evidence().note(
                    &serde_json::json!({"at": now(), "step": step, "exec": argv, "pods": pods}),
                );
                Ok(())
            }
            Action::CarveNamespace {
                context,
                name,
                quota,
                limit_default,
                limit_default_request,
                timeout,
            } => {
                let objects = super::carve::namespace_objects(
                    name,
                    &self.run_id,
                    quota,
                    limit_default,
                    limit_default_request,
                );
                let timeout = duration(timeout.as_deref(), Duration::from_secs(300))?;
                self.create_objects(step, context, name, &objects, timeout)
            }
            Action::Fence {
                context,
                namespace,
                egress,
                ingress,
                timeout,
            } => {
                let egress = self.resolve_peers(step, context, egress)?;
                let ingress = self.resolve_peers(step, context, ingress)?;
                let objects = super::carve::fence_objects(&self.run_id, &egress, &ingress);
                let timeout = duration(timeout.as_deref(), Duration::from_secs(120))?;
                self.create_objects(step, context, namespace, &objects, timeout)
            }
            Action::CarveNode {
                context,
                selector,
                taint_key,
                taint_value,
                label_key,
                label_value,
            } => {
                let list = self.op.json(
                    &format!("{step}:nodes"),
                    &Verb::Get {
                        context: context.clone(),
                        namespace: "default".into(),
                        resources: "nodes".into(),
                        selector: Some(selector.clone()),
                    },
                )?;
                let nodes = list_items(&list);
                let (node, uid) = super::carve::pick_node(&nodes, taint_key, label_key)
                    .map_err(|e| format!("{step}: refused, nothing written — {e}"))?;
                let live = nodes
                    .iter()
                    .find(|n| {
                        n.pointer("/metadata/name").and_then(Value::as_str) == Some(node.as_str())
                    })
                    .cloned()
                    .unwrap_or_default();
                self.nodes.borrow_mut().insert(
                    step.to_string(),
                    NodeCarve {
                        context: context.clone(),
                        node: node.clone(),
                        uid,
                        taint_key: taint_key.clone(),
                        label_key: label_key.clone(),
                    },
                );
                let patch = super::carve::carve_node_patch(
                    &live,
                    taint_key,
                    taint_value.as_deref(),
                    label_key,
                    label_value,
                );
                self.act(
                    step,
                    &Verb::MergePatch {
                        context: context.clone(),
                        namespace: "default".into(),
                        resource: "node".into(),
                        name: node.clone(),
                        patch: patch.to_string(),
                        subresource: None,
                    },
                )?;
                self.op.evidence().note(
                    &serde_json::json!({"at": now(), "step": step, "carved-node": node, "taint": taint_key, "label": label_key}),
                );
                Ok(())
            }
            Action::ArgocdPause {
                context,
                namespace,
                application,
            } => self.argocd_pause(step, context, namespace, application),
            Action::DeleteRunnerPod {
                context,
                namespace,
                run_of,
                job,
                wait,
            } => self.delete_runner_pod(step, (context, namespace), (run_of, job), wait.as_deref()),
        }
    }

    fn dispatched_of(&self, step: &str, run_of: &str) -> Result<Dispatched, String> {
        let fixture = format!("place:{run_of}");
        self.dispatched()
            .into_iter()
            .find(|d| d.fixture == fixture)
            .ok_or_else(|| format!("{step}: `{run_of}` dispatched no run in this trial"))
    }

    fn run_jobs_read(&self, step: &str, d: &Dispatched) -> Result<Vec<judge::RunJob>, String> {
        let answer = self.retrying(step, true, || {
            self.op.json(
                step,
                &Verb::GhApi {
                    path: format!(
                        "repos/{}/actions/runs/{}/jobs?per_page=100",
                        d.repo, d.run_id
                    ),
                },
            )
        })?;
        Ok(judge::run_jobs(&answer))
    }

    fn running_job(
        &self,
        step: &str,
        d: &Dispatched,
        job: &str,
        wait: Option<&str>,
    ) -> Result<judge::RunJob, String> {
        let wait_text = wait.unwrap_or("0s");
        let deadline = Instant::now() + duration(Some(wait_text), Duration::ZERO)?;
        let mut polls = 0usize;
        loop {
            polls += 1;
            let jobs = self.run_jobs_read(&format!("{step}:jobs"), d)?;
            let seen = judge::job(&jobs, job);
            if let Some(j) = seen.filter(|j| j.status == "in_progress" && !j.runner.is_empty()) {
                return Ok(j.clone());
            }
            let now_i = Instant::now();
            if now_i >= deadline {
                return Err(match seen {
                    None => format!(
                        "job `{job}` is not in run {} after {polls} read(s) over {wait_text}",
                        d.run_id
                    ),
                    Some(j) => format!(
                        "job `{job}` is {} on runner `{}` after {polls} read(s) over {wait_text}; only the pod of our own running job may be deleted",
                        j.status, j.runner
                    ),
                });
            }
            self.pause(Duration::from_secs(10).min(deadline - now_i))?;
        }
    }

    fn delete_runner_pod(
        &self,
        step: &str,
        (ctx, ns): (&str, &str),
        (run_of, job): (&str, &str),
        wait: Option<&str>,
    ) -> Result<(), String> {
        let refused = |why: String| format!("{step}: refused, nothing written — {why}");
        let d = self.dispatched_of(step, run_of).map_err(refused)?;
        let j = self.running_job(step, &d, job, wait).map_err(refused)?;
        let pods = self
            .read_pods(
                ctx,
                ns,
                std::slice::from_ref(&j.runner),
                &format!("{step}:pod"),
            )
            .map_err(|e| {
                refused(format!(
                    "runner `{}` of job `{job}` could not be read in {ns}: {e}",
                    j.runner
                ))
            })?;
        let Some((pod, obj)) = pods.into_iter().find(|(n, _)| n == &j.runner) else {
            return Err(refused(format!(
                "runner `{}` of job `{job}` is not a pod in {ns}",
                j.runner
            )));
        };
        self.op.evidence().note(&serde_json::json!({
            "at": now(),
            "step": step,
            "runner-pod": pod,
            "uid": obj.pointer("/metadata/uid"),
            "node": obj.pointer("/spec/nodeName"),
            "job": job,
            "job-id": j.id,
            "run": d.run_id,
        }));
        self.act(
            step,
            &Verb::DeletePod {
                context: ctx.into(),
                namespace: ns.into(),
                pod,
            },
        )
    }

    fn resolve_peers(
        &self,
        step: &str,
        ctx: &str,
        peers: &[Peer],
    ) -> Result<Vec<super::carve::ResolvedPeer>, String> {
        let mut out = Vec::new();
        for p in peers {
            match p {
                Peer::Dns => out.push(super::carve::dns_peer()),
                Peer::Namespace(n) => out.push(super::carve::namespace_peer(n)),
                Peer::ApiServer => {
                    let ep = self.op.json(
                        &format!("{step}:api-server"),
                        &Verb::GetNamed {
                            context: ctx.into(),
                            namespace: "default".into(),
                            resource: "endpoints".into(),
                            names: vec!["kubernetes".into()],
                        },
                    )?;
                    let peers = super::carve::api_server_peers(&ep);
                    if peers.is_empty() {
                        return Err(format!(
                            "{step}: refused, nothing written — default/kubernetes has no endpoint address to admit"
                        ));
                    }
                    out.extend(peers);
                }
                Peer::Service { namespace, name } => {
                    let svc = self.op.json(
                        &format!("{step}:service"),
                        &Verb::GetNamed {
                            context: ctx.into(),
                            namespace: namespace.clone(),
                            resource: "service".into(),
                            names: vec![name.clone()],
                        },
                    )?;
                    out.push(super::carve::service_peer(&svc).map_err(|e| {
                        format!(
                            "{step}: refused, nothing written — service {namespace}/{name}: {e}"
                        )
                    })?);
                }
            }
        }
        Ok(out)
    }

    fn argocd_pause(
        &self,
        step: &str,
        ctx: &str,
        ns: &str,
        application: &str,
    ) -> Result<(), String> {
        let app = self.op.json(
            &format!("{step}:app"),
            &Verb::GetNamed {
                context: ctx.into(),
                namespace: ns.into(),
                resource: "applications.argoproj.io".into(),
                names: vec![application.into()],
            },
        )?;
        let pause = match super::carve::owning_appset(&app) {
            Some(set) => {
                let appset = self.op.json(
                    &format!("{step}:appset"),
                    &Verb::GetNamed {
                        context: ctx.into(),
                        namespace: ns.into(),
                        resource: "applicationsets.argoproj.io".into(),
                        names: vec![set.clone()],
                    },
                )?;
                if !super::carve::appset_preserves_skip(&appset) {
                    return Err(format!(
                        "{step}: refused, nothing written — `{application}` is owned by ApplicationSet `{set}`, which regenerates the Application from its template (a removed `automated` comes back) and drops annotations it does not preserve; list `{}` in its spec.preservedFields.annotations to pause it",
                        super::carve::SKIP_RECONCILE
                    ));
                }
                PauseMode::SkipReconcile {
                    captured: app
                        .pointer("/metadata/annotations")
                        .and_then(|a| a.get(super::carve::SKIP_RECONCILE))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }
            }
            None => PauseMode::SyncPolicy {
                captured: app.pointer("/spec/syncPolicy").cloned(),
            },
        };
        self.paused.borrow_mut().insert(
            step.to_string(),
            ArgoPaused {
                context: ctx.into(),
                namespace: ns.into(),
                application: application.into(),
                pause: pause.clone(),
            },
        );
        self.op
            .evidence()
            .note(&serde_json::json!({"at": now(), "step": step, "argocd-pause": application, "snapshot": pause}));
        match pause {
            PauseMode::SkipReconcile { .. } => self.act(
                step,
                &Verb::Annotate {
                    context: ctx.into(),
                    namespace: ns.into(),
                    resource: "applications.argoproj.io".into(),
                    name: application.into(),
                    key: super::carve::SKIP_RECONCILE.into(),
                    value: Some("true".into()),
                },
            ),
            PauseMode::SyncPolicy { .. } => {
                let patch = super::carve::pause_patch(&app);
                if patch.as_object().is_some_and(serde_json::Map::is_empty) {
                    return Ok(());
                }
                self.act(
                    step,
                    &Verb::MergePatch {
                        context: ctx.into(),
                        namespace: ns.into(),
                        resource: "applications.argoproj.io".into(),
                        name: application.into(),
                        patch: patch.to_string(),
                        subresource: None,
                    },
                )
            }
        }
    }

    fn argocd_unpause(&self, step: &str, place_step: &str) -> Result<(), String> {
        let Some(p) = self.paused.borrow().get(place_step).cloned() else {
            return Ok(());
        };
        let res = match &p.pause {
            PauseMode::SkipReconcile { captured } => self.act(
                step,
                &Verb::Annotate {
                    context: p.context.clone(),
                    namespace: p.namespace.clone(),
                    resource: "applications.argoproj.io".into(),
                    name: p.application.clone(),
                    key: super::carve::SKIP_RECONCILE.into(),
                    value: captured.clone(),
                },
            ),
            PauseMode::SyncPolicy { captured } => {
                let live = self.op.json(
                    &format!("{step}:app"),
                    &Verb::GetNamed {
                        context: p.context.clone(),
                        namespace: p.namespace.clone(),
                        resource: "applications.argoproj.io".into(),
                        names: vec![p.application.clone()],
                    },
                )?;
                let patch = super::carve::unpause_patch(&live, captured.as_ref());
                if patch.as_object().is_some_and(serde_json::Map::is_empty) {
                    Ok(())
                } else {
                    self.act(
                        step,
                        &Verb::MergePatch {
                            context: p.context.clone(),
                            namespace: p.namespace.clone(),
                            resource: "applications.argoproj.io".into(),
                            name: p.application.clone(),
                            patch: patch.to_string(),
                            subresource: None,
                        },
                    )
                }
            }
        };
        if res.is_ok() {
            self.paused.borrow_mut().remove(place_step);
        }
        res
    }

    fn uncarve_node(&self, step: &str, place_step: &str) -> Result<(), String> {
        let Some(c) = self.nodes.borrow().get(place_step).cloned() else {
            return Ok(());
        };
        let o = ObjectRef {
            api_version: "v1".into(),
            kind: "Node".into(),
            name: c.node.clone(),
            uid: c.uid.clone(),
        };
        let Some(live) = self.read_object(&format!("{step}:read"), &c.context, "default", &o)?
        else {
            self.nodes.borrow_mut().remove(place_step);
            return Ok(());
        };
        if !o.is(&live) {
            return Err(format!(
                "{step}: node `{}` is no longer the node this run carved (uid changed); left alone",
                c.node
            ));
        }
        let has_taint = live
            .pointer("/spec/taints")
            .and_then(Value::as_array)
            .is_some_and(|t| {
                t.iter()
                    .any(|x| x.get("key").and_then(Value::as_str) == Some(c.taint_key.as_str()))
            });
        let has_label = live
            .pointer("/metadata/labels")
            .and_then(|l| l.get(&c.label_key))
            .is_some();
        if has_taint || has_label {
            self.act(
                step,
                &Verb::MergePatch {
                    context: c.context.clone(),
                    namespace: "default".into(),
                    resource: "node".into(),
                    name: c.node.clone(),
                    patch: super::carve::uncarve_node_patch(&live, &c.taint_key, &c.label_key)
                        .to_string(),
                    subresource: None,
                },
            )?;
        }
        self.nodes.borrow_mut().remove(place_step);
        Ok(())
    }

    /// The Ready, not-terminating pods behind `selector`, by name: the first,
    /// or every one with `all`.
    fn ready_pods(
        &self,
        step: &str,
        ctx: &str,
        ns: &str,
        selector: &str,
        all: bool,
    ) -> Result<Vec<String>, String> {
        let list = self.op.json(
            &format!("{step}:pods"),
            &Verb::Get {
                context: ctx.into(),
                namespace: ns.into(),
                resources: "pods".into(),
                selector: Some(selector.into()),
            },
        )?;
        let mut ready: Vec<String> = list_items(&list)
            .iter()
            .filter(|p| p.pointer("/metadata/deletionTimestamp").is_none() && pod_ready(p))
            .filter_map(|p| {
                p.pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        ready.sort();
        if ready.is_empty() {
            return Err(format!("{step}: `{selector}` selects no Ready pod in {ns}"));
        }
        if !all {
            ready.truncate(1);
        }
        Ok(ready)
    }

    /// Run an exec's `:undo-argv` in every pod its argv was attempted in.
    fn exec_undo(&self, step: &str, place_step: &str, undo_argv: &[String]) -> Result<(), String> {
        let Some(ran) = self.exec_ran.borrow().get(place_step).cloned() else {
            return Ok(());
        };
        let errs: Vec<String> = ran
            .pods
            .iter()
            .filter_map(|p| {
                self.act(
                    step,
                    &Verb::Exec {
                        context: ran.context.clone(),
                        namespace: ran.namespace.clone(),
                        pod: p.clone(),
                        container: ran.container.clone(),
                        argv: undo_argv.to_vec(),
                    },
                )
                .err()
            })
            .collect();
        if errs.is_empty() {
            self.exec_ran.borrow_mut().remove(place_step);
            Ok(())
        } else {
            Err(errs.join("; "))
        }
    }

    fn read_object(
        &self,
        step: &str,
        ctx: &str,
        ns: &str,
        o: &ObjectRef,
    ) -> Result<Option<Value>, String> {
        let e = self.op.run(
            step,
            &Verb::GetNamed {
                context: ctx.into(),
                namespace: ns.into(),
                resource: o.resource(),
                names: vec![o.name.clone()],
            },
        );
        if e.ok() {
            serde_json::from_str(&e.stdout)
                .map(Some)
                .map_err(|err| format!("{step}: {o} read is not JSON: {err}"))
        } else if super::command::vanished(&e.stderr) {
            Ok(None)
        } else {
            Err(format!(
                "{step}: could not read {o}: exit {}: {}",
                e.status,
                e.stderr.trim()
            ))
        }
    }

    /// Create a manifest's objects, refusing before any write if one exists,
    /// and record every identity created — even when the create fails part
    /// way, so the undo still removes what did land.
    fn create_objects(
        &self,
        step: &str,
        ctx: &str,
        ns: &str,
        objects: &[Value],
        timeout: Duration,
    ) -> Result<(), String> {
        let refs: Vec<ObjectRef> = objects.iter().filter_map(ObjectRef::of).collect();
        let mut present = Vec::new();
        for o in &refs {
            if self
                .read_object(&format!("{step}:absent"), ctx, ns, o)?
                .is_some()
            {
                present.push(o.to_string());
            }
        }
        if !present.is_empty() {
            return Err(format!(
                "{step}: refused, nothing written — {} already exist(s) in {ns}; this fixture owns only what it creates",
                present.join(", ")
            ));
        }
        let list = serde_json::json!({"apiVersion": "v1", "kind": "List", "items": objects});
        let mut shown = list.clone();
        super::snapshot::redact_secrets(&mut shown);
        let slug: String = step
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let _ = self.op.evidence().write(
            &format!("manifest-{slug}.json"),
            serde_json::to_vec_pretty(&shown)
                .unwrap_or_default()
                .as_slice(),
        );
        let e = self.stage_create(step, ctx, ns, objects)?;
        let from_output: Vec<ObjectRef> = serde_json::from_str::<Value>(&e.stdout)
            .map(|v| list_items(&v).iter().filter_map(ObjectRef::of).collect())
            .unwrap_or_default();
        let created: Vec<ObjectRef> = if e.ok() && from_output.len() == refs.len() {
            from_output
        } else {
            let mut landed = Vec::new();
            for o in &refs {
                if let Ok(Some(live)) = self.read_object(&format!("{step}:landed"), ctx, ns, o) {
                    landed.extend(ObjectRef::of(&live));
                }
            }
            landed
        };
        self.op.evidence().note(&serde_json::json!({
            "at": now(), "step": step, "context": ctx, "namespace": ns, "created": created,
        }));
        self.created.borrow_mut().insert(
            step.to_string(),
            Created {
                context: ctx.into(),
                namespace: ns.into(),
                objects: created,
                timeout,
            },
        );
        if e.ok() {
            Ok(())
        } else {
            Err(format!("{step}: exit {}: {}", e.status, e.stderr.trim()))
        }
    }

    fn stage_create(
        &self,
        step: &str,
        ctx: &str,
        ns: &str,
        objects: &[Value],
    ) -> Result<super::command::Exec, String> {
        let list = serde_json::json!({"apiVersion": "v1", "kind": "List", "items": objects});
        let path = std::env::temp_dir().join(format!(
            "ensaio-visita-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::write(&path, list.to_string())
            .map_err(|e| format!("{step}: could not stage the manifest: {e}"))?;
        let e = self.op.run(
            step,
            &Verb::Create {
                context: ctx.into(),
                namespace: ns.into(),
                file: path.display().to_string(),
            },
        );
        let _ = std::fs::remove_file(&path);
        Ok(e)
    }

    fn await_available(
        &self,
        step: &str,
        ctx: &str,
        ns: &str,
        names: &[String],
        timeout: Duration,
    ) -> Result<(), String> {
        if names.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let list = self.op.json(
                &format!("{step}:available"),
                &Verb::GetNamed {
                    context: ctx.into(),
                    namespace: ns.into(),
                    resource: "deployment".into(),
                    names: names.to_vec(),
                },
            )?;
            let ready: Vec<String> = list_items(&list)
                .iter()
                .filter(|d| {
                    d.pointer("/status/conditions")
                        .and_then(Value::as_array)
                        .is_some_and(|cs| {
                            cs.iter().any(|c| {
                                c.get("type").and_then(Value::as_str) == Some("Available")
                                    && c.get("status").and_then(Value::as_str) == Some("True")
                            })
                        })
                })
                .filter_map(|d| {
                    d.pointer("/metadata/name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect();
            let waiting: Vec<&String> = names.iter().filter(|n| !ready.contains(n)).collect();
            if waiting.is_empty() {
                self.op
                    .evidence()
                    .note(&serde_json::json!({"at": now(), "step": step, "available": names}));
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{step}: {waiting:?} not Available within {}s — the manifest is not in effect",
                    timeout.as_secs()
                ));
            }
            self.pause(Duration::from_secs(2))
                .map_err(|why| format!("{step}: {why}"))?;
        }
    }

    /// Delete exactly what `place_step` created — each object only while it
    /// still carries the uid recorded at its creation — and wait, bounded,
    /// until every one is gone.
    fn delete_created(&self, step: &str, place_step: &str) -> Result<(), String> {
        let Some(c) = self.created.borrow().get(place_step).cloned() else {
            return Ok(());
        };
        let mut errs = Vec::new();
        for o in c.objects.iter().rev() {
            match self.read_object(&format!("{step}:read"), &c.context, &c.namespace, o) {
                Ok(None) => {}
                Ok(Some(live)) if !o.is(&live) => errs.push(format!(
                    "{o} in {} is no longer the object this run created (uid changed); left alone",
                    c.namespace
                )),
                Ok(Some(_)) => {
                    let e = self.op.run(
                        step,
                        &Verb::Delete {
                            context: c.context.clone(),
                            namespace: c.namespace.clone(),
                            resource: o.resource(),
                            name: o.name.clone(),
                        },
                    );
                    if !e.ok() && !super::command::vanished(&e.stderr) {
                        errs.push(format!(
                            "delete {o}: exit {}: {}",
                            e.status,
                            e.stderr.trim()
                        ));
                    }
                }
                Err(e) => errs.push(e),
            }
        }
        let deadline = Instant::now() + c.timeout;
        loop {
            let mut left = Vec::new();
            for o in &c.objects {
                match self.read_object(&format!("{step}:gone"), &c.context, &c.namespace, o) {
                    Ok(Some(live)) if o.is(&live) => left.push(o.to_string()),
                    Ok(_) => {}
                    Err(e) => left.push(e),
                }
            }
            if left.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                errs.push(format!(
                    "still present {}s after delete: {}",
                    c.timeout.as_secs(),
                    left.join(", ")
                ));
                break;
            }
            self.pause_unguarded(Duration::from_secs(2));
        }
        self.op.evidence().note(&serde_json::json!({
            "at": now(), "step": step, "deleted": c.objects, "errors": errs,
        }));
        if errs.is_empty() {
            self.created.borrow_mut().remove(place_step);
            Ok(())
        } else {
            Err(format!("{step}: {}", errs.join("; ")))
        }
    }

    /// The inverse of a snapshot-invertible action, derived from the captured
    /// natural state rather than chosen by the author.
    fn restore(&self, step: &str, a: &Action) -> Result<(), String> {
        let natural = self.natural.borrow();
        let natural = natural
            .as_ref()
            .ok_or_else(|| format!("{step}: no natural state captured to restore from"))?;
        let (ctx, ns, resource, name) = a
            .target()
            .ok_or_else(|| format!("{step}: action has no single target to restore"))?;
        let kind = kind_of(resource);
        let original = natural.get(ctx, ns, &kind, name).ok_or_else(|| {
            format!("{step}: {ctx}/{ns}/{kind}/{name} is not in the natural state")
        })?;
        match a {
            Action::Annotate { key, .. } => {
                let value = original
                    .pointer("/metadata/annotations")
                    .and_then(|m| m.get(key))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.act(
                    step,
                    &Verb::Annotate {
                        context: ctx.into(),
                        namespace: ns.into(),
                        resource: resource.into(),
                        name: name.into(),
                        key: key.clone(),
                        value,
                    },
                )
            }
            Action::Patch { patch, .. } => {
                let p: Value = serde_json::from_str(patch)
                    .map_err(|e| format!("{step}: the forward patch is not JSON: {e}"))?;
                let inverse = inverse_merge_patch(&p, original);
                self.act(
                    step,
                    &Verb::MergePatch {
                        context: ctx.into(),
                        namespace: ns.into(),
                        resource: resource.into(),
                        name: name.into(),
                        patch: inverse.to_string(),
                        subresource: None,
                    },
                )
            }
            Action::ResizePod {
                container, timeout, ..
            } => {
                let block =
                    breathe_resize::container_resources(original, container).ok_or_else(|| {
                        format!("{step}: container `{container}` is not in the captured pod")
                    })?;
                // Already at the captured resources (the resize never went
                // out, or was put back): nothing to write.
                let live = self.read_pods(ctx, ns, &[name.to_string()], &format!("{step}:read"))?;
                if live
                    .first()
                    .and_then(|(_, p)| breathe_resize::container_resources(p, container))
                    .is_some_and(|now| now == block)
                {
                    return Ok(());
                }
                self.resize_apply(
                    step,
                    &ResizeTarget {
                        context: ctx,
                        namespace: ns,
                        pod: name,
                        container,
                    },
                    &block,
                    timeout.as_deref(),
                    false,
                )
            }
            Action::DispatchWorkflow { .. }
            | Action::ArgoSync { .. }
            | Action::RolloutRestart { .. }
            | Action::RabbitmqCloseConnections { .. }
            | Action::ApplyManifest { .. }
            | Action::Exec { .. }
            | Action::CarveNamespace { .. }
            | Action::Fence { .. }
            | Action::CarveNode { .. }
            | Action::ArgocdPause { .. }
            | Action::DeleteRunnerPod { .. } => Err(format!(
                "{step}: this action is not invertible from a snapshot (refused at validation; reaching here is a rig defect)"
            )),
        }
    }

    /// Resize one container in place. Refuses before any write if its cpu
    /// `resizePolicy` would restart it (unless allowed), builds the
    /// QoS-preserving block, and waits for the kubelet to converge.
    fn resize_place(
        &self,
        step: &str,
        t: &ResizeTarget<'_>,
        cpu_limit: Option<&str>,
        cpu_request: Option<&str>,
        allow_restart: bool,
        timeout: Option<&str>,
    ) -> Result<(), String> {
        let pods = self.read_pods(
            t.context,
            t.namespace,
            &[t.pod.to_string()],
            &format!("{step}:read"),
        )?;
        let (_, pod) = pods
            .into_iter()
            .next()
            .ok_or_else(|| format!("{step}: pod `{}` not found", t.pod))?;
        // Refuse only what the pod DECLARES will restart. An unspecified
        // policy is the API default, NotRequired — relied on, and said so in
        // the evidence, never assumed silently.
        match breathe_resize::resize_restart_policy(&pod, Some(t.container), "cpu") {
            breathe_resize::RestartPolicy::RestartContainer if !allow_restart => {
                return Err(format!(
                    "{step}: container `{}` of `{}` declares cpu resizePolicy RestartContainer, so an in-place resize would RESTART it; refused (declare :allow-restart to accept that)",
                    t.container, t.pod
                ));
            }
            breathe_resize::RestartPolicy::Unspecified => {
                self.op.evidence().note(&serde_json::json!({
                    "at": now(), "step": step, "resize_policy": "unspecified",
                    "relied_on": "API default NotRequired (k8s.io/api core/v1 ContainerResizePolicy)",
                }));
            }
            breathe_resize::RestartPolicy::RestartContainer
            | breathe_resize::RestartPolicy::NotRequired => {}
        }
        let qos = pod
            .pointer("/status/qosClass")
            .and_then(Value::as_str)
            .unwrap_or("Burstable");
        let current = breathe_resize::container_resources(&pod, t.container)
            .ok_or_else(|| format!("{step}: no container `{}` in `{}`", t.container, t.pod))?;
        let current_request = current.pointer("/requests/cpu").and_then(Value::as_str);
        let current_limit = current
            .pointer("/limits/cpu")
            .and_then(Value::as_str)
            .and_then(|q| breathe_resize::parse("cpu", q));
        let limit = cpu_limit
            .map(|q| {
                breathe_resize::parse("cpu", q)
                    .ok_or(format!("{step}: `{q}` is not a cpu quantity"))
            })
            .transpose()?;
        let request = cpu_request
            .map(|q| {
                breathe_resize::parse("cpu", q)
                    .ok_or(format!("{step}: `{q}` is not a cpu quantity"))
            })
            .transpose()?;
        if let (Some(req), Some(lim)) = (request, limit.or(current_limit)) {
            if req > lim {
                return Err(format!(
                    "{step}: request {req}m above limit {lim}m is not a valid resize"
                ));
            }
        }
        let mut block = match limit {
            Some(l) => breathe_resize::limit_block(qos, "cpu", l, current_request),
            None => serde_json::json!({}),
        };
        if let Some(r) = request {
            block["requests"] = breathe_resize::request_block("cpu", r)["requests"].clone();
        }
        self.op.evidence().note(&serde_json::json!({
            "at": now(), "step": step, "resize": t.pod, "container": t.container, "qos": qos, "from": current, "to": block,
        }));
        self.resize_apply(step, t, &block, timeout, true)
    }

    /// Write a resize and wait for `status.containerStatuses` to report it.
    /// `guarded` waits stop when the guard trips; the restoring resize is
    /// unguarded — it must finish precisely then.
    fn resize_apply(
        &self,
        step: &str,
        t: &ResizeTarget<'_>,
        block: &Value,
        timeout: Option<&str>,
        guarded: bool,
    ) -> Result<(), String> {
        let body = breathe_resize::resize_body(t.container, block);
        self.act(
            step,
            &Verb::ResizePatch {
                context: t.context.into(),
                namespace: t.namespace.into(),
                pod: t.pod.into(),
                body: body.to_string(),
            },
        )?;
        let timeout = duration(timeout, Duration::from_secs(120))?;
        let deadline = Instant::now() + timeout;
        loop {
            let pods = self.read_pods(
                t.context,
                t.namespace,
                &[t.pod.to_string()],
                &format!("{step}:converge"),
            )?;
            if pods
                .first()
                .is_some_and(|(_, p)| breathe_resize::converged(p, t.container, block))
            {
                self.op
                    .evidence()
                    .note(&serde_json::json!({"at": now(), "step": step, "converged": block}));
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{step}: the kubelet did not report {} on `{}`/{} within {}s — the resize is not in effect",
                    block,
                    t.pod,
                    t.container,
                    timeout.as_secs()
                ));
            }
            if guarded {
                self.pause(Duration::from_secs(2))
                    .map_err(|why| format!("{step}: {why}"))?;
            } else {
                self.pause_unguarded(Duration::from_secs(2));
            }
        }
    }

    fn assert_exec_metric(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        let window = duration(a.operator.window.as_deref(), Duration::from_secs(60))?;
        let from_fixture = self.record_started(&a.id).is_some();
        self.start_recorder(a);
        // Recorded from a fixture: judge what the record holds now (one more
        // sample taken). Otherwise: record for `window` from here.
        if from_fixture {
            self.tick();
        } else {
            let r = self.pause(window);
            if let Err(why) = r {
                self.stop_recorder(&a.id);
                return Err(why);
            }
        }
        self.stop_recorder(&a.id);
        let samples = self
            .exec_recorded
            .borrow()
            .get(&a.id)
            .cloned()
            .unwrap_or_default();
        Ok(judge::exec_metric(
            &samples,
            a.operator.stat.unwrap_or(crate::types::Stat::All),
            a.operator.min,
            a.operator.max,
        ))
    }

    fn assert_rollout(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        let ctx = a.operator.context.clone().unwrap_or_default();
        let ns = a
            .target_namespace
            .clone()
            .unwrap_or_else(|| "default".into());
        let names = a.operator.names.clone().unwrap_or_default();
        let window = duration(a.operator.window.as_deref(), Duration::from_secs(1200))?;
        let interval = duration(a.operator.interval.as_deref(), Duration::from_secs(2))?;
        let expect = a.operator.expect_image.clone();
        let read = |label: &str| -> Result<Vec<Value>, String> {
            let list = self.op.json(
                label,
                &Verb::Get {
                    context: ctx.clone(),
                    namespace: ns.clone(),
                    resources: "deployments".into(),
                    selector: None,
                },
            )?;
            Ok(list
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|d| {
                    d.pointer("/metadata/name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| names.iter().any(|x| x == n))
                })
                .collect())
        };
        // The baseline is the generation in the NATURAL state, so a rollout
        // that began before this step started is still judged. A name the
        // natural state does not cover falls back to its generation now.
        let now_gens: BTreeMap<String, i64> = read(&format!("{}:baseline", a.id))?
            .iter()
            .map(|d| RolloutSample::from_deployment(d, ""))
            .map(|s| (s.name, s.generation))
            .collect();
        let baseline: BTreeMap<String, i64> = {
            let natural = self.natural.borrow();
            names
                .iter()
                .filter_map(|n| {
                    natural
                        .as_ref()
                        .and_then(|nat| nat.generation(&ctx, &ns, "Deployment", n))
                        .or_else(|| now_gens.get(n).copied())
                        .map(|g| (n.clone(), g))
                })
                .collect()
        };
        // The record: begun at `:record-from`'s placement when declared, else
        // now. Its window runs from when it began, and it keeps sampling (via
        // `pause`) until a complete generation on the expected image is in.
        self.start_recorder(a);
        let began = self
            .recorders
            .borrow()
            .iter()
            .find(|r| r.assertion == a.id)
            .map_or_else(Instant::now, |r| r.started);
        let deadline = began + window;
        loop {
            let samples = self
                .recorded
                .borrow()
                .get(&a.id)
                .cloned()
                .unwrap_or_default();
            // Settled only by a COMPLETE generation on the expected image: a
            // restart of the old build while the pipeline is still building
            // must not end the watch.
            if judge::rollout_settled(&samples, &names, &baseline, expect.as_deref())
                || Instant::now() >= deadline
            {
                break;
            }
            self.pause(interval)?;
        }
        for r in self
            .recorders
            .borrow_mut()
            .iter_mut()
            .filter(|r| r.assertion == a.id)
        {
            r.active = false;
        }
        let samples = self
            .recorded
            .borrow()
            .get(&a.id)
            .cloned()
            .unwrap_or_default();
        let (surge, unavailable) = (
            a.operator.max_surge.unwrap_or(1),
            a.operator.max_unavailable.unwrap_or(0),
        );
        let events =
            judge::rollout_events(&samples, &baseline, surge, unavailable, expect.as_deref());
        let _ = self.op.evidence().write(
            &format!("rollout-{}.events.json", a.id),
            serde_json::to_vec_pretty(&events)
                .unwrap_or_default()
                .as_slice(),
        );
        Ok(judge::rollout(
            &samples,
            &baseline,
            surge,
            unavailable,
            expect.as_deref(),
        ))
    }

    /// Classify one pod read: text, vanished mid-read, or failed.
    fn pod_read(e: &super::command::Exec) -> judge::PodRead {
        if e.ok() {
            judge::PodRead::Text(e.stdout.clone())
        } else if super::command::vanished(&e.stderr) {
            judge::PodRead::Vanished
        } else {
            judge::PodRead::Failed(e.stderr.trim().to_string())
        }
    }

    fn assert_logs(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        let ctx = a.operator.context.clone().unwrap_or_default();
        let ns = a
            .target_namespace
            .clone()
            .unwrap_or_else(|| "default".into());
        let since = a
            .operator
            .window
            .clone()
            .map_or_else(|| Since::Time(self.started_at.clone()), Since::Window);
        let selector = a
            .operator
            .selector
            .clone()
            .ok_or("LogWindow needs :selector")?;
        let (pods, note) = self.select_pods(
            &format!("{}:pods", a.id),
            &ctx,
            &ns,
            &selector,
            a.operator.expect_image.as_deref(),
        )?;
        self.op
            .evidence()
            .note(&serde_json::json!({"at": now(), "step": a.id, "pods": note}));
        let reads: Vec<(String, judge::PodRead)> = pods
            .into_iter()
            .map(|pod| {
                let e = self.op.run(
                    &format!("{}:{pod}", a.id),
                    &Verb::Logs {
                        context: ctx.clone(),
                        namespace: ns.clone(),
                        pod: pod.clone(),
                        since: since.clone(),
                    },
                );
                (pod, Self::pod_read(&e))
            })
            .collect();
        Ok(judge::logs(
            &reads,
            a.operator.patterns.as_deref().unwrap_or_default(),
            a.operator.presence.unwrap_or(Presence::Absent),
            a.operator.min_count.unwrap_or(1),
            a.operator.match_any.unwrap_or(false),
        ))
    }

    fn assert_metric(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        let ctx = a.operator.context.clone().unwrap_or_default();
        let ns = a
            .target_namespace
            .clone()
            .unwrap_or_else(|| "default".into());
        let port = a
            .operator
            .port
            .ok_or("MetricInRange needs :port (the service's conf port, not an annotation)")?;
        let path = a.operator.path.clone().unwrap_or_else(|| "/metrics".into());
        let metric = a
            .operator
            .metric
            .clone()
            .ok_or("MetricInRange needs :metric")?;
        let selector = a
            .operator
            .selector
            .clone()
            .ok_or("MetricInRange needs :selector")?;
        let (pods, note) = self.select_pods(
            &format!("{}:pods", a.id),
            &ctx,
            &ns,
            &selector,
            a.operator.expect_image.as_deref(),
        )?;
        self.op
            .evidence()
            .note(&serde_json::json!({"at": now(), "step": a.id, "pods": note}));
        let reads: Vec<(String, judge::PodRead)> = pods
            .into_iter()
            .map(|pod| {
                let e = self.op.run(
                    &format!("{}:{pod}", a.id),
                    &Verb::GetRaw {
                        context: ctx.clone(),
                        path: format!("/api/v1/namespaces/{ns}/pods/{pod}:{port}/proxy{path}"),
                    },
                );
                (pod, Self::pod_read(&e))
            })
            .collect();
        Ok(judge::metrics(
            &reads,
            &metric,
            a.operator.min,
            a.operator.max,
        ))
    }

    fn assert_object_fields(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        let o = &a.operator;
        let ctx = o.context.clone().unwrap_or_default();
        let ns = a
            .target_namespace
            .clone()
            .unwrap_or_else(|| "default".into());
        let target = ObjectRef {
            api_version: o
                .api_version
                .clone()
                .ok_or("ObjectFields needs :api-version")?,
            kind: o
                .resource_kind
                .clone()
                .ok_or("ObjectFields needs :resource-kind")?,
            name: o.name.clone().ok_or("ObjectFields needs :name")?,
            uid: None,
        };
        let subject = target.to_string();
        let wait = o.wait_until.clone().unwrap_or_default();
        let expect = o.expect.clone().unwrap_or_default();
        let every: Vec<&crate::field::FieldPredicate> = wait.iter().chain(&expect).collect();
        let timeout_text = o.timeout.clone().unwrap_or_else(|| "5m".into());
        let timeout = duration(Some(&timeout_text), Duration::from_secs(300))?;
        let interval = duration(o.interval.as_deref(), Duration::from_secs(10))?;
        let deadline = Instant::now() + timeout;
        let samples = format!("object-{}.jsonl", a.id);
        let mut last: Option<judge::Seen> = None;
        let mut reads = 0usize;
        let watch = loop {
            reads += 1;
            let read = judge::ObjectRead::classify(self.read_object(
                &format!("{}:read", a.id),
                &ctx,
                &ns,
                &target,
            ));
            let line = match &read {
                judge::ObjectRead::Seen(seen) => {
                    serde_json::json!({"at": now(), "seen": seen_tag(seen), "fields": judge::selected_fields(seen, &every)})
                }
                judge::ObjectRead::Blind { reason, .. } => {
                    serde_json::json!({"at": now(), "seen": "blind", "reason": reason})
                }
            };
            let _ = self
                .op
                .evidence()
                .append(&samples, format!("{line}\n").as_bytes());
            let blind = match read {
                judge::ObjectRead::Seen(seen) => {
                    if judge::holds_all(&seen, &wait) {
                        break judge::ObjectWatch::Settled(seen);
                    }
                    last = Some(seen);
                    None
                }
                judge::ObjectRead::Blind {
                    reason,
                    lasting: true,
                } => {
                    break judge::ObjectWatch::Blind { reason, last };
                }
                judge::ObjectRead::Blind {
                    reason,
                    lasting: false,
                } => Some(reason),
            };
            let now_i = Instant::now();
            if now_i >= deadline {
                break match (blind, last) {
                    (None, Some(seen)) => judge::ObjectWatch::TimedOut {
                        after: timeout_text.clone(),
                        last: seen,
                    },
                    (Some(reason), last) => judge::ObjectWatch::Blind { reason, last },
                    (None, None) => judge::ObjectWatch::Blind {
                        reason: "no read answered".into(),
                        last: None,
                    },
                };
            }
            self.pause(interval.min(deadline - now_i))?;
        };
        let final_fields = match &watch {
            judge::ObjectWatch::Settled(seen) | judge::ObjectWatch::TimedOut { last: seen, .. } => {
                Some(judge::selected_fields(seen, &every))
            }
            judge::ObjectWatch::Blind { last, .. } => {
                last.as_ref().map(|s| judge::selected_fields(s, &every))
            }
        };
        let _ = self.op.evidence().write(
            &format!("object-{}.json", a.id),
            serde_json::to_vec_pretty(&serde_json::json!({
                "object": subject,
                "context": ctx,
                "namespace": a.target_namespace,
                "reads": reads,
                "watch": watch,
                "fields": final_fields,
            }))
            .unwrap_or_default()
            .as_slice(),
        );
        Ok(judge::object_fields(&subject, &watch, &wait, &expect))
    }

    fn assert_hpa(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        let list = self.op.json(
            &a.id,
            &Verb::Get {
                context: a.operator.context.clone().unwrap_or_default(),
                namespace: a
                    .target_namespace
                    .clone()
                    .unwrap_or_else(|| "default".into()),
                resources: "horizontalpodautoscalers".into(),
                selector: None,
            },
        )?;
        let items = list
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let policy = a
            .operator
            .scale_up
            .clone()
            .ok_or("HpaBehavior needs :scale-up")?;
        Ok(judge::hpa(
            &items,
            a.operator.names.as_deref().unwrap_or_default(),
            &policy,
        ))
    }

    fn assert_workflow_jobs(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        let o = &a.operator;
        let run_of = o.run_of.clone().ok_or("WorkflowJobs needs :run-of")?;
        let expect = o.jobs.clone().unwrap_or_default();
        let d = match self.dispatched_of(&a.id, &run_of) {
            Ok(d) => d,
            Err(e) => {
                return Ok(Verdict::Skipped {
                    reason: format!("unobserved: {e}"),
                });
            }
        };
        let subject = format!("run {} of {}", d.run_id, d.repo);
        let timeout_text = o.timeout.clone().unwrap_or_else(|| "60m".into());
        let timeout = duration(Some(&timeout_text), Duration::from_secs(3600))?;
        let interval = duration(o.interval.as_deref(), Duration::from_secs(30))?;
        let deadline = Instant::now() + timeout;
        let samples = format!("jobs-{}.jsonl", a.id);
        let lasting = |e: &str| {
            !transient(e)
                && ["HTTP 401", "HTTP 403", "HTTP 404", "must be logged in"]
                    .iter()
                    .any(|m| e.contains(m))
        };
        let mut last: Option<Vec<judge::RunJob>> = None;
        let watch = loop {
            let read = self.run_jobs_read(&format!("{}:read", a.id), &d);
            let line = match &read {
                Ok(jobs) => serde_json::json!({
                    "at": now(),
                    "jobs": expect.iter().map(|e| judge::job(jobs, &e.job)).collect::<Vec<_>>(),
                }),
                Err(e) => serde_json::json!({"at": now(), "blind": e}),
            };
            let _ = self
                .op
                .evidence()
                .append(&samples, format!("{line}\n").as_bytes());
            match read {
                Ok(jobs) if judge::expected_completed(&jobs, &expect) => {
                    break judge::JobsWatch::Settled { jobs };
                }
                Ok(jobs) => last = Some(jobs),
                Err(e) if lasting(&e) => break judge::JobsWatch::Blind { reason: e },
                Err(_) => {}
            }
            let now_i = Instant::now();
            if now_i >= deadline {
                break match last.take() {
                    Some(jobs) => judge::JobsWatch::TimedOut {
                        after: timeout_text.clone(),
                        jobs,
                    },
                    None => judge::JobsWatch::Blind {
                        reason: "no read answered".into(),
                    },
                };
            }
            self.pause(interval.min(deadline - now_i))?;
        };
        let texts = match &watch {
            judge::JobsWatch::Settled { jobs } | judge::JobsWatch::TimedOut { jobs, .. } => {
                match self.job_texts(&a.id, &d, jobs, &expect) {
                    Ok(t) => t,
                    Err(e) => {
                        return Ok(Verdict::Skipped {
                            reason: format!("unobserved (blind): {e}"),
                        });
                    }
                }
            }
            judge::JobsWatch::Blind { .. } => judge::JobTexts::default(),
        };
        let _ = self.op.evidence().write(
            &format!("jobs-{}.json", a.id),
            serde_json::to_vec_pretty(
                &serde_json::json!({"run": subject, "expect": expect, "watch": watch}),
            )
            .unwrap_or_default()
            .as_slice(),
        );
        Ok(judge::workflow_jobs(&subject, &watch, &expect, &texts))
    }

    fn job_texts(
        &self,
        id: &str,
        d: &Dispatched,
        jobs: &[judge::RunJob],
        expect: &[crate::types::JobExpect],
    ) -> Result<judge::JobTexts, String> {
        let mut texts = judge::JobTexts::default();
        for e in expect {
            let Some(j) = judge::job(jobs, &e.job).filter(|j| j.status == "completed") else {
                continue;
            };
            if e.log_contains.as_ref().is_some_and(|v| !v.is_empty()) {
                let step = format!("{id}:log:{}", j.id);
                let exec = self.op.run(
                    &step,
                    &Verb::GhApi {
                        path: format!("repos/{}/actions/jobs/{}/logs", d.repo, j.id),
                    },
                );
                if !exec.ok() {
                    return Err(format!(
                        "the log of job `{}` could not be read: {}",
                        e.job,
                        exec.stderr.trim()
                    ));
                }
                texts.logs.insert(e.job.clone(), exec.stdout);
            }
            if e.annotation_contains
                .as_ref()
                .is_some_and(|v| !v.is_empty())
            {
                let answer = self.op.json(
                    &format!("{id}:annotations:{}", j.id),
                    &Verb::GhApi {
                        path: format!(
                            "repos/{}/check-runs/{}/annotations?per_page=100",
                            d.repo, j.id
                        ),
                    },
                )?;
                let notes = answer
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .map(|n| {
                                format!(
                                    "{} {}",
                                    n.get("title").and_then(Value::as_str).unwrap_or_default(),
                                    n.get("message").and_then(Value::as_str).unwrap_or_default()
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                texts.annotations.insert(e.job.clone(), notes);
            }
        }
        Ok(texts)
    }
}

/// A merge patch that puts every key `forward` touches back to its value in
/// `original` (`null` — delete — where the original had none).
#[must_use]
pub fn inverse_merge_patch(forward: &Value, original: &Value) -> Value {
    match forward {
        Value::Object(fm) => {
            let mut out = serde_json::Map::new();
            for (k, fv) in fm {
                let ov = original.get(k);
                let v = match (fv, ov) {
                    (Value::Object(_), Some(o @ Value::Object(_))) => inverse_merge_patch(fv, o),
                    (_, Some(o)) => o.clone(),
                    (_, None) => Value::Null,
                };
                out.insert(k.clone(), v);
            }
            Value::Object(out)
        }
        _ => original.clone(),
    }
}

/// kubectl resource name → the `kind` the snapshot records.
fn kind_of(resource: &str) -> String {
    match resource
        .split('.')
        .next()
        .unwrap_or(resource)
        .to_ascii_lowercase()
        .as_str()
    {
        "secret" | "secrets" => "Secret".into(),
        "configmap" | "configmaps" | "cm" => "ConfigMap".into(),
        "deployment" | "deployments" | "deploy" => "Deployment".into(),
        "hpa" | "horizontalpodautoscaler" | "horizontalpodautoscalers" => {
            "HorizontalPodAutoscaler".into()
        }
        "application" | "applications" | "app" => "Application".into(),
        "pod" | "pods" | "po" => "Pod".into(),
        other => other.to_string(),
    }
}

#[must_use]
pub fn transient(err: &str) -> bool {
    [
        "API rate limit exceeded",
        "secondary rate limit",
        "HTTP 502",
        "HTTP 503",
        "HTTP 504",
    ]
    .iter()
    .any(|m| err.contains(m))
}

fn seen_tag(seen: &judge::Seen) -> &'static str {
    match seen {
        judge::Seen::Object(_) => "object",
        judge::Seen::Absent(_) => "absent",
    }
}

fn duration(s: Option<&str>, default: Duration) -> Result<Duration, String> {
    s.map_or(Ok(default), |d| {
        humantime::parse_duration(d).map_err(|e| format!("`{d}`: {e}"))
    })
}

impl EnsaioEnvironment for OperatorEnvironment<'_> {
    fn provision_system(&self, sys: &SystemSpec) -> Result<String, String> {
        Err(format!("a visita provisions nothing (system `{}`)", sys.id))
    }

    fn wait_ready(&self, _endpoint: &str) -> Result<(), String> {
        Err("a visita provisions nothing to wait for".into())
    }

    fn wire(&self, w: &WiringSpec) -> Result<(), String> {
        Err(format!("a visita wires nothing ({} -> {})", w.from, w.to))
    }

    fn place_fixture(&self, f: &Fixture) -> Result<(), String> {
        let a = f
            .action
            .as_ref()
            .ok_or_else(|| format!("fixture `{}` declares no :action", f.name))?;
        self.place(&format!("place:{}", f.name), a)
    }

    fn clean_fixture(&self, f: &Fixture) -> Result<(), String> {
        match &f.cleanup {
            Cleanup::Inverse(inv) => {
                let a = inv
                    .action
                    .as_ref()
                    .ok_or_else(|| format!("inverse of `{}` declares no :action", f.name))?;
                self.place(&format!("undo:{}", f.name), a)
            }
            Cleanup::RestoreFromSnapshot => {
                let a = f
                    .action
                    .as_ref()
                    .ok_or_else(|| format!("fixture `{}` declares no :action", f.name))?;
                self.restore(&format!("undo:{}", f.name), a)
            }
            Cleanup::OwnInverse => {
                let (step, place) = (format!("undo:{}", f.name), format!("place:{}", f.name));
                match &f.action {
                    Some(Action::Exec {
                        undo_argv: Some(u), ..
                    }) => self.exec_undo(&step, &place, u),
                    Some(Action::CarveNode { .. }) => self.uncarve_node(&step, &place),
                    Some(Action::ArgocdPause { .. }) => self.argocd_unpause(&step, &place),
                    _ => self.delete_created(&step, &place),
                }
            }
            Cleanup::Stop => {
                let step = format!("place:{}", f.name);
                let runs: Vec<Dispatched> = self
                    .dispatched()
                    .into_iter()
                    .filter(|d| d.fixture == step)
                    .collect();
                runs.iter().try_for_each(|d| self.stop_run(d))
            }
            Cleanup::Transient { .. }
            | Cleanup::Retained { .. }
            | Cleanup::EnvironmentIsDestroyed => Ok(()),
        }
    }

    fn run_assertion(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        match a.kind {
            AssertionKind::RolloutSurgeBound => self.assert_rollout(a),
            AssertionKind::LogWindow => self.assert_logs(a),
            AssertionKind::MetricInRange => self.assert_metric(a),
            AssertionKind::HpaBehavior => self.assert_hpa(a),
            AssertionKind::ExecMetric => self.assert_exec_metric(a),
            AssertionKind::ObjectFields => self.assert_object_fields(a),
            AssertionKind::WorkflowJobs => self.assert_workflow_jobs(a),
            AssertionKind::NaturalStateRestored
            | AssertionKind::StepsCompleted
            | AssertionKind::GuardHeld => Err(format!(
                "{:?} is computed by the visita runner, not authored",
                a.kind
            )),
            AssertionKind::EphemeralStorageOnly
            | AssertionKind::EnvironmentHealth
            | AssertionKind::AuthserviceAdmission
            | AssertionKind::TlsTerminationPassthrough
            | AssertionKind::HeaderSurvives
            | AssertionKind::NetworkPath
            | AssertionKind::CustomJob
            | AssertionKind::AmqpRoundtrip
            | AssertionKind::PitrDataRoundtrip => Err(format!(
                "{:?} runs a probe inside the target; a visita creates nothing there",
                a.kind
            )),
        }
    }

    fn teardown_system(&self, endpoint: &str) -> Result<(), String> {
        Err(format!(
            "a visita provisioned nothing to tear down (`{endpoint}`)"
        ))
    }

    fn publish_phase_event(&self, _event: &PhaseEvent) -> Result<(), String> {
        // A visita publishes nothing onto the fleet's nervous system: the
        // evidence directory is its whole record.
        Ok(())
    }
}
