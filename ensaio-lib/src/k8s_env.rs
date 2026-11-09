//! A real, `kube-rs`-backed [`EnsaioEnvironment`]. Honest interim:
//! provisions a bare Deployment+Service per system (`system.template`
//! read as a literal container image — named-template rendering from
//! Helm charts is a further increment, not built here); dispatches
//! every assertion kind, including `CustomJob` (a real batch/v1 Job:
//! submit `a.image`/`a.command`, poll to completion, capture the
//! probe pod's logs as evidence, delete the Job — see
//! `K8sEnvironment::run_custom_job`); `wire()` applies a real, named
//! `NetworkPolicy` ingress-allow rule per edge — the actual gap
//! the target cluster's own default-deny posture exposed this session.
//!
//! **Teardown is scale-to-zero, never delete** — the operator
//! correction: a provisioned
//! system's "off" state is `replicas: 0` (cost-free, instantly
//! wakeable), not an absent, deleted resource. Wiring this to a real
//! breathe `ReplicaBand` floor (rather than a direct Deployment patch)
//! is the named next increment.

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvVar, EnvVarSource, ExecAction, ObjectFieldSelector, PersistentVolumeClaim, Pod,
    PodSpec, PodStatus, PodTemplateSpec, Probe, SecretKeySelector, Service, ServicePort,
    ServiceSpec,
};
use k8s_openapi::api::networking::v1::{
    NetworkPolicy, NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicySpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};
use std::collections::BTreeMap;
use std::time::Duration;

use crate::interp::EnsaioEnvironment;
use crate::nervous::PhaseEvent;
use crate::types::{
    AssertionKind, AssertionSpec, Observation, Observations, SystemSpec, Verdict, WiringSpec,
};

/// One pod's health as [`AssertionKind::EnvironmentHealth`] classifies it.
///
/// A **three**-arm sum, not a bool, because the two-valued model is exactly
/// what gets this wrong: a finished Job pod is neither "Ready" nor "broken",
/// and folding it into either one produces a confident wrong answer. Naming
/// `Completed` as its own arm is what forces every reader of a match to decide
/// which side it falls on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PodHealth {
    /// Running, with a `Ready` condition of `True`.
    Ready,
    /// `phase: Succeeded` — a pod that ran to completion and exited 0. The
    /// terminal state of every Job pod in the namespace (bootstrap,
    /// schema-apply, bucket seeding), and **healthy**.
    Completed,
    /// Not healthy, carrying the reason a reader needs — the container's own
    /// waiting/terminated reason when there is one (`CrashLoopBackOff`,
    /// `ImagePullBackOff`, `CreateContainerConfigError`, a non-zero exit),
    /// falling back to the phase + the Ready condition.
    Unhealthy { reason: String },
}

pub struct K8sEnvironment {
    client: Client,
    namespace: String,
    http: reqwest::Client,
    /// `ServiceAccount` a `CustomJob` probe pod runs as. `None` (the
    /// default) leaves the probe on the namespace's own `default` SA —
    /// no cluster read access beyond what that SA already carries. Set
    /// via [`Self::with_probe_service_account`] to reuse the
    /// controller's own identity (broad-read RBAC, see
    /// `charts/ensaio/templates/rbac.yaml`) for probes that need to
    /// inspect cluster state (nodes, `BreatheCloudPools`, …).
    probe_service_account: Option<String>,
}

impl K8sEnvironment {
    /// How often `run_custom_job` re-checks the probe Job's status.
    const CUSTOM_JOB_POLL_INTERVAL: Duration = Duration::from_secs(2);
    /// Upper bound on a `CustomJob` probe's total run time — mirrored
    /// into the Job's own `activeDeadlineSeconds` so a hung probe is
    /// killed server-side even if this poll loop stops running.
    const CUSTOM_JOB_TIMEOUT: Duration = Duration::from_secs(120);

    #[must_use]
    pub fn new(client: Client, namespace: impl Into<String>) -> Self {
        Self {
            client,
            namespace: namespace.into(),
            http: reqwest::Client::new(),
            probe_service_account: None,
        }
    }

    #[must_use]
    pub fn with_probe_service_account(mut self, name: impl Into<String>) -> Self {
        self.probe_service_account = Some(name.into());
        self
    }

    fn labels(id: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("app".to_string(), id.to_string()),
            ("ensaio.pleme.io/system".to_string(), id.to_string()),
        ])
    }

    /// Sanitize an assertion id into a valid `metadata.generateName`
    /// prefix (lowercase alphanumeric + `-`, bounded length — K8s names
    /// are capped at 63 chars and `generateName` appends a 5-char
    /// random suffix, so 40 chars of prefix leaves ample room).
    fn job_name_prefix(assertion_id: &str) -> String {
        let sanitized: String = assertion_id
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .take(40)
            .collect();
        format!("ensaio-probe-{sanitized}-")
    }

    /// Build the `CustomJob` probe pod's env list: the Downward-API
    /// `NODE_NAME` var (always, cheap placement evidence) followed by
    /// every author-declared [`AssertionSpec::env`] entry. Each entry is
    /// **either** a literal `value` **or** a `secret_key_ref` (a K8s
    /// `secretKeyRef`) — the sanctioned channel for a mgmt credential,
    /// so the secret never enters `command`/git/the image. A malformed
    /// entry (neither `value` nor `secret_key_ref`) is dropped rather
    /// than emitted as an empty var.
    fn probe_env(a: &AssertionSpec) -> Vec<EnvVar> {
        let mut env = vec![EnvVar {
            name: "NODE_NAME".to_string(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    field_path: "spec.nodeName".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];
        for e in a.env.iter().flatten() {
            let var = match (&e.value, &e.secret_key_ref) {
                (Some(v), _) => EnvVar {
                    name: e.name.clone(),
                    value: Some(v.clone()),
                    ..Default::default()
                },
                (None, Some(sk)) => EnvVar {
                    name: e.name.clone(),
                    value_from: Some(EnvVarSource {
                        secret_key_ref: Some(SecretKeySelector {
                            name: sk.name.clone(),
                            key: sk.key.clone(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                (None, None) => continue,
            };
            env.push(var);
        }
        env
    }

    /// Build the probe Job — the **pure, client-free** renderer shared by the
    /// single-container [`AssertionKind::CustomJob`] and the two-container
    /// [`AssertionKind::AmqpRoundtrip`] kinds. When [`AssertionSpec::sidecar`]
    /// is set it renders a KEP-753 native sidecar (an `initContainer` with
    /// `restartPolicy: Always` + an `exec` `startupProbe`) that kubelet starts
    /// first, keeps running for the Pod's whole lifetime, and gates the main
    /// container on — the muda **absorbed-test** two-container shape a
    /// single-container Job cannot express
    /// (an absorbed-test Job manifest). Pure + synchronous so
    /// the rendered shape is unit-testable without a live cluster (see
    /// `tests` in this file).
    fn build_probe_job(
        a: &AssertionSpec,
        image: &str,
        probe_ns: &str,
        service_account: Option<&str>,
    ) -> Job {
        // The relay sidecar, if declared — a native sidecar (initContainer,
        // restartPolicy: Always) NOT a peer container, so it is ready before
        // the main container's process dials the pod-local loopback.
        let init_containers = a.sidecar.as_ref().map(|s| {
            vec![Container {
                name: s.name.clone().unwrap_or_else(|| "relay".to_string()),
                image: Some(s.image.clone()),
                command: s.command.clone(),
                args: s.args.clone(),
                restart_policy: Some("Always".to_string()),
                startup_probe: s.startup_probe_command.as_ref().map(|cmd| Probe {
                    exec: Some(ExecAction {
                        command: Some(cmd.clone()),
                    }),
                    period_seconds: Some(1),
                    failure_threshold: Some(30),
                    ..Default::default()
                }),
                ..Default::default()
            }]
        });

        Job {
            metadata: ObjectMeta {
                generate_name: Some(Self::job_name_prefix(&a.id)),
                namespace: Some(probe_ns.to_string()),
                labels: Some(Self::labels(&a.id)),
                ..Default::default()
            },
            spec: Some(JobSpec {
                backoff_limit: Some(0),
                active_deadline_seconds: Some(
                    i64::try_from(Self::CUSTOM_JOB_TIMEOUT.as_secs()).unwrap_or(i64::MAX),
                ),
                ttl_seconds_after_finished: Some(300),
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(Self::labels(&a.id)),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        restart_policy: Some("Never".to_string()),
                        service_account_name: service_account.map(str::to_string),
                        init_containers,
                        containers: vec![Container {
                            name: "probe".to_string(),
                            image: Some(image.to_string()),
                            command: a.command.clone(),
                            args: a.args.clone(),
                            // The Downward-API `NODE_NAME` (cheap placement
                            // evidence, no RBAC cost) followed by every
                            // author-declared env var — the sanctioned
                            // channel for a mgmt credential via
                            // `secretKeyRef` (never inlined in `command`).
                            env: Some(Self::probe_env(a)),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Run one probe-Job assertion ([`AssertionKind::CustomJob`] or
    /// [`AssertionKind::AmqpRoundtrip`]): submit the Job built by
    /// [`Self::build_probe_job`], poll it to completion, capture the probe
    /// pod's logs as evidence, then delete the Job.
    ///
    /// This is deliberately NOT the scale-to-zero teardown rule
    /// (`teardown_system` above) — that rule
    /// governs a `SystemSpec`-declared system's lifecycle across a
    /// whole Ensaio run (it has an ongoing identity worth preserving
    /// dormant). A probe Job is a one-shot process with no identity
    /// beyond its exit code; once that's captured, deleting the Job
    /// (and, via GC, its pod) is the correct terminal state.
    #[allow(clippy::too_many_lines)]
    async fn run_probe_job(&self, a: &AssertionSpec, image: &str) -> Result<Verdict, String> {
        // The probe Job lands in `a.probe_namespace` when set — so it can
        // reach that env's `<svc>.<ns>.svc.cluster.local` names AND sits
        // inside that namespace's own network policy (same-namespace
        // traffic is allowed under a default-deny posture). `None` keeps
        // the rig's own namespace.
        let probe_ns = a.probe_namespace.as_deref().unwrap_or(&self.namespace);
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), probe_ns);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), probe_ns);

        let job = Self::build_probe_job(a, image, probe_ns, self.probe_service_account.as_deref());

        let created = jobs
            .create(&PostParams::default(), &job)
            .await
            .map_err(|e| format!("CustomJob {}: create job: {e}", a.id))?;
        let job_name = created
            .metadata
            .name
            .ok_or_else(|| format!("CustomJob {}: created job has no name", a.id))?;

        let deadline = tokio::time::Instant::now() + Self::CUSTOM_JOB_TIMEOUT;
        let succeeded = loop {
            let current = jobs
                .get(&job_name)
                .await
                .map_err(|e| format!("CustomJob {}: poll job {job_name}: {e}", a.id))?;
            if let Some(status) = &current.status {
                if status.succeeded.unwrap_or(0) > 0 {
                    break true;
                }
                if status.failed.unwrap_or(0) > 0 {
                    break false;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = jobs.delete(&job_name, &DeleteParams::default()).await;
                return Ok(Verdict::Errored {
                    message: format!(
                        "CustomJob {}: job {job_name} did not complete within {:?}",
                        a.id,
                        Self::CUSTOM_JOB_TIMEOUT
                    ),
                });
            }
            tokio::time::sleep(Self::CUSTOM_JOB_POLL_INTERVAL).await;
        };

        let probe_pod_name = pods
            .list(&ListParams::default().labels(&format!("job-name={job_name}")))
            .await
            .ok()
            .and_then(|list| list.items.into_iter().next())
            .and_then(|pod| pod.metadata.name);
        let evidence_logs = match &probe_pod_name {
            Some(pod_name) => pods
                .logs(pod_name, &LogParams::default())
                .await
                .unwrap_or_else(|e| format!("<log fetch failed: {e}>")),
            None => "<no probe pod found>".to_string(),
        };

        let _ = jobs.delete(&job_name, &DeleteParams::default()).await;

        // The verdict is DERIVED from the subject set the probe actually
        // examined, and carries it.
        // The subjects are the job's own terminal status plus every line
        // the probe pod logged — one observation each, so a reader can see
        // both *what* was concluded and *how much* it was concluded from.
        // Previously this was one opaque `evidence` string, and a hold over
        // forty log lines was the same value as a hold over none.
        let outcome_line = if succeeded {
            format!("job {job_name} ({image}) succeeded")
        } else {
            format!("job {job_name} ({image}) failed")
        };
        let mut observations = vec![Observation::new(outcome_line)];
        observations.extend(
            evidence_logs
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(Observation::new),
        );
        Ok(Verdict::from_probe(observations, succeeded))
    }

    // ── EnvironmentHealth: pure classification, cluster-free ───────────
    //
    // The two functions below take `&[Pod]` rather than a `Client`, so the
    // whole judgement — including every way it can be WRONG — is exercisable
    // by unit test against a hand-built pod. The interpreter arm is then a
    // three-line list-and-delegate. This is deliberate: an assertion whose
    // classifier can only be run against a live cluster is an assertion whose
    // classifier is never red-run.

    /// The first container-level problem in a pod's status, as a reason
    /// string — or `None` when no container is complaining.
    ///
    /// Scans **init containers first, then regular ones**, matching kubelet's
    /// own start order: when an init container (or a KEP-753 native sidecar,
    /// which is an init container with `restartPolicy: Always`) is failing,
    /// the main container has not started and reporting its `PodInitializing`
    /// would name the symptom instead of the cause.
    ///
    /// A container that terminated with exit 0 is skipped — that is a
    /// *completed* init container, the normal case, not a problem.
    fn first_container_problem(status: &PodStatus) -> Option<String> {
        let init = status.init_container_statuses.iter().flatten();
        let main = status.container_statuses.iter().flatten();
        for cs in init.chain(main) {
            let Some(state) = cs.state.as_ref() else {
                continue;
            };
            if let Some(waiting) = state.waiting.as_ref() {
                let reason = waiting
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Waiting".to_string());
                return Some(format!("container {} is waiting: {reason}", cs.name));
            }
            if let Some(term) = state.terminated.as_ref() {
                if term.exit_code != 0 {
                    let reason = term
                        .reason
                        .clone()
                        .unwrap_or_else(|| "Terminated".to_string());
                    return Some(format!(
                        "container {} terminated: {reason} (exit {})",
                        cs.name, term.exit_code
                    ));
                }
            }
        }
        None
    }

    /// Classify one pod. **Terminal phases are read before any condition.**
    ///
    /// The ordering is the whole correctness argument. kubelet sets every
    /// condition on a finished pod — `Ready` included — to `False`, so a
    /// classifier that consults `Ready` first marks every completed Job pod as
    /// broken. A spawned application environment runs several (bootstrap,
    /// schema-apply, bucket seeding), so that ordering bug would falsify a
    /// perfectly healthy environment on every run.
    fn classify_pod(pod: &Pod) -> PodHealth {
        let Some(status) = pod.status.as_ref() else {
            return PodHealth::Unhealthy {
                reason: "the apiserver reported no status at all".to_string(),
            };
        };
        let phase = status.phase.as_deref().unwrap_or("<unset>");

        match phase {
            // Ran to completion, exited 0. Healthy — see `PodHealth::Completed`.
            "Succeeded" => return PodHealth::Completed,
            // Ran to completion and did NOT exit 0. Unambiguously unhealthy,
            // and named separately from the Ready path so the exit code
            // survives into the verdict.
            "Failed" => {
                let detail = Self::first_container_problem(status)
                    .or_else(|| status.reason.clone())
                    .unwrap_or_else(|| "no container detail reported".to_string());
                return PodHealth::Unhealthy {
                    reason: format!("phase Failed — {detail}"),
                };
            }
            _ => {}
        }

        // `Ready` is the pod-level AND over every container's readiness, so a
        // crash-looping container cannot leave it `True`. That makes it a
        // sound gate; the container scan below exists to supply the *reason*,
        // never to change the verdict — which is why there is no hand-listed
        // set of "bad" waiting reasons here to go stale.
        let ready = status
            .conditions
            .iter()
            .flatten()
            .find(|c| c.type_ == "Ready")
            .is_some_and(|c| c.status == "True");
        if ready {
            return PodHealth::Ready;
        }

        PodHealth::Unhealthy {
            reason: Self::first_container_problem(status)
                .unwrap_or_else(|| format!("phase {phase}, Ready condition is not True")),
        }
    }

    /// How many times a pod's containers have restarted, init + regular.
    /// Carried as evidence on an otherwise-healthy pod: Ready-now with 47
    /// restarts is a real signal, and a verdict that hid it would be reporting
    /// less than it saw. It is deliberately **not** a finding — a restart is
    /// not by itself a falsification of "the environment is up".
    fn restart_count(status: Option<&PodStatus>) -> i32 {
        let Some(status) = status else { return 0 };
        status
            .init_container_statuses
            .iter()
            .flatten()
            .chain(status.container_statuses.iter().flatten())
            .map(|cs| cs.restart_count)
            .sum()
    }

    /// Judge a whole namespace. Pure: every input is in the arguments.
    ///
    /// The verdict's subject set is **one observation per pod plus a
    /// denominator line**, so a reader can see both what was concluded and how
    /// much it was concluded from. The denominator lives *inside* the verdict
    /// rather than beside it, which is what makes broken discovery fail loudly:
    /// if the pod list ever came back empty because a selector or an RBAC grant
    /// regressed, the witness says `0 pod(s) inspected` and the verdict is
    /// Falsified — it cannot quietly hold over nothing.
    fn judge_environment_health(pods: &[Pod], ns: &str) -> Verdict {
        let mut subjects: Vec<Observation> = Vec::new();
        let mut findings: Vec<Observation> = Vec::new();
        let (mut ready, mut completed) = (0usize, 0usize);

        for pod in pods {
            let name = pod.metadata.name.as_deref().unwrap_or("<unnamed>");
            let restarts = Self::restart_count(pod.status.as_ref());
            let restart_note = if restarts > 0 {
                format!(" [{restarts} restart(s)]")
            } else {
                String::new()
            };

            match Self::classify_pod(pod) {
                PodHealth::Ready => {
                    ready += 1;
                    subjects.push(Observation::new(format!("{name}: Ready{restart_note}")));
                }
                PodHealth::Completed => {
                    completed += 1;
                    subjects.push(Observation::new(format!(
                        "{name}: Completed — ran to completion and exited 0, healthy{restart_note}"
                    )));
                }
                PodHealth::Unhealthy { reason } => {
                    let line = format!("{name}: NOT healthy — {reason}{restart_note}");
                    subjects.push(Observation::new(line.clone()));
                    findings.push(Observation::new(line));
                }
            }
        }

        subjects.push(Observation::new(format!(
            "{} pod(s) inspected in {ns} — {ready} Ready, {completed} Completed, {} unhealthy",
            pods.len(),
            findings.len()
        )));

        // An empty namespace is a FALSIFICATION, never a pass and never
        // `Vacuous`. The namespace WAS examined; what was found is that an
        // environment expected to be running has nothing running in it. Calling
        // that `Vacuous` would route a failed spawn to `Inconclusive` — merely
        // unproven — when it is the loudest possible evidence against health.
        if pods.is_empty() {
            findings.push(Observation::new(format!(
                "no pods found in {ns} — nothing was actually checked, and an \
                 empty namespace is not a healthy environment"
            )));
        }

        Verdict::judge(Observations::scope(subjects), findings)
    }
}

impl EnsaioEnvironment for K8sEnvironment {
    fn provision_system(&self, sys: &SystemSpec) -> Result<String, String> {
        let deployments: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let labels = Self::labels(&sys.id);

        let dep = Deployment {
            metadata: ObjectMeta {
                name: Some(sys.id.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels.clone()),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        containers: vec![Container {
                            name: sys.id.clone(),
                            image: Some(sys.template.clone()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| format!("no tokio runtime available: {e}"))?;
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                deployments
                    .patch(
                        &sys.id,
                        &PatchParams::apply("ensaio").force(),
                        &Patch::Apply(&dep),
                    )
                    .await
                    .map_err(|e| format!("provision {}: {e}", sys.id))?;

                let svc = Service {
                    metadata: ObjectMeta {
                        name: Some(sys.id.clone()),
                        namespace: Some(self.namespace.clone()),
                        labels: Some(labels.clone()),
                        ..Default::default()
                    },
                    spec: Some(ServiceSpec {
                        selector: Some(labels),
                        ports: Some(vec![ServicePort {
                            port: 80,
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                services
                    .patch(
                        &sys.id,
                        &PatchParams::apply("ensaio").force(),
                        &Patch::Apply(&svc),
                    )
                    .await
                    .map_err(|e| format!("provision {} service: {e}", sys.id))?;
                Ok::<(), String>(())
            })
        })?;

        Ok(format!("{}.{}.svc.cluster.local", sys.id, self.namespace))
    }

    fn wait_ready(&self, endpoint: &str) -> Result<(), String> {
        let name = endpoint
            .split('.')
            .next()
            .ok_or_else(|| format!("bad endpoint {endpoint}"))?;
        let deployments: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| format!("no tokio runtime available: {e}"))?;
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                let dep = deployments
                    .get(name)
                    .await
                    .map_err(|e| format!("wait_ready {name}: {e}"))?;
                let ready = dep
                    .status
                    .as_ref()
                    .and_then(|s| s.available_replicas)
                    .unwrap_or(0)
                    > 0;
                if ready {
                    Ok(())
                } else {
                    Err(format!("{name} not yet ready"))
                }
            })
        })
    }

    fn wire(&self, w: &WiringSpec) -> Result<(), String> {
        // Same-namespace K8s DNS already resolves `to.<ns>.svc.cluster.local`
        // from `from`'s pod — the real wiring gap this session's default-deny
        // namespace posture (the target cluster's own isolation) exposed is the
        // NetworkPolicy allow rule, so that's what this applies: an
        // ingress-allow on `to`'s pods, scoped to traffic from `from`'s
        // pods only, named after the edge so a second wiring edge doesn't
        // clobber the first.
        let policies: Api<NetworkPolicy> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = format!("ensaio-wire-{}-{}", w.from, w.to);
        let policy = NetworkPolicy {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            },
            spec: Some(NetworkPolicySpec {
                pod_selector: LabelSelector {
                    match_labels: Some(Self::labels(&w.to)),
                    ..Default::default()
                },
                policy_types: Some(vec!["Ingress".to_string()]),
                ingress: Some(vec![NetworkPolicyIngressRule {
                    from: Some(vec![NetworkPolicyPeer {
                        pod_selector: Some(LabelSelector {
                            match_labels: Some(Self::labels(&w.from)),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ports: None,
                }]),
                egress: None,
            }),
        };

        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| format!("no tokio runtime available: {e}"))?;
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                policies
                    .patch(
                        &name,
                        &PatchParams::apply("ensaio").force(),
                        &Patch::Apply(&policy),
                    )
                    .await
                    .map_err(|e| format!("wire {}->{}: {e}", w.from, w.to))?;
                Ok(())
            })
        })
    }

    fn run_assertion(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| format!("no tokio runtime available: {e}"))?;
        match a.kind {
            AssertionKind::AuthserviceAdmission
            | AssertionKind::TlsTerminationPassthrough
            | AssertionKind::HeaderSurvives => {
                let url = format!("http://{}.{}.svc.cluster.local", a.target, self.namespace);
                let mut req = self.http.post(&url);
                if let Some(header) = &a.header_name {
                    req = req.header(header.as_str(), "ensaio-probe");
                }
                tokio::task::block_in_place(|| {
                    rt.block_on(async {
                        match req.send().await {
                            Ok(resp) => {
                                let ok = resp.status().is_success();
                                let seen = format!("{} -> {}", url, resp.status());
                                Ok(Verdict::from_probe(vec![Observation::new(seen)], ok))
                            }
                            Err(e) => Ok(Verdict::Errored {
                                message: e.to_string(),
                            }),
                        }
                    })
                })
            }
            AssertionKind::EnvironmentHealth => {
                // The environment under qualification, NOT the rig — the same
                // reason `EphemeralStorageOnly` reads `target_namespace`: a
                // confident answer about the wrong namespace is worse than an
                // error, because nothing about it looks wrong.
                let ns = a
                    .target_namespace
                    .clone()
                    .unwrap_or_else(|| self.namespace.clone());
                let pods: Api<Pod> = Api::namespaced(self.client.clone(), &ns);
                tokio::task::block_in_place(|| {
                    rt.block_on(async {
                        match pods.list(&ListParams::default()).await {
                            Ok(list) => Ok(Self::judge_environment_health(&list.items, &ns)),
                            // A denied or broken LIST is `Errored`, never an
                            // empty namespace: "I could not look" and "I looked
                            // and it was empty" are different claims and the
                            // one that must never be rounded into the other.
                            Err(e) => Ok(Verdict::Errored {
                                message: format!("list pods in {ns}: {e}"),
                            }),
                        }
                    })
                })
            }
            AssertionKind::EphemeralStorageOnly => {
                // The environment under qualification, NOT the rig. Defaulting
                // to the rig's namespace is what made this assertion answer a
                // question nobody asked (see AssertionSpec::target_namespace).
                let ns = a
                    .target_namespace
                    .clone()
                    .unwrap_or_else(|| self.namespace.clone());
                let pods: Api<Pod> = Api::namespaced(self.client.clone(), &ns);
                let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), &ns);
                tokio::task::block_in_place(|| {
                    rt.block_on(async {
                        let mut obs = Vec::new();
                        let mut ok = true;

                        // A PVC is the unambiguous failure: it outlives the pod
                        // by definition, which is what makes it not ephemeral.
                        match pvcs.list(&Default::default()).await {
                            Ok(list) => {
                                for c in &list.items {
                                    ok = false;
                                    obs.push(Observation::new(format!(
                                        "PersistentVolumeClaim {} exists — storage that outlives the pod",
                                        c.metadata.name.as_deref().unwrap_or("<unnamed>")
                                    )));
                                }
                                obs.push(Observation::new(format!(
                                    "{} PersistentVolumeClaim(s) in {ns}",
                                    list.items.len()
                                )));
                            }
                            Err(e) => {
                                return Ok(Verdict::Errored { message: format!("list PVCs in {ns}: {e}") });
                            }
                        }

                        // Every working volume must be a BOUNDED memory-backed
                        // emptyDir. Unbounded is not a pass: it trades ENOSPC
                        // for an unattributable OOMKill.
                        let mut inspected = 0usize;
                        match pods.list(&Default::default()).await {
                            Ok(list) => {
                                for pod in &list.items {
                                    let pname =
                                        pod.metadata.name.as_deref().unwrap_or("<unnamed>");
                                    let Some(vols) =
                                        pod.spec.as_ref().map(|s| &s.volumes) else { continue };
                                    for v in vols.iter().flatten() {
                                        // Projected config and the SA token are
                                        // not working storage.
                                        if v.config_map.is_some()
                                            || v.secret.is_some()
                                            || v.projected.is_some()
                                            || v.downward_api.is_some()
                                        {
                                            continue;
                                        }
                                        inspected += 1;
                                        if v.persistent_volume_claim.is_some() {
                                            ok = false;
                                            obs.push(Observation::new(format!(
                                                "{pname}/{} binds a PVC", v.name
                                            )));
                                            continue;
                                        }
                                        match v.empty_dir.as_ref() {
                                            Some(ed) if ed.medium.as_deref() == Some("Memory") => {
                                                if ed.size_limit.is_none() {
                                                    ok = false;
                                                    obs.push(Observation::new(format!(
                                                        "{pname}/{} is an UNBOUNDED ramdisk", v.name
                                                    )));
                                                }
                                            }
                                            Some(_) => {
                                                ok = false;
                                                obs.push(Observation::new(format!(
                                                    "{pname}/{} is a disk-backed emptyDir", v.name
                                                )));
                                            }
                                            None => {
                                                ok = false;
                                                obs.push(Observation::new(format!(
                                                    "{pname}/{} is neither an emptyDir nor a PVC — unclassified storage",
                                                    v.name
                                                )));
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                return Ok(Verdict::Errored { message: format!("list pods in {ns}: {e}") });
                            }
                        }

                        // The denominator, inside the verdict: an empty
                        // namespace must not read as a pass.
                        obs.push(Observation::new(format!(
                            "{inspected} working volume(s) inspected across {ns}"
                        )));
                        if inspected == 0 {
                            ok = false;
                            obs.push(Observation::new(
                                "no working volumes found — nothing was actually checked".to_string(),
                            ));
                        }
                        Ok(Verdict::from_probe(obs, ok))
                    })
                })
            }
            AssertionKind::NetworkPath => {
                let url = format!("http://{}.{}.svc.cluster.local", a.target, self.namespace);
                tokio::task::block_in_place(|| {
                    rt.block_on(async {
                        match self.http.get(&url).send().await {
                            Ok(_) => Ok(Verdict::from_probe(
                                vec![Observation::new(format!("{url} reachable"))],
                                true,
                            )),
                            Err(e) => Ok(Verdict::from_probe(
                                vec![Observation::new(e.to_string())],
                                false,
                            )),
                        }
                    })
                })
            }
            // Both probe-Job kinds share one runner ([`Self::run_probe_job`]);
            // `AmqpRoundtrip` differs only in declaring an [`AssertionSpec::sidecar`]
            // (the two-container absorbed-test shape), which the shared
            // [`Self::build_probe_job`] renders. This stays exhaustive over
            // `AssertionKind` (the CATALOG REFLECTION guarantee), so a new
            // variant still forces a compile error here.
            AssertionKind::PitrDataRoundtrip => {
                // ── THE ASSERTION THAT MAKES A PITR DRILL MEAN ANYTHING ──────
                //
                // Everything else a drill reports is also true of a restore that
                // produced an EMPTY database: the snapshot bound, the PVC bound,
                // the StatefulSet ran, the composite went Healthy. This is the
                // only step that looks at the bytes.
                //
                // ★ WHY THIS IS A TYPED KIND AND NOT `CustomJob`.
                //
                // `types.rs` states the rule — a new assertion is "a new
                // AssertionKind variant + interpreter arm". Reaching for
                // CustomJob would hand the operator a shell and call it typed:
                // the SQL below is the part that is easy to get subtly, silently
                // wrong (a `SELECT 1` that passes on any row; a match on
                // correlation id alone that passes on a truncated payload), and
                // it is exactly the part an author must not have to write. So the
                // operator declares WHAT to find — a `PitrMarker` — and the kind
                // owns HOW to look.
                //
                // Submission, polling, log capture and Job cleanup are NOT
                // reimplemented here: it composes `run_probe_job`, the same path
                // CustomJob and AmqpRoundtrip use. What differs is only the
                // command, and the command is ours.
                let m = a.marker.as_ref().ok_or_else(|| {
                    format!(
                        "PitrDataRoundtrip {}: assertion.marker is required — this kind verifies a \
                         SPECIFIC row seeded before the snapshot, and without one it could only \
                         check that a table exists, which a restore of an empty database also \
                         satisfies",
                        a.id
                    )
                })?;
                let image = a
                    .image
                    .clone()
                    .unwrap_or_else(|| DEFAULT_MYSQL_PROBE_IMAGE.to_string());

                // The restored MySQL, addressed in the namespace the drill
                // restored into. Carried explicitly for the same reason the seed
                // carries its host in full: a bare Service name resolves in
                // whatever namespace the reader sits in, and the failure mode is
                // not an error but a read from the WRONG database — here, one
                // that would report the SOURCE's live row as proof of a restore.
                let ns = a
                    .target_namespace
                    .clone()
                    .unwrap_or_else(|| self.namespace.clone());
                let host = format!("{}.{ns}.svc.cluster.local", a.target);

                let mut probe = a.clone();
                probe.image = Some(image.clone());
                // The password reaches the probe as MYSQL_PWD via this env var,
                // never on argv: a password in a command line is visible in `ps`,
                // in the rendered Pod spec, and in any log that echoes the
                // container's command. Appended to whatever env the author
                // declared rather than replacing it, so a probe that also needs
                // its own variables keeps them.
                let mut env = probe.env.take().unwrap_or_default();
                env.push(crate::types::EnvVarSpec {
                    name: "MARKER_PASSWORD".into(),
                    value: None,
                    secret_key_ref: Some(crate::types::SecretKeyRef {
                        name: m.password_secret.clone(),
                        key: m.password_key.clone(),
                    }),
                });
                probe.env = Some(env);
                probe.command = Some(vec![
                    "sh".into(),
                    "-c".into(),
                    pitr_marker_probe_sql(m, &host),
                ]);
                probe.args = None;
                tokio::task::block_in_place(|| rt.block_on(self.run_probe_job(&probe, &image)))
            }
            AssertionKind::RolloutSurgeBound
            | AssertionKind::LogWindow
            | AssertionKind::MetricInRange
            | AssertionKind::HpaBehavior
            | AssertionKind::ExecMetric
            | AssertionKind::ObjectFields
            | AssertionKind::WorkflowJobs
            | AssertionKind::StepsCompleted
            | AssertionKind::GuardHeld
            | AssertionKind::NaturalStateRestored => Ok(Verdict::Errored {
                message: format!(
                    "{:?} is an operator-side kind: run it with `ensaio visita`, which reads \
                     through the target's own API from the operator's machine",
                    a.kind
                ),
            }),
            AssertionKind::CustomJob | AssertionKind::AmqpRoundtrip => {
                let image = a
                    .image
                    .clone()
                    .ok_or_else(|| format!("{:?} {}: assertion.image is required", a.kind, a.id))?;
                tokio::task::block_in_place(|| rt.block_on(self.run_probe_job(a, &image)))
            }
        }
    }

    fn teardown_system(&self, endpoint: &str) -> Result<(), String> {
        let name = endpoint
            .split('.')
            .next()
            .ok_or_else(|| format!("bad endpoint {endpoint}"))?;
        let deployments: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
        let patch = serde_json::json!({ "spec": { "replicas": 0 } });
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| format!("no tokio runtime available: {e}"))?;
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                deployments
                    .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
                    .await
                    .map_err(|e| format!("teardown (scale-to-zero) {name}: {e}"))?;
                Ok(())
            })
        })
    }

    /// **Honest stub — not the real implementation.** The
    /// reactive-nervous-system publish is a real M5+
    /// candidate; wiring an actual `escuta`/NATS client here (a new
    /// `async-nats` dependency, connection lifecycle management threaded
    /// through `K8sEnvironment::new`, and reconnect/backoff handling) is
    /// a genuine increment this pass does not build. The trait seam
    /// (`EnsaioEnvironment::publish_phase_event`) is real and load-bearing
    /// — `controller::step_phase` calls it at every phase transition, and
    /// `MockEnv` (`src/mock.rs`) proves those call sites fire — only the
    /// K8s-backed *sender* is deferred. Returning `Ok(())` here means a
    /// live cluster silently drops nervous-system events until the real
    /// client lands; nothing consumes them yet (no controller subscribes
    /// on `ensaio.*` today), so this is a no-op, not a lie about a
    /// promise being kept.
    fn publish_phase_event(&self, _event: &PhaseEvent) -> Result<(), String> {
        Ok(())
    }

    /// **REFUSES, and that is the honest answer — a `Fixture` carries no
    /// executable payload.**
    ///
    /// `crate::fixture::Fixture` declares a `name`, an `Effect`, a `Cleanup`
    /// and an `after` graph. It does not declare WHAT to place: no manifest, no
    /// command, no target, no body. So there is nothing for a live executor to
    /// run, and an `Ok(())` here would be the silent no-op the whole seam exists
    /// to end — a declared fixture, a green run, and no data placed.
    ///
    /// This is narrower than the blanket spec-level refusal it replaced. That
    /// one rejected any spec carrying fixtures, including one run against an
    /// environment that could place them; this rejects exactly the environment
    /// that cannot, while `apply`'s ordering, cleanup and residue accounting are
    /// live and exercised end to end by `MockEnv`.
    ///
    /// Closing it is a DECLARATION change, not an executor change: `Fixture`
    /// needs a typed payload (the sibling of `AssertionSpec`'s
    /// `image`/`command`/`args`, or a manifest arm) before this method has
    /// anything to do. Named, not stubbed.
    fn place_fixture(&self, f: &crate::fixture::Fixture) -> Result<(), String> {
        Err(format!(
            "fixture `{}`: the declaration carries no executable payload (name, effect, cleanup \
             and ordering only), so this environment has nothing to place. Refusing rather than \
             reporting a placement that did not happen.",
            f.name
        ))
    }

    /// Unreachable in practice — `apply` only cleans a fixture it placed, and
    /// [`Self::place_fixture`] places none. Stated rather than `unreachable!()`:
    /// a reconcile must never panic, and a typed refusal reads the same way in
    /// a log whichever path reached it.
    fn clean_fixture(&self, f: &crate::fixture::Fixture) -> Result<(), String> {
        Err(format!(
            "fixture `{}`: nothing was placed by this environment, so there is nothing to undo",
            f.name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssertionKind, EnvVarSpec, SecretKeyRef};
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus,
        PodCondition,
    };

    // ── EnvironmentHealth fixtures ─────────────────────────────────────
    //
    // Hand-built pods, not cluster reads: the point of the pure classifier is
    // that every one of its wrong answers is reachable from a test.

    fn container_status(name: &str, state: ContainerState, restarts: i32) -> ContainerStatus {
        ContainerStatus {
            name: name.to_string(),
            state: Some(state),
            restart_count: restarts,
            ready: false,
            image: "irrelevant".to_string(),
            image_id: String::new(),
            ..Default::default()
        }
    }

    fn pod(name: &str, phase: &str, ready: bool, statuses: Vec<ContainerStatus>) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some(phase.to_string()),
                conditions: Some(vec![PodCondition {
                    type_: "Ready".to_string(),
                    status: if ready { "True" } else { "False" }.to_string(),
                    ..Default::default()
                }]),
                container_statuses: Some(statuses),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn healthy_pod(name: &str) -> Pod {
        pod(
            name,
            "Running",
            true,
            vec![container_status(
                "app",
                ContainerState {
                    running: Some(Default::default()),
                    ..Default::default()
                },
                0,
            )],
        )
    }

    /// A finished Job pod, exactly as the apiserver reports one: `Succeeded`,
    /// `Ready: False`, container terminated with exit 0.
    fn completed_job_pod(name: &str) -> Pod {
        pod(
            name,
            "Succeeded",
            false,
            vec![container_status(
                "bootstrap",
                ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code: 0,
                        reason: Some("Completed".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                0,
            )],
        )
    }

    fn waiting_pod(name: &str, reason: &str, restarts: i32) -> Pod {
        pod(
            name,
            "Running",
            false,
            vec![container_status(
                "app",
                ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some(reason.to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                restarts,
            )],
        )
    }

    fn findings_text(v: &Verdict) -> String {
        match v {
            Verdict::Falsified { findings, .. } => findings
                .iter()
                .map(Observation::as_str)
                .collect::<Vec<_>>()
                .join(" | "),
            other => panic!("expected Falsified, got {other:?}"),
        }
    }

    fn subjects_text(v: &Verdict) -> String {
        v.subjects()
            .expect("a claim-making verdict carries its subjects")
            .iter()
            .map(Observation::as_str)
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// **The correctness core of this kind.** A completed Job pod reports
    /// `Ready: False` forever, so a classifier that reads the condition before
    /// the phase falsifies every environment that ever ran a Job — which is
    /// every spawned application environment. Held here, with the Job counted as
    /// Completed rather than as a failure.
    #[test]
    fn a_completed_job_pod_is_healthy_not_a_failure() {
        let pods = vec![
            healthy_pod("rabbitmq-0"),
            completed_job_pod("app-bootstrap-x9k2p"),
        ];
        let v = K8sEnvironment::judge_environment_health(&pods, "viveiro-env-abc");
        assert!(
            v.held(),
            "a finished Job must not falsify the namespace: {v:?}"
        );
        let subjects = subjects_text(&v);
        assert!(
            subjects.contains("app-bootstrap-x9k2p: Completed"),
            "the Job pod is classified Completed, not Ready and not broken: {subjects}"
        );
        assert!(
            subjects.contains(
                "2 pod(s) inspected in viveiro-env-abc — 1 Ready, 1 Completed, 0 unhealthy"
            ),
            "denominator + the breakdown ride inside the verdict: {subjects}"
        );
    }

    /// RED RUN — a crash-looping pod alongside healthy ones. This is the shape
    /// `wait_ready` reported Ready for.
    #[test]
    fn a_crashloopbackoff_pod_falsifies_and_the_reason_is_named() {
        let pods = vec![
            healthy_pod("rabbitmq-0"),
            healthy_pod("mysql-0"),
            waiting_pod("worker-7df76fbd6-h6mxl", "CrashLoopBackOff", 12),
        ];
        let v = K8sEnvironment::judge_environment_health(&pods, "viveiro-env-abc");
        assert!(!v.held());
        let findings = findings_text(&v);
        assert!(
            findings.contains(
                "worker-7df76fbd6-h6mxl: NOT healthy — container app is waiting: CrashLoopBackOff"
            ),
            "the finding names the pod AND kubelet's own reason: {findings}"
        );
        assert!(
            findings.contains("[12 restart(s)]"),
            "restart count is carried as evidence: {findings}"
        );
        // Only the offender is a finding; the healthy pods stay subjects.
        assert_eq!(
            match &v {
                Verdict::Falsified { findings, .. } => findings.count().get(),
                other => panic!("expected Falsified, got {other:?}"),
            },
            1,
            "exactly one pod is at fault, and the verdict says which"
        );
        assert_eq!(v.subject_count(), 4, "3 pods + the denominator line");
    }

    /// RED RUN — an image that cannot be pulled. Distinct from the crash-loop
    /// case because the failure is in `waiting`, before the container ever ran.
    #[test]
    fn an_imagepullbackoff_pod_falsifies_and_the_reason_is_named() {
        let pods = vec![
            healthy_pod("rabbitmq-0"),
            waiting_pod("sdr-1", "ImagePullBackOff", 0),
        ];
        let v = K8sEnvironment::judge_environment_health(&pods, "viveiro-env-abc");
        let findings = findings_text(&v);
        assert!(
            findings.contains("sdr-1: NOT healthy — container app is waiting: ImagePullBackOff"),
            "{findings}"
        );
    }

    /// RED RUN — a pod that ran and exited non-zero. `phase: Failed` must not
    /// take the `Succeeded` path just because it is also terminal.
    #[test]
    fn a_failed_pod_falsifies_and_carries_its_exit_code() {
        let failed = pod(
            "app-schema-apply-abc",
            "Failed",
            false,
            vec![container_status(
                "migrate",
                ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code: 137,
                        reason: Some("OOMKilled".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                0,
            )],
        );
        let v = K8sEnvironment::judge_environment_health(&[failed], "viveiro-env-abc");
        let findings = findings_text(&v);
        assert!(findings.contains("phase Failed"), "{findings}");
        assert!(findings.contains("OOMKilled"), "{findings}");
        assert!(findings.contains("exit 137"), "{findings}");
    }

    /// RED RUN — the anti-vacuity gate. An empty namespace must never read as
    /// healthy, and must not demote to `Vacuous` either: the namespace WAS
    /// examined, and finding nothing running is evidence against health.
    #[test]
    fn an_empty_namespace_falsifies_and_says_nothing_was_checked() {
        let v = K8sEnvironment::judge_environment_health(&[], "viveiro-env-abc");
        assert!(!v.held(), "zero pods must never hold");
        assert!(
            v.falsified(),
            "zero pods is a falsification, not Vacuous/Inconclusive: {v:?}"
        );
        let findings = findings_text(&v);
        assert!(
            findings.contains("no pods found in viveiro-env-abc — nothing was actually checked"),
            "{findings}"
        );
        let subjects = subjects_text(&v);
        assert!(
            subjects.contains("0 pod(s) inspected in viveiro-env-abc"),
            "the denominator is stated even when it is zero: {subjects}"
        );
    }

    /// A pod the apiserver returned with no `status` block at all is unhealthy,
    /// never silently skipped — skipping it would shrink the denominator to
    /// match the answer.
    #[test]
    fn a_pod_with_no_status_is_unhealthy_not_skipped() {
        let bare = Pod {
            metadata: ObjectMeta {
                name: Some("half-created".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let v = K8sEnvironment::judge_environment_health(&[bare], "viveiro-env-abc");
        let findings = findings_text(&v);
        assert!(
            findings.contains("the apiserver reported no status at all"),
            "{findings}"
        );
        assert!(
            subjects_text(&v).contains("1 pod(s) inspected"),
            "it still counts toward the denominator"
        );
    }

    /// A failing native sidecar (a `restartPolicy: Always` initContainer) is
    /// reported as the cause; the main container's `PodInitializing` is the
    /// symptom and must not win the reason slot.
    #[test]
    fn a_failing_init_container_wins_the_reason_over_podinitializing() {
        let mut p = waiting_pod("probe-with-relay", "PodInitializing", 0);
        p.status.as_mut().unwrap().init_container_statuses = Some(vec![container_status(
            "relay",
            ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("CrashLoopBackOff".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            5,
        )]);
        let v = K8sEnvironment::judge_environment_health(&[p], "viveiro-env-abc");
        let findings = findings_text(&v);
        assert!(
            findings.contains("container relay is waiting: CrashLoopBackOff"),
            "the init container is named, not the main one: {findings}"
        );
    }

    /// A namespace of nothing but finished Jobs holds — and says so with a
    /// `0 Ready, N Completed` breakdown, so a reader can tell it apart from a
    /// running environment at a glance.
    #[test]
    fn a_namespace_of_only_completed_jobs_holds_and_the_breakdown_says_so() {
        let pods = vec![completed_job_pod("a"), completed_job_pod("b")];
        let v = K8sEnvironment::judge_environment_health(&pods, "ns");
        assert!(v.held());
        assert!(
            subjects_text(&v)
                .contains("2 pod(s) inspected in ns — 0 Ready, 2 Completed, 0 unhealthy"),
            "{}",
            subjects_text(&v)
        );
    }

    fn assertion_with_env(env: Option<Vec<EnvVarSpec>>) -> AssertionSpec {
        AssertionSpec {
            id: "probe".into(),
            kind: AssertionKind::CustomJob,
            target_namespace: None,
            target: "cluster".into(),
            proof_channel: None,
            session: None,
            header_name: None,
            from: None,
            must_not_traverse: None,
            marker: None,
            image: Some("curlimages/curl:8.11.1".into()),
            command: Some(vec!["sh".into(), "-c".into(), "true".into()]),
            args: None,
            sidecar: None,
            env,
            probe_namespace: Some("trials".into()),
            operator: crate::types::OperatorParams::default(),
        }
    }

    #[test]
    fn probe_env_always_carries_node_name_first() {
        let a = assertion_with_env(None);
        let env = K8sEnvironment::probe_env(&a);
        assert_eq!(env.len(), 1, "no declared env => just NODE_NAME");
        assert_eq!(env[0].name, "NODE_NAME");
        assert!(
            env[0].value_from.as_ref().unwrap().field_ref.is_some(),
            "NODE_NAME is a Downward-API field_ref"
        );
    }

    #[test]
    fn probe_env_literal_and_secret_key_ref() {
        let a = assertion_with_env(Some(vec![
            EnvVarSpec {
                name: "RABBITMQ_MGMT_USER".into(),
                value: Some("guest".into()),
                secret_key_ref: None,
            },
            EnvVarSpec {
                name: "RABBITMQ_MGMT_PASSWORD".into(),
                value: None,
                secret_key_ref: Some(SecretKeyRef {
                    name: "rabbitmq-topology-admin".into(),
                    key: "password".into(),
                }),
            },
        ]));
        let env = K8sEnvironment::probe_env(&a);
        // NODE_NAME + the two declared vars, in order.
        assert_eq!(env.len(), 3);
        assert_eq!(env[0].name, "NODE_NAME");

        let user = &env[1];
        assert_eq!(user.name, "RABBITMQ_MGMT_USER");
        assert_eq!(user.value.as_deref(), Some("guest"));
        assert!(user.value_from.is_none());

        // The credential rides a secretKeyRef — NEVER a literal value in
        // the pod spec (the zero-plaintext discipline).
        let pass = &env[2];
        assert_eq!(pass.name, "RABBITMQ_MGMT_PASSWORD");
        assert!(pass.value.is_none(), "credential is not a literal value");
        let sk = pass
            .value_from
            .as_ref()
            .unwrap()
            .secret_key_ref
            .as_ref()
            .unwrap();
        assert_eq!(sk.name, "rabbitmq-topology-admin");
        assert_eq!(sk.key, "password");
    }

    #[test]
    fn probe_env_drops_malformed_entry() {
        // Neither value nor secret_key_ref => dropped, never an empty var.
        let a = assertion_with_env(Some(vec![EnvVarSpec {
            name: "EMPTY".into(),
            value: None,
            secret_key_ref: None,
        }]));
        let env = K8sEnvironment::probe_env(&a);
        assert_eq!(env.len(), 1, "malformed entry dropped, only NODE_NAME left");
        assert_eq!(env[0].name, "NODE_NAME");
    }

    /// A `CustomJob` (no `sidecar`) renders a single-container Pod — the
    /// existing behaviour, unchanged by the two-container refactor.
    #[test]
    fn build_probe_job_single_container_when_no_sidecar() {
        let a = assertion_with_env(None);
        let job = K8sEnvironment::build_probe_job(&a, "curlimages/curl:8.11.1", "trials", None);
        let pod = job.spec.unwrap().template.spec.unwrap();
        assert!(
            pod.init_containers.is_none(),
            "no sidecar => no init containers"
        );
        assert_eq!(pod.containers.len(), 1, "single-container probe");
        assert_eq!(pod.containers[0].name, "probe");
    }

    /// The AMQP client-plane roundtrip (`AmqpRoundtrip`) renders the muda
    /// **two-container absorbed-test shape**: a native-sidecar relay
    /// (`initContainer` + `restartPolicy: Always` + an `exec` `startupProbe`)
    /// alongside the main probe container running the upstream test binary
    /// (`command` + `args`). Proven by rendering — no live cluster.
    #[test]
    fn build_probe_job_renders_two_container_absorbed_test_shape() {
        let a = AssertionSpec {
            id: "amqp-client-plane-roundtrip".into(),
            kind: AssertionKind::AmqpRoundtrip,
            target_namespace: None,
            target: "cluster".into(),
            proof_channel: None,
            session: None,
            header_name: None,
            from: None,
            must_not_traverse: None,
            marker: None,
            image: Some("ghcr.io/example/rabbitmq-absorbed-test:rev-51aafcef3101".into()),
            command: Some(vec!["/bin/rabbitmqtest.test".into()]),
            args: Some(vec!["-test.v".into(), "-test.timeout=600s".into()]),
            sidecar: Some(crate::types::SidecarSpec {
                name: Some("relay".into()),
                image: "ghcr.io/pleme-io/socat-relay:nix0".into(),
                command: Some(vec!["socat".into()]),
                args: Some(vec![
                    "-d".into(),
                    "TCP-LISTEN:5672,bind=127.0.0.1,fork,reuseaddr".into(),
                    "TCP:rabbitmq:5672".into(),
                ]),
                startup_probe_command: Some(vec![
                    "socat".into(),
                    "-u".into(),
                    "OPEN:/dev/null".into(),
                    "TCP:127.0.0.1:5672,connect-timeout=1".into(),
                ]),
            }),
            env: None,
            probe_namespace: None,
            operator: crate::types::OperatorParams::default(),
        };
        let job = K8sEnvironment::build_probe_job(
            &a,
            a.image.as_deref().unwrap(),
            "viveiro-env-c1-env",
            None,
        );
        let pod = job.spec.unwrap().template.spec.unwrap();

        // ── the relay is a native sidecar (initContainer, restartPolicy Always) ──
        let inits = pod
            .init_containers
            .expect("AmqpRoundtrip with a sidecar must render an initContainer");
        assert_eq!(inits.len(), 1);
        let relay = &inits[0];
        assert_eq!(relay.name, "relay");
        assert_eq!(
            relay.image.as_deref(),
            Some("ghcr.io/pleme-io/socat-relay:nix0")
        );
        assert_eq!(
            relay.restart_policy.as_deref(),
            Some("Always"),
            "the KEP-753 native-sidecar marker — start first, run for the Pod's life"
        );
        // The startupProbe is `exec` (never tcpSocket — the pod-local loopback trap).
        let probe = relay.startup_probe.as_ref().expect("sidecar startupProbe");
        assert!(
            probe.exec.is_some(),
            "startupProbe must be exec, never tcpSocket"
        );
        assert!(probe.tcp_socket.is_none());

        // ── the main container runs the upstream test binary (command + args) ──
        assert_eq!(pod.containers.len(), 1);
        let main = &pod.containers[0];
        assert_eq!(main.name, "probe");
        assert_eq!(
            main.command.as_deref(),
            Some(&["/bin/rabbitmqtest.test".to_string()][..])
        );
        assert_eq!(
            main.args.as_deref(),
            Some(&["-test.v".to_string(), "-test.timeout=600s".to_string()][..])
        );
    }
}

/// Default probe image for [`AssertionKind::PitrDataRoundtrip`].
///
/// Pinned to the same major.minor the restore server runs: a client from a
/// different line can fail the handshake against a restored 8.0 server for
/// reasons that read as a credential problem. `assertion.image` overrides it.
const DEFAULT_MYSQL_PROBE_IMAGE: &str = "mysql:8.0";

/// The read-back a [`AssertionKind::PitrDataRoundtrip`] runs against the
/// RESTORED database.
///
/// ★ WHY THE PREDICATE IS THIS STRICT, clause by clause. Each one exists
/// because a weaker version passes on a restore that proved nothing:
///
/// - `WHERE correlation_id = <this drill's>` — without it, a marker left behind
///   by an EARLIER drill satisfies the assertion. That is the difference between
///   proving this restore worked and proving the table has ever been written to.
/// - `AND checksum = SHA2(payload, 256)` — the seed had MySQL compute the
///   checksum from the payload, so recomputing it here catches a row that came
///   back truncated, re-encoded or partially written. A match on the id alone
///   would call that a pass.
/// - `COUNT(*) = 1`, not `>= 1` — two rows for one correlation means something
///   other than the seed wrote one, and an assertion that shrugs at that is not
///   evidence of anything.
///
/// The exit code IS the verdict: `run_probe_job` reads the Job's terminal state,
/// so the SQL must make a failure a non-zero exit rather than a message. `-N -B`
/// strips headers and formatting so the comparison is against the value, not a
/// rendering of it, and `--connect-timeout` keeps a wrong host from consuming the
/// probe's whole deadline before saying so.
///
/// The password arrives via `MYSQL_PWD` from a Secret, never on argv: a password
/// in a command line is visible in `ps`, in the Pod spec, and in any log that
/// echoes the container's command.
fn pitr_marker_probe_sql(m: &crate::types::PitrMarker, host: &str) -> String {
    // Identifiers are interpolated (MySQL has no bind parameters for them) and
    // backtick-quoted; VALUES are single-quoted literals. Single quotes and not
    // double, because MySQL reads a double-quoted token as an identifier under
    // ANSI_QUOTES — the same trap the seed template carries a note about.
    format!(
        "set -eu; \
         export MYSQL_PWD=\"$MARKER_PASSWORD\"; \
         n=$(mysql -h '{host}' -u '{user}' -N -B --connect-timeout=10 \
             -e \"SELECT COUNT(*) FROM \\`{db}\\`.\\`{table}\\` \
                  WHERE correlation_id = '{corr}' \
                    AND checksum = SHA2(payload, 256);\"); \
         echo \"marker rows matching correlation {corr} with a valid checksum: $n\"; \
         if [ \"$n\" != \"1\" ]; then \
           echo 'FAIL: expected exactly 1 intact marker row in the RESTORED database.' >&2; \
           echo 'A restore that produced an empty database, or one predating the seed,' >&2; \
           echo 'reaches exactly this state — the snapshot and PVC bind either way.' >&2; \
           exit 1; \
         fi; \
         echo 'PASS: the row seeded before the snapshot came back intact.'",
        host = host,
        user = m.user,
        db = m.database,
        table = m.table,
        corr = m.correlation_id,
    )
}

#[cfg(test)]
mod pitr_roundtrip_tests {
    use super::*;
    use crate::types::PitrMarker;

    fn marker() -> PitrMarker {
        let name = "app-mysql-root";
        PitrMarker {
            correlation_id: "drill-deadbeef".into(),
            database: "pitr_drill".into(),
            table: "marker".into(),
            user: "root".into(),
            password_secret: name.into(),
            password_key: "root-password".into(),
        }
    }

    /// Every clause below exists because a weaker predicate passes on a restore
    /// that proved nothing. The test asserts the clauses individually, not the
    /// whole string, so a future edit that drops ONE of them fails naming which.
    #[test]
    fn the_probe_predicate_cannot_be_satisfied_by_a_useless_restore() {
        let sql = pitr_marker_probe_sql(&marker(), "mysql-restore.env-ff.svc.cluster.local");

        // Scoped to THIS drill. Without it, a marker an earlier drill left behind
        // satisfies the assertion — proving the table has ever been written to,
        // not that this restore worked.
        assert!(
            sql.contains("correlation_id = 'drill-deadbeef'"),
            "the predicate must name this drill's correlation:\n{sql}"
        );

        // Recomputes the checksum. A row that came back truncated or re-encoded
        // still matches on id alone; this is what makes it evidence of INTACT
        // bytes rather than of a row's existence.
        assert!(
            sql.contains("checksum = SHA2(payload, 256)"),
            "the predicate must recompute the payload checksum:\n{sql}"
        );

        // Exactly one. Two rows for one correlation means something other than
        // the seed wrote one, and an assertion that shrugs at that is not
        // evidence.
        assert!(
            sql.contains(r#""$n" != "1""#),
            "the probe must require exactly one row, not at-least-one:\n{sql}"
        );

        // Reads the RESTORED host, not the source. Reading the source would
        // report the live row as proof of a restore — a pass that means the
        // opposite of what it claims.
        assert!(
            sql.contains("mysql-restore.env-ff.svc.cluster.local"),
            "the probe must address the restored host:\n{sql}"
        );

        // The exit code IS the verdict: run_probe_job reads the Job's terminal
        // state, so a failure must be a non-zero exit and not a printed message.
        assert!(
            sql.contains("exit 1"),
            "a failure must exit non-zero:\n{sql}"
        );
        assert!(
            sql.contains("set -eu"),
            "an unexpected error must not read as a pass:\n{sql}"
        );
    }

    /// The password must never reach argv. It is visible there in `ps`, in the
    /// rendered Pod spec, and in any log that echoes the container's command.
    #[test]
    fn the_password_never_appears_on_the_command_line() {
        let sql = pitr_marker_probe_sql(&marker(), "h");
        assert!(
            !sql.contains("-p") || !sql.contains("--password"),
            "no password flag may appear in the probe command:\n{sql}"
        );
        assert!(
            sql.contains("MYSQL_PWD=\"$MARKER_PASSWORD\""),
            "the password must arrive through the environment:\n{sql}"
        );
    }

    /// MySQL reads a double-quoted token as an IDENTIFIER under ANSI_QUOTES, so
    /// a value written with double quotes means something different depending on
    /// a server setting nothing here controls. Same trap the seed template
    /// carries a note about; asserted on both sides so they cannot drift apart.
    #[test]
    fn values_are_single_quoted_identifiers_are_backticked() {
        let sql = pitr_marker_probe_sql(&marker(), "h");
        assert!(
            sql.contains("'drill-deadbeef'"),
            "values single-quoted:\n{sql}"
        );
        // Backticks arrive BACKSLASH-ESCAPED, and that is required rather than
        // incidental: the SQL travels inside a double-quoted shell string
        // (`mysql -e "..."`), and inside double quotes a bare backtick starts
        // COMMAND SUBSTITUTION. The shell strips the backslash and MySQL sees a
        // plain backtick-quoted identifier. Asserting the escaped form is what
        // makes this test able to catch someone "tidying" the backslashes away —
        // which would turn the table name into a subshell.
        assert!(
            sql.contains(r"\`pitr_drill\`"),
            "identifiers must be backticked AND shell-escaped:\n{sql}"
        );
        assert!(
            sql.contains(r"\`marker\`"),
            "identifiers must be backticked AND shell-escaped:\n{sql}"
        );
    }
}
