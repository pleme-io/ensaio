//! `visita`: snapshot → mutate → undo → diff, against an in-memory cluster
//! behind the same `Runner` seam the CLI drives kubectl through.
//!
//! SYNTHETIC-FIXTURE: every credential-shaped value below is fake and says so.
//!
//! The live counterpart is `specs/qual-envtrial*.visita.lisp` against a local
//! cluster; these pin the same properties hermetically.

use std::cell::RefCell;
use std::collections::BTreeMap;

use ensaio::visita::command::{Evidence, Exec, Operator, Runner};
use ensaio::visita::judge::{self, RolloutSample};
use ensaio::visita::{OperatorEnvironment, VisitaSpec, run};
use ensaio::{CertVerdict, Presence, Verdict, load_one};
use serde_json::{Value, json};

// ── an in-memory cluster that speaks the argv `Verb` produces ─────────────

#[derive(Default)]
struct FakeCluster {
    objects: RefCell<BTreeMap<(String, String, String), Value>>,
    calls: RefCell<Vec<Vec<String>>>,
    rabbit_closed: RefCell<String>,
    /// What `gh workflow run` prints. Default: a run URL.
    dispatch_out: RefCell<Option<String>>,
    /// Successive `gh run view` answers; the last one repeats.
    run_views: RefCell<Vec<Value>>,
    /// Whether `gh run cancel` actually stops the run.
    cancel_works: RefCell<bool>,
    cancelled: RefCell<bool>,
    /// Per-pod log text; a pod absent here answers NotFound (it vanished).
    pod_logs: RefCell<BTreeMap<String, String>>,
    /// Merge patches applied to Deployment `svc`, one per `gh run view`: the
    /// pipeline rolling the workload while the trial awaits it.
    deploy_script: RefCell<Vec<Value>>,
    /// Whether the fake kubelet reports a resize in `status` (converges).
    kubelet_applies: RefCell<bool>,
    /// After a resize, the pod goes not-Ready (a starved broker).
    starve_on_resize: RefCell<bool>,
    /// When set, only this pod starves on a resize.
    starve_only: RefCell<Option<String>>,
    /// Successive outputs of an exec; the last repeats.
    exec_script: RefCell<Vec<String>>,
    /// Per branch, successive `behind_by` answers of the compare API; the
    /// last repeats; an unlisted branch is 0 behind.
    behind: RefCell<BTreeMap<String, Vec<i64>>>,
    /// Successive `get events` answers; the last repeats; none is an empty list.
    events: RefCell<Vec<Value>>,
    /// A created Deployment never reports Available.
    created_unavailable: RefCell<bool>,
    /// `kubectl delete` answers success and removes nothing.
    deletes_ignored: RefCell<bool>,
    /// An exec whose argv contains one of these exits 1.
    exec_fails: RefCell<Vec<String>>,
    uids: RefCell<u32>,
    object_reads: RefCell<Vec<ObjRead>>,
    run_jobs: RefCell<Vec<Value>>,
    view_failures: RefCell<Vec<String>>,
    cancel_failures: RefCell<Vec<String>>,
    jobs_failures: RefCell<Vec<String>>,
    job_logs: RefCell<BTreeMap<String, String>>,
    job_annotations: RefCell<BTreeMap<String, Value>>,
}

#[derive(Clone)]
enum ObjRead {
    State(Option<Value>),
    Fail(&'static str),
}

fn kind_of(resource: &str) -> &'static str {
    match resource.split('.').next().unwrap() {
        "secret" | "secrets" => "Secret",
        "configmap" | "configmaps" => "ConfigMap",
        "deployment" | "deployments" => "Deployment",
        "hpa" | "horizontalpodautoscalers" => "HorizontalPodAutoscaler",
        "pods" | "pod" => "Pod",
        "drillsession" => "DrillSession",
        "applications" => "Application",
        "autoscalingrunnersets" => "AutoscalingRunnerSet",
        "namespaces" => "Namespace",
        other => panic!("fake cluster: unknown resource {other}"),
    }
}

fn merge(target: &mut Value, patch: &Value) {
    if let Value::Object(p) = patch {
        if !target.is_object() {
            *target = json!({});
        }
        let t = target.as_object_mut().unwrap();
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                merge(t.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    } else {
        *target = patch.clone();
    }
}

impl FakeCluster {
    fn put(&self, ns: &str, obj: Value) {
        let kind = obj["kind"].as_str().unwrap().to_string();
        let name = obj["metadata"]["name"].as_str().unwrap().to_string();
        self.objects
            .borrow_mut()
            .insert((ns.into(), kind, name), obj);
    }

    fn get(&self, ns: &str, kind: &str, name: &str) -> Value {
        self.objects.borrow()[&(ns.to_string(), kind.to_string(), name.to_string())].clone()
    }

    fn mutating_calls(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|a| {
                a.iter()
                    .any(|x| x == "annotate" || x == "patch" || x == "exec")
            })
            .count()
    }
}

impl Runner for FakeCluster {
    fn run(&self, argv: &[String]) -> Exec {
        self.calls.borrow_mut().push(argv.to_vec());
        let ok = |s: String| Exec {
            status: 0,
            stdout: s,
            stderr: String::new(),
        };
        if argv[0] == "gh" {
            let scripted = |q: &RefCell<Vec<String>>| {
                let mut q = q.borrow_mut();
                (!q.is_empty()).then(|| Exec {
                    status: 1,
                    stdout: String::new(),
                    stderr: q.remove(0),
                })
            };
            let failure = match (argv[1].as_str(), argv[2].as_str()) {
                ("run", "view") => scripted(&self.view_failures),
                ("run", "cancel") => scripted(&self.cancel_failures),
                ("api", p) if p.contains("/actions/runs/") && p.contains("/jobs") => {
                    scripted(&self.jobs_failures)
                }
                _ => None,
            };
            if let Some(f) = failure {
                return f;
            }
            return match (argv[1].as_str(), argv[2].as_str()) {
                ("workflow", "run") => {
                    ok(self.dispatch_out.borrow().clone().unwrap_or_else(|| {
                        "https://github.com/example-org/example-repo/actions/runs/424242\n".into()
                    }))
                }
                ("run", "view") => {
                    let step = {
                        let mut script = self.deploy_script.borrow_mut();
                        (!script.is_empty()).then(|| script.remove(0))
                    };
                    if let Some(patch) = step {
                        let mut objs = self.objects.borrow_mut();
                        let key = (
                            "stage".to_string(),
                            "Deployment".to_string(),
                            "svc".to_string(),
                        );
                        merge(objs.get_mut(&key).unwrap(), &patch);
                    }
                    if *self.cancelled.borrow() && *self.cancel_works.borrow() {
                        return ok(
                            json!({"status": "completed", "conclusion": "cancelled", "jobs": []})
                                .to_string(),
                        );
                    }
                    let mut views = self.run_views.borrow_mut();
                    let v = if views.len() > 1 {
                        views.remove(0)
                    } else {
                        views[0].clone()
                    };
                    ok(v.to_string())
                }
                ("api", path) if path.contains("/compare/") => {
                    let branch = path.rsplit("...").next().unwrap().to_string();
                    let mut all = self.behind.borrow_mut();
                    let seq = all.entry(branch).or_insert_with(|| vec![0]);
                    let b = if seq.len() > 1 { seq.remove(0) } else { seq[0] };
                    ok(json!({"status": if b > 0 { "diverged" } else { "ahead" }, "behind_by": b, "ahead_by": 2}).to_string())
                }
                ("run", "cancel") => {
                    *self.cancelled.borrow_mut() = true;
                    ok(String::new())
                }
                ("api", path) if path.contains("/actions/runs/") && path.contains("/jobs") => {
                    let mut answers = self.run_jobs.borrow_mut();
                    let v = match answers.len() {
                        0 => json!({"total_count": 0, "jobs": []}),
                        1 => answers[0].clone(),
                        _ => answers.remove(0),
                    };
                    ok(v.to_string())
                }
                ("api", path) if path.ends_with("/logs") => {
                    let id = path
                        .trim_end_matches("/logs")
                        .rsplit('/')
                        .next()
                        .unwrap()
                        .to_string();
                    match self.job_logs.borrow().get(&id) {
                        Some(t) => ok(t.clone()),
                        None => Exec {
                            status: 1,
                            stdout: String::new(),
                            stderr: "gh: Not Found (HTTP 404)".into(),
                        },
                    }
                }
                ("api", path) if path.contains("/annotations") => {
                    let id = path
                        .split("/check-runs/")
                        .nth(1)
                        .unwrap()
                        .split('/')
                        .next()
                        .unwrap()
                        .to_string();
                    ok(self
                        .job_annotations
                        .borrow()
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| json!([]))
                        .to_string())
                }
                other => panic!("fake gh: {other:?}"),
            };
        }
        assert_eq!(argv[1], "--context", "every kubectl call names its context");
        if argv[3] == "get" && argv[4] == "--raw" {
            return ok(String::new());
        }
        let ns = argv[4].clone();
        let verb = argv[5].as_str();
        if verb == "create" {
            assert_eq!(
                argv[6..],
                [
                    "-f".to_string(),
                    argv[7].clone(),
                    "-o".into(),
                    "json".into()
                ]
            );
            let list: Value =
                serde_json::from_str(&std::fs::read_to_string(&argv[7]).unwrap()).unwrap();
            let mut made = Vec::new();
            for o in list["items"].as_array().unwrap() {
                let (kind, name) = (
                    o["kind"].as_str().unwrap().to_string(),
                    o["metadata"]["name"].as_str().unwrap().to_string(),
                );
                if self
                    .objects
                    .borrow()
                    .contains_key(&(ns.clone(), kind.clone(), name.clone()))
                {
                    return Exec {
                        status: 1,
                        stdout: String::new(),
                        stderr: format!(
                            "Error from server (AlreadyExists): {kind} \"{name}\" already exists"
                        ),
                    };
                }
                let mut o = o.clone();
                *self.uids.borrow_mut() += 1;
                o["metadata"]["uid"] = json!(format!("uid-{}", self.uids.borrow()));
                if kind == "Deployment" {
                    let up = if *self.created_unavailable.borrow() {
                        "False"
                    } else {
                        "True"
                    };
                    o["status"] = json!({"conditions": [{"type": "Available", "status": up}]});
                }
                self.put(&ns, o.clone());
                made.push(o);
            }
            return ok(json!({"kind": "List", "items": made}).to_string());
        }
        if verb == "delete" {
            assert_eq!(argv[8], "--wait=false");
            let key = (ns.clone(), kind_of(&argv[6]).to_string(), argv[7].clone());
            if *self.deletes_ignored.borrow() {
                return ok(String::new());
            }
            return match self.objects.borrow_mut().remove(&key) {
                Some(_) => ok(String::new()),
                None => Exec {
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("Error from server (NotFound): \"{}\" not found", argv[7]),
                },
            };
        }
        if verb == "get" && argv[6] == "events" {
            assert_eq!(argv[7..10], ["--field-selector", "reason=Unhealthy", "-o"]);
            let mut evs = self.events.borrow_mut();
            let v = match evs.len() {
                0 => json!({"kind": "List", "items": []}),
                1 => evs[0].clone(),
                _ => evs.remove(0),
            };
            return ok(v.to_string());
        }
        match verb {
            "get" if argv[7] != "-o" && argv[7] != "-l" => {
                // `get <resource> <name>… -o json`: named objects only.
                let kind = kind_of(&argv[6]);
                let wanted: Vec<&String> = argv[7..].iter().take_while(|a| *a != "-o").collect();
                if kind == "DrillSession" {
                    let step = {
                        let mut script = self.object_reads.borrow_mut();
                        match script.len() {
                            0 => None,
                            1 => Some(script[0].clone()),
                            _ => Some(script.remove(0)),
                        }
                    };
                    match step {
                        Some(ObjRead::Fail(e)) => {
                            return Exec {
                                status: 1,
                                stdout: String::new(),
                                stderr: e.into(),
                            };
                        }
                        Some(ObjRead::State(Some(o))) => self.put(&ns, o),
                        Some(ObjRead::State(None)) => {
                            self.objects.borrow_mut().remove(&(
                                ns.clone(),
                                kind.to_string(),
                                wanted[0].clone(),
                            ));
                        }
                        None => {}
                    }
                }
                let items: Vec<Value> = self
                    .objects
                    .borrow()
                    .iter()
                    .filter(|((n, k, name), _)| *n == ns && k == kind && wanted.contains(&name))
                    .map(|(_, o)| o.clone())
                    .collect();
                // As real kubectl: ONE name returns the object itself (or
                // NotFound), several return a List.
                if wanted.len() == 1 {
                    return match items.into_iter().next() {
                        Some(o) => ok(o.to_string()),
                        None => Exec {
                            status: 1,
                            stdout: String::new(),
                            stderr: format!(
                                "Error from server (NotFound): {} \"{}\" not found",
                                argv[6], wanted[0]
                            ),
                        },
                    };
                }
                ok(json!({"kind": "List", "items": items}).to_string())
            }
            "get" => {
                let kinds: Vec<&str> = argv[6].split(',').map(kind_of).collect();
                let sel = argv
                    .iter()
                    .position(|a| a == "-l")
                    .map(|i| argv[i + 1].clone());
                let items: Vec<Value> = self
                    .objects
                    .borrow()
                    .iter()
                    .filter(|((n, k, _), _)| *n == ns && kinds.contains(&k.as_str()))
                    .filter(|(_, o)| {
                        sel.as_ref().is_none_or(|s| {
                            let (k, v) = s.split_once('=').unwrap();
                            o["metadata"]["labels"][k].as_str() == Some(v)
                        })
                    })
                    .map(|(_, o)| o.clone())
                    .collect();
                ok(json!({"kind": "List", "items": items}).to_string())
            }
            "annotate" => {
                let kind = kind_of(&argv[7]);
                let name = &argv[8];
                let kv = &argv[9];
                let mut objs = self.objects.borrow_mut();
                let o = objs.get_mut(&(ns, kind.into(), name.clone())).unwrap();
                if let Some(key) = kv.strip_suffix('-') {
                    if let Some(a) = o["metadata"]["annotations"].as_object_mut() {
                        a.remove(key);
                    }
                } else {
                    let (k, v) = kv.split_once('=').unwrap();
                    merge(&mut o["metadata"], &json!({"annotations": {k: v}}));
                }
                ok(String::new())
            }
            "patch" if argv[8] != "--subresource" => {
                let kind = kind_of(&argv[6]);
                let name = &argv[7];
                let patch: Value = serde_json::from_str(&argv[11]).unwrap();
                let mut objs = self.objects.borrow_mut();
                let o = objs.get_mut(&(ns, kind.into(), name.clone())).unwrap();
                merge(o, &patch);
                if let Some(g) = o["metadata"]["generation"].as_i64() {
                    o["metadata"]["generation"] = json!(g + 1);
                }
                ok(String::new())
            }
            "exec" if argv.iter().any(|a| a.contains("total_run_queue_lengths")) => {
                // The fake broker's run queue is high only while its cpu is
                // starved: the peak exists ONLY during the load.
                let objs = self.objects.borrow();
                let starved = objs
                    .get(&(ns.clone(), "Pod".into(), "broker-0".into()))
                    .is_some_and(|p| {
                        p["spec"]["containers"][0]["resources"]["limits"]["cpu"] == "250m"
                    });
                // A phase marked "quiet" in svc-conf never saturates: how a
                // test makes phase B's load fail to bite.
                let quiet = objs
                    .get(&(ns.clone(), "ConfigMap".into(), "svc-conf".into()))
                    .is_some_and(|c| c["data"]["phase"] == "quiet");
                let starved = starved && !quiet;
                ok(if starved { "9\n" } else { "0\n" }.into())
            }
            "exec" if self.exec_fails.borrow().iter().any(|f| argv.contains(f)) => Exec {
                status: 1,
                stdout: String::new(),
                stderr: "command terminated with exit code 1".into(),
            },
            "exec" => {
                let mut script = self.exec_script.borrow_mut();
                if script.is_empty() {
                    ok(self.rabbit_closed.borrow().clone())
                } else if script.len() > 1 {
                    ok(script.remove(0))
                } else {
                    ok(script[0].clone())
                }
            }
            "rollout" => {
                assert_eq!(argv[6], "restart");
                let mut objs = self.objects.borrow_mut();
                for name in &argv[8..] {
                    let o = objs
                        .get_mut(&(ns.clone(), "Deployment".into(), name.clone()))
                        .unwrap();
                    merge(
                        o,
                        &json!({"spec": {"template": {"metadata": {"annotations": {"kubectl.kubernetes.io/restartedAt": "now"}}}}}),
                    );
                }
                ok(String::new())
            }
            "patch" if argv[8] == "--subresource" => {
                assert_eq!(
                    (argv[9].as_str(), argv[11].as_str()),
                    ("resize", "strategic"),
                    "in-place resize, strategic"
                );
                let body: Value = serde_json::from_str(&argv[13]).unwrap();
                let c = &body["spec"]["containers"][0];
                let (cname, resources) = (
                    c["name"].as_str().unwrap().to_string(),
                    c["resources"].clone(),
                );
                let mut objs = self.objects.borrow_mut();
                let pod = objs
                    .get_mut(&(ns.clone(), "Pod".into(), argv[7].clone()))
                    .unwrap();
                for (path, applies) in [
                    ("/spec/containers", true),
                    ("/status/containerStatuses", *self.kubelet_applies.borrow()),
                ] {
                    if !applies {
                        continue;
                    }
                    if let Some(list) = pod.pointer_mut(path).and_then(Value::as_array_mut) {
                        for ct in list.iter_mut().filter(|ct| ct["name"] == cname.as_str()) {
                            merge(&mut ct["resources"], &resources);
                        }
                    }
                }
                if *self.starve_on_resize.borrow()
                    && self
                        .starve_only
                        .borrow()
                        .as_ref()
                        .is_none_or(|p| p == &argv[7])
                {
                    pod["status"]["conditions"] = json!([{"type": "Ready", "status": "False"}]);
                }
                ok(String::new())
            }
            "logs" => match self.pod_logs.borrow().get(&argv[6]) {
                Some(t) => ok(t.clone()),
                None => Exec {
                    status: 1,
                    stdout: String::new(),
                    stderr: format!(
                        "Error from server (NotFound): pods \"{}\" not found",
                        argv[6]
                    ),
                },
            },
            other => panic!("fake cluster: verb {other} is not one a visita may use"),
        }
    }
}

fn seeded() -> FakeCluster {
    let c = FakeCluster::default();
    c.put(
        "stage",
        json!({"kind": "Secret", "metadata": {"name": "stage-cluster", "labels": {"project": "stage"},
               "annotations": {"crossplane": "keep-me",
                               "kubectl.kubernetes.io/last-applied-configuration": "{\"stringData\":{\"config\":\"NOT-A-REAL-TOKEN\"}}"},
               "resourceVersion": "1", "uid": "u1", "managedFields": [{"x": 1}]},
               "data": {"config": "Tk9ULUEtUkVBTC1UT0tFTg=="}}),
    );
    c.put(
        "stage",
        json!({"kind": "ConfigMap", "metadata": {"name": "svc-conf", "resourceVersion": "3"},
               "data": {"pool_size": "4"}}),
    );
    c.put(
        "stage",
        json!({"kind": "Deployment", "metadata": {"name": "svc", "generation": 7},
               "spec": {"replicas": 3, "strategy": {"rollingUpdate": {"maxSurge": "25%", "maxUnavailable": "25%"}},
                        "template": {"spec": {"containers": [{"name": "app", "image": "r/svc:natural"}]}}},
               "status": {"replicas": 3, "availableReplicas": 3}}),
    );
    c.put(
        "stage",
        json!({"kind": "Pod", "metadata": {"name": "svc-1", "labels": {"app": "svc"}}, "status": {"podIP": "10.1.2.3"}}),
    );
    c
}

const NATURAL: &str = r#"
  :natural-state ((:context "stage-ctx" :namespace "stage"
                   :kinds ("deployments" "configmaps")
                   :names ("svc" "svc-conf"))
                  (:context "stage-ctx" :namespace "stage"
                   :kinds ("secrets") :selector "project=stage"))"#;

const PIN: &str = r#"(:name "pin" :effect mutate :cleanup restore-from-snapshot
      :action (:annotate (:context "stage-ctx" :namespace "stage" :resource "secret"
                          :name "stage-cluster" :key "branch-pin" :value "my-branch")))"#;

const STRATEGY: &str = r#"(:name "strategy" :effect mutate :cleanup restore-from-snapshot
      :action (:patch (:context "stage-ctx" :namespace "stage" :resource "deployment" :name "svc"
                       :patch "{\"spec\":{\"strategy\":{\"rollingUpdate\":{\"maxSurge\":1,\"maxUnavailable\":0}}}}")))"#;

fn conf(cleanup: &str) -> String {
    format!(
        r#"(:name "conf" :effect mutate :cleanup {cleanup}
      :action (:patch (:context "stage-ctx" :namespace "stage" :resource "configmap" :name "svc-conf"
                       :patch "{{\"data\":{{\"pool_size\":\"8\",\"feature_flag\":\"on\"}}}}")))"#
    )
}

fn spec(fixtures: &str, steps: &str) -> VisitaSpec {
    let src = format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL}
             :fixtures ({fixtures})
             :steps ({steps}))"#
    );
    load_one::<VisitaSpec>(&src).expect("spec compiles")
}

fn allow() -> Vec<String> {
    vec!["stage-ctx".into()]
}

fn run_on(c: &FakeCluster, s: &VisitaSpec) -> (ensaio::visita::VisitaReport, tempdir::Dir) {
    s.validate(&allow()).expect("spec validates");
    let dir = tempdir::Dir::new();
    let ev = Evidence::open(dir.path()).unwrap();
    let env = OperatorEnvironment::new(Operator::new(c, &ev, None))
        .with_stop_bounds(std::time::Duration::ZERO, std::time::Duration::ZERO)
        .with_transient_bounds(
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(10),
        );
    let report = run(s, &env).expect("natural state captured");
    (report, dir)
}

mod tempdir {
    pub struct Dir(std::path::PathBuf);
    impl Dir {
        pub fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "visita-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            Self(p)
        }
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

// ── the loop ─────────────────────────────────────────────────────────────

#[test]
fn snapshot_mutate_undo_diff_is_empty_and_certifies() {
    let c = seeded();
    let s = spec(
        &format!("{PIN} {STRATEGY} {}", conf("restore-from-snapshot")),
        r#"(:place "pin") (:place "strategy") (:place "conf")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert_eq!(
        c.mutating_calls(),
        6,
        "three changes, three derived inverses"
    );
    assert!(r.restore.is_empty(), "restore diff: {:?}", r.restore);
    assert_eq!(r.restore.checked, 3);
    assert_eq!(r.receipt.verdict, CertVerdict::Certified);
    assert_eq!(
        r.undone,
        vec!["conf", "strategy", "pin"],
        "undo is reverse placement"
    );

    // The inverse restored the value that was there, not one an author chose:
    // the `crossplane` annotation someone else owns is untouched, `branch-pin` gone.
    let secret = c.get("stage", "Secret", "stage-cluster");
    assert_eq!(secret["metadata"]["annotations"]["crossplane"], "keep-me");
    assert!(
        secret["metadata"]["annotations"]
            .get("branch-pin")
            .is_none()
    );
    assert!(
        c.get("stage", "ConfigMap", "svc-conf")["data"]
            .get("feature_flag")
            .is_none()
    );
}

#[test]
fn negative_control_a_missing_inverse_leaves_a_non_empty_diff() {
    let c = seeded();
    let s = spec(
        &format!(
            r#"{PIN} {}"#,
            conf(r#"(:transient (:reason "negative control"))"#)
        ),
        r#"(:place "pin") (:place "conf")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(!r.restore.is_empty());
    assert_eq!(r.restore.differing.len(), 1);
    let paths: Vec<&str> = r.restore.differing[0]
        .changes
        .iter()
        .map(|c| c.path.as_str())
        .collect();
    assert_eq!(paths, vec!["data.feature_flag", "data.pool_size"]);
    assert_eq!(r.receipt.verdict, CertVerdict::NotCertified);
    let restored = r
        .results
        .iter()
        .find(|x| x.assertion_id == "natural-state-restored")
        .unwrap();
    assert!(restored.verdict.falsified());
}

#[test]
fn a_failed_placement_stops_the_steps_but_still_undoes_what_landed() {
    let c = seeded();
    let bad = r#"(:name "bad" :effect mutate :cleanup restore-from-snapshot
      :action (:rabbitmq-close-connections (:context "stage-ctx" :broker-namespace "rabbitmq"
               :broker-pod "rabbitmq-0" :vhost "orders" :app-namespace "stage" :app-selector "app=svc")))"#;
    // restore-from-snapshot on a fault is refused at validation; declare it
    // transient as a real spec would.
    let bad = bad.replace(
        "restore-from-snapshot",
        r#"(:transient (:reason "clients reconnect"))"#,
    );
    *c.rabbit_closed.borrow_mut() = "0".into();
    let s = spec(
        &format!("{PIN} {bad} {STRATEGY}"),
        r#"(:place "pin") (:place "bad") (:place "strategy")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted
            .as_deref()
            .unwrap_or("")
            .contains("closed 0 connections")
    );
    assert_eq!(r.placed, vec!["pin", "bad"], "strategy never placed");
    assert!(
        r.restore.is_empty(),
        "pin was still undone: {:?}",
        r.restore
    );
    // A clean restore does not make an aborted trial a pass.
    assert_eq!(
        r.receipt.verdict,
        CertVerdict::Inconclusive,
        "an aborted trial answered nothing"
    );
}

fn occupy(key: &str, value: &str) -> String {
    format!(
        r#"(:name "occupy" :effect mutate :cleanup restore-from-snapshot
      :action (:annotate (:context "stage-ctx" :namespace "stage" :resource "secret"
                          :name "stage-cluster" :key "{key}" :value "{value}" :absorb #t)))"#
    )
}

#[test]
fn an_absorbing_fixture_takes_unheld_ground_and_gives_it_back() {
    let c = seeded();
    let s = spec(&occupy("failover", "my-branch"), r#"(:place "occupy")"#);
    let (r, _d) = run_on(&c, &s);
    assert_eq!(r.placed, vec!["occupy"]);
    assert!(r.restore.is_empty(), "restore diff: {:?}", r.restore);
    assert_eq!(r.receipt.verdict, CertVerdict::Certified);
    let secret = c.get("stage", "Secret", "stage-cluster");
    assert_eq!(secret["metadata"]["annotations"]["crossplane"], "keep-me");
}

#[test]
fn an_absorbing_fixture_refuses_a_key_the_environment_holds_before_writing() {
    let c = seeded();
    let s = spec(&occupy("crossplane", "my-branch"), r#"(:place "occupy")"#);
    let (r, _d) = run_on(&c, &s);
    let why = r.aborted.as_deref().unwrap_or("");
    assert!(
        why.contains("/metadata/annotations/crossplane") && why.contains("keep-me"),
        "{why}"
    );
    assert_eq!(
        c.mutating_calls(),
        0,
        "refused before the write, so there is nothing to undo"
    );
    assert!(r.placed.is_empty());
    assert!(r.restore.is_empty(), "restore diff: {:?}", r.restore);
    assert_eq!(
        c.get("stage", "Secret", "stage-cluster")["metadata"]["annotations"]["crossplane"],
        "keep-me"
    );
    assert_eq!(r.receipt.verdict, CertVerdict::Inconclusive);
}

#[test]
fn an_absorbing_fixture_reads_the_live_object_not_the_snapshot() {
    let c = seeded();
    let pin_late = r#"(:name "late" :effect mutate :cleanup restore-from-snapshot
      :action (:annotate (:context "stage-ctx" :namespace "stage" :resource "secret"
                          :name "stage-cluster" :key "failover" :value "their-branch")))"#;
    let s = spec(
        &format!("{pin_late} {}", occupy("failover", "my-branch")),
        r#"(:place "late") (:place "occupy")"#,
    );
    let (r, _d) = run_on(&c, &s);
    let why = r.aborted.as_deref().unwrap_or("");
    assert!(why.contains("their-branch"), "{why}");
    assert_eq!(r.placed, vec!["late"]);
}

#[test]
fn an_absorbing_fixture_may_restate_a_held_value_unchanged() {
    let c = seeded();
    let s = spec(&occupy("crossplane", "keep-me"), r#"(:place "occupy")"#);
    let (r, _d) = run_on(&c, &s);
    assert_eq!(r.placed, vec!["occupy"]);
    assert_eq!(r.receipt.verdict, CertVerdict::Certified);
}

#[test]
fn a_fault_closes_the_ipv4_mapped_peers_once_and_records_the_count() {
    let c = seeded();
    *c.rabbit_closed.borrow_mut() = "3".into();
    let fault = r#"(:name "fault" :effect mutate :cleanup (:transient (:reason "clients reconnect"))
      :action (:rabbitmq-close-connections (:context "stage-ctx" :broker-namespace "rabbitmq"
               :broker-pod "rabbitmq-0" :vhost "orders" :app-namespace "stage" :app-selector "app=svc")))"#;
    let s = spec(fault, r#"(:place "fault")"#);
    let (r, _d) = run_on(&c, &s);
    assert!(r.aborted.is_none());
    let calls = c.calls.borrow();
    let exec: Vec<&Vec<String>> = calls
        .iter()
        .filter(|a| a.iter().any(|x| x == "exec"))
        .collect();
    assert_eq!(exec.len(), 1, "cluster-wide eval runs once");
    let expr = exec[0].last().unwrap();
    assert!(expr.contains("{0,0,0,0,0,65535,2561,515}"), "{expr}");
    assert!(expr.contains("<<\"orders\">>"));
}

#[test]
fn secret_values_never_reach_the_evidence() {
    let c = seeded();
    let s = spec(PIN, r#"(:place "pin")"#);
    let (_r, d) = run_on(&c, &s);
    let mut stack = vec![d.path().to_path_buf()];
    let mut files = 0;
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).unwrap() {
            let e = e.unwrap().path();
            if e.is_dir() {
                stack.push(e);
                continue;
            }
            files += 1;
            let body = std::fs::read_to_string(&e).unwrap_or_default();
            for secret in ["NOT-A-REAL-TOKEN", "Tk9ULUEtUkVBTC1UT0tFTg=="] {
                assert!(
                    !body.contains(secret),
                    "{} carries a secret value",
                    e.display()
                );
            }
        }
    }
    assert!(files > 5, "the bundle was actually written ({files} files)");
}

#[test]
fn natural_state_strips_bookkeeping_and_is_content_addressed() {
    let c = seeded();
    let s = spec(PIN, r#"(:place "pin")"#);
    let (r, d) = run_on(&c, &s);
    let n: ensaio::visita::NaturalState =
        serde_json::from_slice(&std::fs::read(d.path().join("natural-state.json")).unwrap())
            .unwrap();
    assert_eq!(n.address, r.natural_state_address);
    let secret = n.objects.values().find(|o| o.key.kind == "Secret").unwrap();
    assert!(secret.object["metadata"].get("managedFields").is_none());
    assert!(secret.object["metadata"].get("resourceVersion").is_none());
    let dep = n
        .objects
        .values()
        .find(|o| o.key.kind == "Deployment")
        .unwrap();
    assert!(dep.object.get("status").is_none());
    assert_eq!(
        dep.generation,
        Some(7),
        "rollout baseline kept beside the object"
    );
}

// ── refused before anything runs ─────────────────────────────────────────

fn refusal(fixtures: &str, extra: &str) -> String {
    let src = format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL} :fixtures ({fixtures}) {extra} :steps ())"#
    );
    let s = load_one::<VisitaSpec>(&src).unwrap();
    s.validate(&allow()).unwrap_err()
}

#[test]
fn environment_is_destroyed_is_refused_on_a_borrowed_environment() {
    let e = refusal(
        &PIN.replace("restore-from-snapshot", "environment-is-destroyed"),
        "",
    );
    assert!(e.contains("never destroyed"), "{e}");
}

#[test]
fn restore_from_snapshot_is_refused_where_no_inverse_can_be_derived() {
    let dispatch = r#"(:name "d" :effect mutate :cleanup restore-from-snapshot
      :action (:dispatch-workflow (:repo "o/r" :workflow "w.yml" :git-ref "b")))"#;
    assert!(refusal(dispatch, "").contains("cannot invert"));
}

#[test]
fn restore_from_snapshot_is_refused_on_an_object_the_snapshot_does_not_cover() {
    let e = refusal(
        &PIN.replace("stage-cluster", "someone-elses-secret")
            .replace(":namespace \"stage\"", ":namespace \"argocd\""),
        "",
    );
    assert!(e.contains("no :natural-state selection captures"), "{e}");
}

#[test]
fn a_job_creating_kind_is_refused() {
    let e = refusal(
        "",
        r#":assertions ((:id "j" :kind CustomJob :target "x" :context "stage-ctx"))"#,
    );
    assert!(e.contains("creates nothing there"), "{e}");
}

#[test]
fn an_operator_kind_missing_a_parameter_is_refused_at_load() {
    let e = refusal(
        "",
        r#":assertions ((:id "m" :kind MetricInRange :target "x" :context "stage-ctx" :selector "app=svc" :metric "m"))"#,
    );
    assert!(e.contains("missing :port"), "{e}");
    let e = refusal(
        "",
        r#":assertions ((:id "h" :kind HpaBehavior :target "x" :context "stage-ctx" :names ("h")))"#,
    );
    assert!(e.contains("missing :scale-up"), "{e}");
}

#[test]
fn an_unallowed_context_is_refused() {
    let s = spec(PIN, r#"(:place "pin")"#);
    let e = s.validate(&["other".into()]).unwrap_err();
    assert!(e.contains("--allow-context stage-ctx"), "{e}");
}

// ── judges ───────────────────────────────────────────────────────────────

fn sample(
    name: &str,
    generation: i64,
    desired: i64,
    replicas: i64,
    available: i64,
    updated: i64,
) -> RolloutSample {
    RolloutSample {
        at: format!("t{replicas}{available}"),
        name: name.into(),
        generation,
        observed_generation: generation,
        desired,
        replicas,
        updated,
        ready: available,
        available,
        unavailable: desired - available,
        image: "x".into(),
        strategy: None,
    }
}

/// A sample as the hand recorder writes it: `ts`, `gen`, the Deployment's
/// `rollingUpdate` as `strategy`, and `null` for an absent count.
fn recorded(
    ts: &str,
    generation: i64,
    desired: i64,
    replicas: i64,
    updated: i64,
    available: i64,
) -> RolloutSample {
    serde_json::from_value(json!({
        "ts": ts, "name": "api-deployment", "gen": generation, "desired": desired,
        "replicas": replicas, "updated": updated, "ready": available, "available": available,
        "unavailable": if desired > available { json!(desired - available) } else { Value::Null },
        "strategy": {"maxSurge": 1, "maxUnavailable": 0}, "image": "branch-a1b2"
    }))
    .unwrap()
}

/// The rule this judge replaced: `desired - available <= max_unavailable` per
/// sample. Kept here only to show the trace below is a case it gets wrong.
fn old_rule_flags(samples: &[RolloutSample], base: i64) -> bool {
    samples
        .iter()
        .filter(|s| s.generation > base)
        .any(|s| s.desired - s.available > 0)
}

/// A real trace, names neutralised: mid-rollout the HPA raises desired 2 -> 3
/// on the SAME template (updated == replicas == 3); available stays 2 until
/// the new pod is Ready. No pod was removed.
fn hpa_scale_up_trace() -> Vec<RolloutSample> {
    vec![
        recorded("t00:30", 57, 2, 2, 2, 2),
        recorded("t00:34", 57, 2, 2, 2, 2),
        recorded("t00:38", 57, 2, 2, 2, 2),
        recorded("t00:42", 57, 2, 2, 2, 2),
        recorded("t00:47", 58, 3, 3, 3, 2),
        recorded("t00:51", 58, 3, 3, 3, 2),
        recorded("t00:56", 58, 3, 3, 3, 2),
        recorded("t01:00", 58, 3, 3, 3, 3),
    ]
}

#[test]
fn an_hpa_scale_up_on_the_trial_strategy_is_a_scale_event_not_a_breach() {
    let trace = hpa_scale_up_trace();
    let base = BTreeMap::from([("api-deployment".to_string(), 56)]);
    assert!(
        old_rule_flags(&trace, 56),
        "the old rule misreads this trace"
    );
    let events = judge::rollout_events(&trace, &base, 1, 0, None);
    assert!(events.iter().all(|e| !e.is_breach()), "{events:?}");
    assert!(events.contains(&judge::RolloutEvent::Scale {
        name: "api-deployment".into(),
        at: "t00:47".into(),
        generation: 58,
        from: 2,
        to: 3,
    }));
    assert!(judge::rollout(&trace, &base, 1, 0, None).held());
}

#[test]
fn a_pod_removed_before_its_replacement_is_ready_is_a_breach() {
    // Same strategy, desired steady at 3: one old pod goes before the new one
    // is Ready, so available drops 3 -> 2 with nothing scaling.
    let trace = vec![
        recorded("t00:00", 57, 3, 3, 3, 3),
        recorded("t00:04", 58, 3, 4, 1, 3),
        recorded("t00:08", 58, 3, 3, 1, 2),
        recorded("t00:12", 58, 3, 4, 2, 3),
        recorded("t00:16", 58, 3, 3, 3, 3),
    ];
    let base = BTreeMap::from([("api-deployment".to_string(), 57)]);
    let events = judge::rollout_events(&trace, &base, 1, 0, None);
    let breaches: Vec<_> = events.iter().filter(|e| e.is_breach()).collect();
    assert_eq!(breaches.len(), 1, "{events:?}");
    assert!(matches!(
        breaches[0],
        judge::RolloutEvent::Unavailable {
            available: 2,
            previous_available: 3,
            ..
        }
    ));
    assert!(judge::rollout(&trace, &base, 1, 0, None).falsified());
}

#[test]
fn a_generation_on_another_strategy_is_reported_not_judged() {
    let mut s = recorded("t00:04", 58, 3, 3, 1, 2);
    s.strategy = Some(json!({"maxSurge": "25%", "maxUnavailable": "25%"}));
    let trace = vec![recorded("t00:00", 57, 3, 3, 3, 3), s];
    let base = BTreeMap::from([("api-deployment".to_string(), 57)]);
    let events = judge::rollout_events(&trace, &base, 1, 0, None);
    assert!(
        matches!(
            events.as_slice(),
            [judge::RolloutEvent::OtherStrategy { generation: 58, .. }]
        ),
        "{events:?}"
    );
    assert!(matches!(
        judge::rollout(&trace, &base, 1, 0, None),
        Verdict::Vacuous {}
    ));
}

#[test]
fn rollout_within_one_surge_and_none_unavailable_holds() {
    let base = BTreeMap::from([("svc".to_string(), 7)]);
    let s = vec![
        sample("svc", 7, 3, 3, 3, 3),
        sample("svc", 8, 3, 4, 3, 1),
        sample("svc", 8, 3, 4, 3, 2),
        sample("svc", 8, 3, 3, 3, 3),
    ];
    assert!(judge::rollout(&s, &base, 1, 0, None).held());
}

#[test]
fn rollout_surging_two_or_dropping_one_is_falsified() {
    let base = BTreeMap::from([("svc".to_string(), 7)]);
    let surge = vec![sample("svc", 8, 3, 5, 3, 2), sample("svc", 8, 3, 3, 3, 3)];
    assert!(judge::rollout(&surge, &base, 1, 0, None).falsified());
    let dip = vec![
        sample("svc", 7, 3, 3, 3, 3),
        sample("svc", 8, 3, 3, 2, 2),
        sample("svc", 8, 3, 3, 3, 3),
    ];
    assert!(judge::rollout(&dip, &base, 1, 0, None).falsified());
    let unfinished = vec![sample("svc", 8, 3, 4, 3, 1)];
    assert!(judge::rollout(&unfinished, &base, 1, 0, None).falsified());
}

#[test]
fn an_hpa_scale_up_mid_rollout_is_not_rollout_unavailability() {
    // desired 3 -> 4 while rolling; the 4th pod warms up. Nothing was taken
    // down, so a maxUnavailable-0 rollout holds.
    let base = BTreeMap::from([("svc".to_string(), 7)]);
    let s = vec![
        sample("svc", 7, 3, 3, 3, 3),
        sample("svc", 8, 3, 4, 3, 1),
        sample("svc", 8, 4, 4, 3, 2),
        sample("svc", 8, 4, 5, 4, 3),
        sample("svc", 8, 4, 4, 4, 4),
    ];
    assert!(judge::rollout(&s, &base, 1, 0, None).held());
    // …but a pod lost after the scale-up is still caught.
    let mut lost = s.clone();
    lost.insert(4, sample("svc", 8, 4, 4, 3, 3));
    assert!(judge::rollout(&lost, &base, 1, 0, None).falsified());
}

#[test]
fn no_new_generation_is_vacuous_not_a_pass() {
    let base = BTreeMap::from([("svc".to_string(), 8)]);
    let s = vec![sample("svc", 8, 3, 3, 3, 3)];
    assert!(matches!(
        judge::rollout(&s, &base, 1, 0, None),
        Verdict::Vacuous {}
    ));
}

#[test]
fn a_hand_recorded_rollout_line_parses() {
    let line = r#"{"ts": "2026-01-01T00:00:00+00:00", "name": "svc", "gen": 8653, "desired": 3, "replicas": 3, "updated": 3, "ready": 3, "available": 3, "unavailable": null, "strategy": {"maxSurge": "25%"}, "image": "abc"}"#;
    let s: RolloutSample = serde_json::from_str(line).unwrap();
    assert_eq!((s.generation, s.unavailable), (8653, 0));
    assert!(s.complete());
}

#[test]
fn logs_absent_present_silent_vanished_and_failed() {
    use judge::PodRead::{Failed, Text, Vanished};
    let reads = vec![
        ("a".to_string(), Text("ok\nconnection reset\n".into())),
        ("b".to_string(), Text("pool ready: 8\n".into())),
    ];
    let pats = vec!["connection reset".to_string()];
    assert!(judge::logs(&reads, &pats, Presence::Absent, 1, false).falsified());
    assert!(judge::logs(&reads, &["pool ready:".into()], Presence::Present, 1, false).held());
    assert!(judge::logs(&reads, &["init failed".into()], Presence::Absent, 1, false).held());
    let silent = vec![("a".to_string(), Text(String::new()))];
    assert!(matches!(
        judge::logs(&silent, &pats, Presence::Absent, 1, false),
        Verdict::Vacuous {}
    ));
    // A pod that vanished mid-read is skipped and counted — not an error.
    let mut with_gone = reads.clone();
    with_gone.push(("c".to_string(), Vanished));
    let v = judge::logs(
        &with_gone,
        &["init failed".into()],
        Presence::Absent,
        1,
        false,
    );
    assert!(v.held(), "{v:?}");
    assert!(
        v.subjects()
            .unwrap()
            .iter()
            .any(|o| o.as_str().contains("vanished mid-read: c"))
    );
    let all_failed = vec![("a".to_string(), Failed("forbidden".into()))];
    assert!(matches!(
        judge::logs(&all_failed, &pats, Presence::Absent, 1, false),
        Verdict::Errored { .. }
    ));
}

#[test]
fn metrics_present_in_range_absent_and_unreadable() {
    let body = "# HELP pool_ready_workers x\npool_ready_workers{vhost=\"t\"} 8\npool_ready_workers_total 1\n";
    let ok = vec![("p1".to_string(), judge::PodRead::Text(body.to_string()))];
    assert!(judge::metrics(&ok, "pool_ready_workers", Some(1.0), None).held());
    assert!(judge::metrics(&ok, "pool_ready_workers", Some(9.0), None).falsified());
    assert!(judge::metrics(&ok, "absent_metric", None, None).falsified());
    let down = vec![(
        "p1".to_string(),
        judge::PodRead::Failed("connection refused".to_string()),
    )];
    let gone = vec![("p1".to_string(), judge::PodRead::Vanished)];
    assert!(
        matches!(judge::metrics(&gone, "m", None, None), Verdict::Vacuous {}),
        "vanished is not errored"
    );
    assert!(matches!(
        judge::metrics(&down, "m", None, None),
        Verdict::Errored { .. }
    ));
    assert_eq!(
        judge::metric_samples(body, "pool_ready_workers").len(),
        1,
        "a longer name is not a sample"
    );
}

#[test]
fn hpa_policy_found_and_missing() {
    let items = vec![
        json!({"metadata": {"name": "h1"}, "spec": {"behavior": {"scaleUp": {"policies": [{"type": "Pods", "value": 5, "periodSeconds": 60}]}}}}),
        json!({"metadata": {"name": "h2"}, "spec": {}}),
    ];
    let p = ensaio::HpaPolicy {
        kind: "Pods".into(),
        value: 5,
        period_seconds: 60,
    };
    assert!(judge::hpa(&items, &["h1".into()], &p).held());
    assert!(judge::hpa(&items, &["h1".into(), "h2".into()], &p).falsified());
}

#[test]
fn ipv4_mapped_tuple_matches_what_rabbitmq_reports() {
    assert_eq!(
        judge::ipv4_mapped_tuple("10.1.2.3").unwrap(),
        "{0,0,0,0,0,65535,2561,515}"
    );
    assert!(judge::ipv4_mapped_tuple("fe80::1").is_err());
}

// ── the committed specs compile and validate ──────────────────────────────

#[test]
fn committed_visita_specs_validate() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../specs");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.to_string_lossy().ends_with(".visita.lisp") {
            let s = load_one::<VisitaSpec>(&std::fs::read_to_string(&p).unwrap()).unwrap();
            if s.target.is_some() {
                continue;
            }
            let ctx: Vec<String> = s.contexts().into_iter().collect();
            s.validate(&ctx)
                .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            n += 1;
        }
    }
    assert!(n >= 2, "found {n} visita specs");
}

// ── a dispatched pipeline: awaited, and stopped before anything is undone ──

fn view(status: &str, jobs: Value) -> Value {
    json!({"status": status, "conclusion": if status == "completed" { json!("success") } else { Value::Null }, "jobs": jobs})
}

fn deploy_job(sync: (&str, &str), validate: (&str, &str)) -> Value {
    json!([{"name": "build / deploy-environments", "status": "in_progress", "conclusion": null, "steps": [
        {"name": "argocd sync", "status": sync.0, "conclusion": sync.1},
        {"name": "validate_deployed_version", "status": validate.0, "conclusion": validate.1}
    ]}])
}

fn dispatch(cleanup: &str, extra: &str) -> String {
    format!(
        r#"(:name "deploy" :effect mutate :cleanup {cleanup} :after ("pin")
      :action (:dispatch-workflow (:repo "example-org/example-repo" :workflow "deploy.yml" :git-ref "my-branch"
               :await (:job "deploy-environments" :step "argocd sync")
               :accept-failure ("validate_deployed_version")
               :poll "0s" :timeout "5s" {extra})))"#
    )
}

fn calls_with(c: &FakeCluster, word: &str) -> Vec<usize> {
    c.calls
        .borrow()
        .iter()
        .enumerate()
        .filter(|(_, a)| a.join(" ").contains(word))
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn a_dispatch_is_awaited_until_the_declared_step_and_accepts_a_declared_failure() {
    let c = seeded();
    *c.run_views.borrow_mut() = vec![
        view("queued", json!([])),
        view(
            "in_progress",
            deploy_job(("in_progress", ""), ("queued", "")),
        ),
        view(
            "in_progress",
            deploy_job(("completed", "success"), ("completed", "failure")),
        ),
    ];
    *c.cancel_works.borrow_mut() = true;
    let s = spec(
        &format!("{PIN} {}", dispatch("stop", "")),
        r#"(:place "pin") (:place "deploy")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    // Three views to reach the step: placement did not return at dispatch.
    let views_before_undo = calls_with(&c, "run view")
        .into_iter()
        .filter(|i| {
            *i < calls_with(&c, "run cancel")
                .first()
                .copied()
                .unwrap_or(usize::MAX)
        })
        .count();
    assert!(
        views_before_undo >= 3,
        "awaited through {views_before_undo} views"
    );
}

#[test]
fn an_unaccepted_failing_step_aborts_the_placement() {
    let c = seeded();
    *c.run_views.borrow_mut() = vec![view(
        "in_progress",
        json!([{"name": "build / deploy-environments", "status": "in_progress", "conclusion": null,
                "steps": [{"name": "helm lint", "status": "completed", "conclusion": "failure"}]}]),
    )];
    *c.cancel_works.borrow_mut() = true;
    let s = spec(
        &format!("{PIN} {}", dispatch("stop", "")),
        r#"(:place "pin") (:place "deploy")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("helm lint"),
        "{:?}",
        r.aborted
    );
    assert!(
        r.restore.is_empty(),
        "the pin was still undone: {:?}",
        r.restore
    );
}

#[test]
fn a_dispatch_whose_run_cannot_be_tracked_is_refused() {
    let c = seeded();
    *c.dispatch_out.borrow_mut() = Some("Created workflow_dispatch event\n".into());
    *c.run_views.borrow_mut() = vec![view("completed", json!([]))];
    let s = spec(
        &format!("{PIN} {}", dispatch("stop", "")),
        r#"(:place "pin") (:place "deploy")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("no run URL"),
        "{:?}",
        r.aborted
    );
}

#[test]
fn a_run_in_flight_is_cancelled_before_the_pin_is_undone() {
    let c = seeded();
    *c.run_views.borrow_mut() = vec![view(
        "in_progress",
        deploy_job(("completed", "success"), ("in_progress", "")),
    )];
    *c.cancel_works.borrow_mut() = true;
    // Pin placed LAST, so reverse-order undo alone would unpin first: only
    // the stop-every-run-first phase puts the cancel ahead of the unpin.
    let s = spec(
        &format!("{PIN} {}", dispatch("stop", "")),
        r#"(:place "deploy") (:place "pin")"#,
    );
    let (r, _d) = run_on(&c, &s);
    let cancel = calls_with(&c, "run cancel");
    let unpin = calls_with(&c, "branch-pin-");
    assert_eq!(cancel.len(), 1, "cancelled once");
    assert!(
        cancel[0] < unpin[0],
        "cancel at call {} must precede unpin at call {}",
        cancel[0],
        unpin[0]
    );
    assert!(r.restore.is_empty());
}

#[test]
fn a_run_that_will_not_stop_holds_back_every_undo() {
    let c = seeded();
    *c.run_views.borrow_mut() = vec![view(
        "in_progress",
        deploy_job(("completed", "success"), ("in_progress", "")),
    )];
    *c.cancel_works.borrow_mut() = false;
    let s = spec(
        &format!("{PIN} {}", dispatch("stop", "")),
        r#"(:place "pin") (:place "deploy")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        calls_with(&c, "branch-pin-").is_empty(),
        "never unpin while a run is in flight"
    );
    assert!(
        r.undo_errors.iter().any(|e| e.contains("still in flight")),
        "{:?}",
        r.undo_errors
    );
    assert!(
        !r.restore.is_empty(),
        "the pin is still there, and the diff says so"
    );
    assert_eq!(r.receipt.verdict, CertVerdict::NotCertified);
}

#[test]
fn a_dispatch_without_stop_is_refused() {
    let e = refusal(
        &format!("{PIN} {}", dispatch(r#"(:transient (:reason "x"))"#, "")),
        "",
    );
    assert!(e.contains("must declare :cleanup stop"), "{e}");
}

// ── observing the build under test, not the one before it ─────────────────

fn imaged(
    generation: i64,
    desired: i64,
    available: i64,
    updated: i64,
    image: &str,
) -> RolloutSample {
    let mut s = sample("svc", generation, desired, desired, available, updated);
    s.image = image.into();
    s
}

#[test]
fn a_new_generation_of_the_old_build_does_not_settle_the_watch() {
    let base = BTreeMap::from([("svc".to_string(), 7)]);
    let names = vec!["svc".to_string()];
    let old_restart = vec![imaged(7, 3, 3, 3, "old-1"), imaged(8, 3, 3, 3, "old-1")];
    assert!(!judge::rollout_settled(
        &old_restart,
        &names,
        &base,
        Some("new-")
    ));
    assert!(matches!(
        judge::rollout(&old_restart, &base, 1, 0, Some("new-")),
        Verdict::Vacuous {}
    ));
    let mut deployed = old_restart.clone();
    deployed.push(imaged(9, 3, 3, 1, "new-2"));
    deployed.push(imaged(9, 3, 3, 3, "new-2"));
    assert!(judge::rollout_settled(
        &deployed,
        &names,
        &base,
        Some("new-")
    ));
    let events = judge::rollout_events(&deployed, &base, 1, 0, Some("new-"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, judge::RolloutEvent::OtherImage { generation: 8, .. }))
    );
    assert!(judge::rollout(&deployed, &base, 1, 0, Some("new-")).held());
}

fn pod(name: &str, tag: &str) -> Value {
    json!({"kind": "Pod", "metadata": {"name": name, "labels": {"app": "svc"}},
           "spec": {"containers": [{"name": "app", "image": format!("r/svc:{tag}")}]},
           "status": {"phase": "Running", "podIP": "10.1.2.3"}})
}

#[test]
fn pod_observes_read_only_the_expected_image_and_skip_a_vanished_pod() {
    let c = seeded();
    c.put("stage", pod("svc-old", "old-1"));
    c.put("stage", pod("svc-new", "new-2"));
    c.put("stage", pod("svc-gone", "new-2"));
    c.pod_logs
        .borrow_mut()
        .insert("svc-old".into(), "connection reset\n".into());
    c.pod_logs
        .borrow_mut()
        .insert("svc-new".into(), "pool ready: 8\n".into());
    let src = format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL}
             :assertions ((:id "clean" :kind LogWindow :target "svc" :context "stage-ctx"
                           :target-namespace "stage" :selector "app=svc"
                           :patterns ("connection reset") :presence absent :expect-image "new-"))
             :steps ((:observe "clean")))"#
    );
    let s = load_one::<VisitaSpec>(&src).unwrap();
    let (r, _d) = run_on(&c, &s);
    let v = &r
        .results
        .iter()
        .find(|x| x.assertion_id == "clean")
        .unwrap()
        .verdict;
    assert!(
        v.held(),
        "old build's line must not count; vanished pod must not error: {v:?}"
    );
    assert!(
        calls_with(&c, "logs svc-old").is_empty(),
        "the old build's pod is not read"
    );
    assert!(
        v.subjects()
            .unwrap()
            .iter()
            .any(|o| o.as_str().contains("vanished mid-read: svc-gone"))
    );
}

// ── a rollout is judged over what was watched in motion ────────────────────

#[test]
fn a_rollout_seen_only_at_rest_is_not_held() {
    let base = BTreeMap::from([("svc".to_string(), 7)]);
    let at_rest = vec![imaged(8, 3, 3, 3, "new-2"), imaged(8, 3, 3, 3, "new-2")];
    let v = judge::rollout(&at_rest, &base, 1, 0, Some("new-"));
    assert!(
        matches!(v, Verdict::Skipped { .. }),
        "a finished state alone must not pass: {v:?}"
    );
    let events = judge::rollout_events(&at_rest, &base, 1, 0, Some("new-"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, judge::RolloutEvent::Unobserved { .. })),
        "{events:?}"
    );
    // …while a breach still wins over "unobserved elsewhere".
    let mut breached = at_rest.clone();
    breached.insert(0, imaged(8, 3, 2, 1, "new-2"));
    breached.insert(0, imaged(7, 3, 3, 3, "new-2"));
    assert!(judge::rollout(&breached, &base, 1, 0, Some("new-")).falsified());
}

fn rollout_step(generation: i64, replicas: i64, updated: i64, available: i64) -> Value {
    json!({"metadata": {"generation": generation},
           "spec": {"strategy": {"rollingUpdate": {"maxSurge": 1, "maxUnavailable": 0}},
                    "template": {"spec": {"containers": [{"name": "app", "image": "r/svc:new-2"}]}}},
           "status": {"observedGeneration": generation, "replicas": replicas, "updatedReplicas": updated,
                      "readyReplicas": available, "availableReplicas": available}})
}

#[test]
fn the_record_starts_at_record_from_and_samples_through_the_awaited_dispatch() {
    let c = seeded();
    *c.deploy_script.borrow_mut() = vec![
        rollout_step(8, 4, 1, 3),
        rollout_step(8, 4, 2, 3),
        rollout_step(8, 3, 3, 3),
    ];
    *c.run_views.borrow_mut() = vec![
        view(
            "in_progress",
            deploy_job(("in_progress", ""), ("queued", "")),
        ),
        view(
            "in_progress",
            deploy_job(("in_progress", ""), ("queued", "")),
        ),
        view(
            "in_progress",
            deploy_job(("completed", "success"), ("in_progress", "")),
        ),
    ];
    *c.cancel_works.borrow_mut() = true;
    let src = format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL}
             :fixtures ({PIN} {})
             :assertions ((:id "roll" :kind RolloutSurgeBound :target "svc" :context "stage-ctx"
                           :target-namespace "stage" :names ("svc") :max-surge 1 :max-unavailable 0
                           :window "2s" :interval "0s" :expect-image "new-" :record-from "deploy"))
             :steps ((:place "pin") (:place "deploy") (:observe "roll")))"#,
        dispatch("stop", "")
    );
    let s = load_one::<VisitaSpec>(&src).unwrap();
    let (r, d) = run_on(&c, &s);
    let v = &r
        .results
        .iter()
        .find(|x| x.assertion_id == "roll")
        .unwrap()
        .verdict;
    assert!(
        v.held(),
        "the transition was recorded during the await: {v:?}"
    );
    assert!(
        v.subjects()
            .unwrap()
            .iter()
            .any(|o| o.as_str().contains("2 mid-rollout")),
        "{v:?}"
    );
    // The sampler reads the named Deployments, never the whole namespace.
    assert!(
        calls_with(&c, "get deployment svc -o json").len() >= 4,
        "named reads"
    );
    let jsonl = std::fs::read_to_string(d.path().join("rollout-roll.jsonl")).unwrap();
    assert!(
        jsonl.lines().count() >= 4,
        "every sample written as taken: {jsonl}"
    );
    let summary = std::fs::read_to_string(d.path().join("summary.md")).unwrap();
    assert!(
        summary.contains("roll — what was watched") && summary.contains("mid-rollout"),
        "{summary}"
    );
}

#[test]
fn record_from_must_be_placed_before_its_observe() {
    let src = format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL}
             :fixtures ({PIN})
             :assertions ((:id "roll" :kind RolloutSurgeBound :target "svc" :context "stage-ctx"
                           :names ("svc") :max-surge 1 :max-unavailable 0 :record-from "pin"))
             :steps ((:observe "roll") (:place "pin")))"#
    );
    let e = load_one::<VisitaSpec>(&src)
        .unwrap()
        .validate(&allow())
        .unwrap_err();
    assert!(e.contains("must be placed before"), "{e}");
}

// ── load: an in-place resize, a restart storm, a guard, a broker metric ────

fn broker(policy: Option<&str>) -> Value {
    broker_named("broker-0", policy)
}

fn broker_named(name: &str, policy: Option<&str>) -> Value {
    let mut c = json!({"name": "broker", "image": "r/broker:1",
                       "resources": {"limits": {"cpu": "2"}, "requests": {"cpu": "300m"}}});
    if let Some(p) = policy {
        c["resizePolicy"] = json!([{"resourceName": "cpu", "restartPolicy": p}]);
    }
    json!({"kind": "Pod", "metadata": {"name": name, "labels": {"app": "broker"}},
           "spec": {"containers": [c]},
           "status": {"phase": "Running", "qosClass": "Burstable",
                      "conditions": [{"type": "Ready", "status": "True"}],
                      "containerStatuses": [{"name": "broker", "restartCount": 0,
                                             "resources": {"limits": {"cpu": "2"}, "requests": {"cpu": "300m"}}}]}})
}

fn with_broker(policy: Option<&str>) -> FakeCluster {
    let c = seeded();
    c.put("stage", broker(policy));
    *c.kubelet_applies.borrow_mut() = true;
    c
}

const BROKER_STATE: &str = r#"
  :natural-state ((:context "stage-ctx" :namespace "stage"
                   :kinds ("deployments" "configmaps") :names ("svc" "svc-conf"))
                  (:context "stage-ctx" :namespace "stage" :kinds ("pods") :names ("broker-0")))"#;

const STARVE: &str = r#"(:name "starve" :effect mutate :cleanup restore-from-snapshot
      :action (:resize-pod (:context "stage-ctx" :namespace "stage" :pod "broker-0"
                            :container "broker" :cpu-limit "250m" :timeout "1s")))"#;

fn load_spec(fixtures: &str, extra: &str, steps: &str) -> VisitaSpec {
    load_one::<VisitaSpec>(&format!(
        r#"(defvisita :name "t" :claim "c" {BROKER_STATE} :fixtures ({fixtures}) {extra} :steps ({steps}))"#
    ))
    .expect("spec compiles")
}

fn broker_cpu(c: &FakeCluster, side: &str) -> Value {
    c.get("stage", "Pod", "broker-0")["spec"]["containers"][0]["resources"][side]["cpu"].clone()
}

#[test]
fn an_in_place_resize_starves_the_pod_and_the_snapshot_puts_it_back() {
    let c = with_broker(Some("NotRequired"));
    let s = load_spec(STARVE, "", r#"(:place "starve")"#);
    let (r, _d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    let resizes = calls_with(&c, "--subresource resize");
    assert_eq!(resizes.len(), 2, "one resize down, one back");
    // Burstable, request 300m above the new 250m limit: clamped (breathe's rule).
    let down = &c.calls.borrow()[resizes[0]];
    assert!(
        down.last().unwrap().contains(r#""limits":{"cpu":"250m"}"#),
        "{down:?}"
    );
    assert!(
        down.last()
            .unwrap()
            .contains(r#""requests":{"cpu":"250m"}"#),
        "{down:?}"
    );
    assert_eq!(
        (broker_cpu(&c, "limits"), broker_cpu(&c, "requests")),
        (json!("2"), json!("300m"))
    );
    assert!(r.restore.is_empty(), "{:?}", r.restore);
    assert_eq!(r.receipt.verdict, CertVerdict::Certified);
}

#[test]
fn a_resize_that_would_restart_the_container_is_refused_before_any_write() {
    let c = with_broker(Some("RestartContainer"));
    let s = load_spec(STARVE, "", r#"(:place "starve")"#);
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("would RESTART"),
        "{:?}",
        r.aborted
    );
    // The undo of a refused placement is a resize back to the captured value —
    // a no-op on an unchanged pod. The refused resize itself never went out.
    assert!(
        calls_with(&c, "250m")
            .iter()
            .all(|i| !c.calls.borrow()[*i].join(" ").contains("--subresource"))
    );
}

#[test]
fn an_unspecified_resize_policy_relies_on_the_api_default_and_says_so() {
    // k8s.io/api core/v1: "If not specified, it defaults to NotRequired".
    let c = with_broker(None);
    let s = load_spec(STARVE, "", r#"(:place "starve")"#);
    let (r, d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    let index = std::fs::read_to_string(d.path().join("steps.jsonl")).unwrap();
    assert!(
        index.contains("API default NotRequired"),
        "the reliance is on record"
    );
}

#[test]
fn a_resize_the_kubelet_never_applies_aborts_and_is_still_put_back() {
    let c = with_broker(Some("NotRequired"));
    *c.kubelet_applies.borrow_mut() = false;
    let s = load_spec(STARVE, "", r#"(:place "starve")"#);
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("not in effect"),
        "{:?}",
        r.aborted
    );
    assert_eq!(
        broker_cpu(&c, "limits"),
        json!("2"),
        "spec put back from the snapshot"
    );
}

const GUARD: &str = r#":guard (:context "stage-ctx" :namespace "stage" :pods ("broker-0") :max-unready "0s" :interval "0s")"#;

#[test]
fn the_guard_trips_on_an_unready_pod_undoes_the_resize_first_and_ends_inconclusive() {
    let c = with_broker(Some("NotRequired"));
    *c.starve_on_resize.borrow_mut() = true;
    let fixtures = format!(
        r#"{STARVE} (:name "conf" :effect mutate :cleanup restore-from-snapshot
            :action (:patch (:context "stage-ctx" :namespace "stage" :resource "configmap" :name "svc-conf"
                             :patch "{{\"data\":{{\"pool_size\":\"9\"}}}}")))"#
    );
    // conf placed AFTER the resize, so plain reverse order would undo conf
    // first; the guard must put the broker back before anything else.
    let s = load_spec(
        &fixtures,
        GUARD,
        r#"(:place "starve") (:place "conf") (:wait "5s")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("not Ready"),
        "{:?}",
        r.aborted
    );
    let resizes = calls_with(&c, "--subresource resize");
    let conf_undo = calls_with(&c, r#""pool_size":"4""#);
    assert!(
        resizes.len() >= 2 && (conf_undo.is_empty() || resizes[1] < conf_undo[0]),
        "resize back first: {resizes:?} {conf_undo:?}"
    );
    assert_eq!(broker_cpu(&c, "limits"), json!("2"));
    let g = r
        .results
        .iter()
        .find(|x| x.assertion_id == "guard")
        .unwrap();
    assert!(
        g.verdict.falsified(),
        "a trip is witnessed evidence, not a skip: {:?}",
        g.verdict
    );
    assert_eq!(r.receipt.verdict, CertVerdict::Inconclusive);
}

#[test]
fn a_trip_is_reported_as_a_violation_naming_the_pod_the_signal_and_the_time() {
    let c = with_live_broker();
    *c.events.borrow_mut() = vec![
        events(&[]),
        events(&[liveness_event("e1", "broker-0", 4, "2026-10-01T00:02:00Z")]),
    ];
    let (r, d) = run_on(
        &c,
        &load_spec(STARVE, SLOW_GUARD, r#"(:place "starve") (:wait "5s")"#),
    );
    let g = r
        .results
        .iter()
        .find(|x| x.assertion_id == "guard")
        .unwrap();
    let Verdict::Falsified { findings, .. } = &g.verdict else {
        panic!("{:?}", g.verdict)
    };
    let finding = findings.iter().next().unwrap().as_str().to_string();
    assert!(
        finding.contains("`broker-0`") && finding.contains("liveness probe 4/6"),
        "{finding}"
    );
    assert!(
        finding.contains(" at 20"),
        "the time of the trip: {finding}"
    );
    let summary = std::fs::read_to_string(d.path().join("summary.md")).unwrap();
    let row = summary
        .lines()
        .find(|l| l.starts_with("| guard |"))
        .unwrap();
    assert!(!row.contains("skipped") && row.contains("tripped"), "{row}");
    assert!(
        summary.contains("guard tripped:") && summary.contains("broker-0"),
        "{summary}"
    );
}

#[test]
fn on_a_trip_the_tripped_pods_fixtures_are_undone_before_every_other() {
    let c = with_broker(Some("NotRequired"));
    c.put("stage", broker_named("broker-1", Some("NotRequired")));
    *c.starve_on_resize.borrow_mut() = true;
    *c.starve_only.borrow_mut() = Some("broker-0".into());
    let starve = |n: &str| {
        STARVE
            .replace("\"starve\"", &format!("\"starve-{n}\""))
            .replace("broker-0", &format!("broker-{n}"))
    };
    let s = load_one::<VisitaSpec>(&format!(
        r#"(defvisita :name "t" :claim "c"
             :natural-state ((:context "stage-ctx" :namespace "stage" :kinds ("pods") :names ("broker-0" "broker-1")))
             :fixtures ({} {})
             :guard (:context "stage-ctx" :namespace "stage" :pods ("broker-0" "broker-1") :max-unready "0s" :interval "0s")
             :steps ((:place "starve-0") (:place "starve-1") (:wait "5s")))"#,
        starve("0"),
        starve("1")
    ))
    .unwrap();
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted
            .as_deref()
            .unwrap_or("")
            .contains("`broker-0` not Ready"),
        "{:?}",
        r.aborted
    );
    let back: Vec<String> = calls_with(&c, "--subresource resize")
        .into_iter()
        .map(|i| c.calls.borrow()[i].clone())
        .filter(|a| a.last().unwrap().contains(r#""limits":{"cpu":"2"}"#))
        .map(|a| a[7].clone())
        .collect();
    assert_eq!(
        back,
        vec!["broker-0", "broker-1"],
        "the tripped pod gets its cpu back first, though it was placed first"
    );
    assert_eq!(r.undone, vec!["starve-0", "starve-1"]);
    assert!(r.restore.is_empty(), "{:?}", r.restore);
}

#[test]
fn the_guard_trips_on_a_restart() {
    let c = with_broker(Some("NotRequired"));
    let s = load_spec(STARVE, GUARD, r#"(:place "starve") (:wait "5s")"#);
    c.objects
        .borrow_mut()
        .get_mut(&("stage".into(), "Pod".into(), "broker-0".into()))
        .unwrap()["status"]["containerStatuses"][0]["restartCount"] = json!(0);
    // A restart lands after the guard's baseline: bump it via the exec hook.
    let dir = tempdir::Dir::new();
    let ev = Evidence::open(dir.path()).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None))
        .with_stop_bounds(std::time::Duration::ZERO, std::time::Duration::ZERO);
    s.validate(&allow()).unwrap();
    let g: ensaio::visita::spec::Guard = s.guard.clone().unwrap();
    env.start_guard(&g).unwrap();
    c.objects
        .borrow_mut()
        .get_mut(&("stage".into(), "Pod".into(), "broker-0".into()))
        .unwrap()["status"]["containerStatuses"][0]["restartCount"] = json!(1);
    assert!(env.pause(std::time::Duration::from_millis(10)).is_err());
    assert!(env.guard_tripped().unwrap().contains("restarted"));
}

fn liveness_event(uid: &str, pod: &str, count: u64, last: &str) -> Value {
    json!({"kind": "Event", "metadata": {"name": format!("{pod}.{uid}"), "uid": uid},
           "involvedObject": {"kind": "Pod", "name": pod, "fieldPath": "spec.containers{broker}"},
           "reason": "Unhealthy", "message": "Liveness probe failed: command timed out after 20s",
           "count": count, "lastTimestamp": last})
}

fn events(items: &[Value]) -> Value {
    json!({"kind": "List", "items": items})
}

fn with_live_broker() -> FakeCluster {
    let c = with_broker(Some("NotRequired"));
    c.objects
        .borrow_mut()
        .get_mut(&("stage".into(), "Pod".into(), "broker-0".into()))
        .unwrap()["spec"]["containers"][0]["livenessProbe"] = json!({"exec": {"command": ["rabbitmq-diagnostics", "status"]}, "timeoutSeconds": 20, "periodSeconds": 30, "failureThreshold": 6});
    c
}

const SLOW_GUARD: &str = r#":guard (:context "stage-ctx" :namespace "stage" :pods ("broker-0") :max-unready "10m" :interval "0s")"#;

#[test]
fn the_guard_trips_on_liveness_failures_short_of_the_kill_while_the_pod_stays_ready() {
    let c = with_live_broker();
    let readiness = json!({"kind": "Event", "metadata": {"name": "r", "uid": "r"},
                           "involvedObject": {"kind": "Pod", "name": "broker-0"},
                           "reason": "Unhealthy", "message": "Readiness probe failed: x", "count": 40,
                           "lastTimestamp": "2026-10-01T00:00:00Z"});
    *c.events.borrow_mut() = vec![
        events(&[
            liveness_event("e1", "broker-0", 5, "2026-10-01T00:00:00Z"),
            readiness.clone(),
        ]),
        events(&[
            liveness_event("e1", "broker-0", 6, "2026-10-01T00:00:30Z"),
            readiness.clone(),
        ]),
        events(&[liveness_event("e1", "broker-0", 7, "2026-10-01T00:01:00Z")]),
        events(&[liveness_event("e1", "broker-0", 8, "2026-10-01T00:01:30Z")]),
        events(&[liveness_event("e1", "broker-0", 9, "2026-10-01T00:02:00Z")]),
        events(&[liveness_event("e1", "broker-0", 10, "2026-10-01T00:02:30Z")]),
    ];
    let s = load_spec(STARVE, SLOW_GUARD, r#"(:place "starve") (:wait "5s")"#);
    let (r, _d) = run_on(&c, &s);
    let why = r.aborted.clone().unwrap_or_default();
    assert!(
        why.contains("liveness probe 4/6"),
        "five failures before the guard began never count; trips at 6-2: {why}"
    );
    assert_eq!(
        broker_cpu(&c, "limits"),
        json!("2"),
        "the starved pod got its cpu back"
    );
    assert_eq!(r.receipt.verdict, CertVerdict::Inconclusive);
}

#[test]
fn a_liveness_margin_is_configurable_and_a_pod_without_a_probe_gives_no_liveness_signal() {
    let c = with_live_broker();
    *c.events.borrow_mut() = vec![
        events(&[]),
        events(&[liveness_event("e1", "broker-0", 1, "2026-10-01T00:00:30Z")]),
    ];
    let guard = SLOW_GUARD.replace(":interval", ":liveness-margin 5 :interval");
    let (r, _d) = run_on(
        &c,
        &load_spec(STARVE, &guard, r#"(:place "starve") (:wait "1s")"#),
    );
    assert!(
        r.aborted
            .as_deref()
            .unwrap_or("")
            .contains("liveness probe 1/6"),
        "{:?}",
        r.aborted
    );

    let c = with_broker(Some("NotRequired"));
    *c.events.borrow_mut() = vec![events(&[liveness_event(
        "e1",
        "broker-0",
        99,
        "2026-10-01T00:00:30Z",
    )])];
    let (r, _d) = run_on(
        &c,
        &load_spec(STARVE, SLOW_GUARD, r#"(:place "starve") (:wait "0s")"#),
    );
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert!(
        calls_with(&c, "get events").is_empty(),
        "no probe, no read: absence is not a zero count"
    );
}

#[test]
fn liveness_failures_are_counted_from_aggregated_events_and_a_gap_restarts_the_streak() {
    use ensaio::visita::guard::{LivenessStreak, liveness_failures, liveness_probes};
    let probes = liveness_probes(&json!({"spec": {"containers": [
        {"name": "broker", "livenessProbe": {"failureThreshold": 6, "periodSeconds": 30}},
        {"name": "sidecar", "livenessProbe": {}},
        {"name": "plain"}]}}));
    assert_eq!(probes.len(), 2, "a container without a probe has no entry");
    assert_eq!(
        (probes[1].failure_threshold, probes[1].period_seconds),
        (3, 10),
        "API defaults"
    );
    assert_eq!(probes[0].trips_at(2), 4);
    assert_eq!(probes[1].trips_at(5), 1, "never below one");

    let series = json!({"kind": "Event", "metadata": {"name": "s"}, "involvedObject": {"kind": "Pod", "name": "p"},
                        "reason": "Unhealthy", "message": "Liveness probe failed: x",
                        "series": {"count": 4, "lastObservedTime": "2026-10-01T00:02:00.000000Z"}});
    let read = |n: u64, t: &str| liveness_failures(&events(&[liveness_event("e1", "p", n, t)]));
    assert_eq!(
        liveness_failures(&events(&[series]))[0].count,
        4,
        "events.k8s.io series.count"
    );

    let mut s = LivenessStreak::baseline(&read(3, "2026-10-01T00:00:00Z"));
    assert_eq!(
        s.observe(&read(3, "2026-10-01T00:00:00Z"), 30),
        0,
        "the baseline never counts"
    );
    assert_eq!(
        s.observe(&read(5, "2026-10-01T00:01:00Z"), 30),
        2,
        "an aggregated jump of two"
    );
    assert_eq!(s.observe(&read(6, "2026-10-01T00:01:30Z"), 30), 3);
    assert_eq!(
        s.observe(&read(7, "2026-10-01T00:03:30Z"), 30),
        1,
        "two minutes for one failure: a success in between"
    );
}

#[test]
fn a_healthy_guard_holds_and_gates_nothing() {
    let c = with_broker(Some("NotRequired"));
    let s = load_spec(STARVE, GUARD, r#"(:place "starve") (:wait "0s")"#);
    let (r, _d) = run_on(&c, &s);
    let g = r
        .results
        .iter()
        .find(|x| x.assertion_id == "guard")
        .unwrap();
    assert!(g.verdict.held(), "{:?}", g.verdict);
    assert_eq!(r.receipt.verdict, CertVerdict::Certified);
}

#[test]
fn exec_metric_judges_the_window_statistic_and_counts_unparsed_samples() {
    use ensaio::Stat;
    use ensaio::visita::env::{ExecSample, parse_number};
    let at = |v: Option<f64>| ExecSample {
        at: "t".into(),
        value: v,
        raw: String::new(),
    };
    let rec = vec![at(Some(0.0)), at(Some(3.0)), at(Some(7.0)), at(None)];
    assert!(
        judge::exec_metric(&rec, Stat::Max, Some(5.0), None).held(),
        "saturation reached"
    );
    assert!(judge::exec_metric(&rec, Stat::All, Some(5.0), None).falsified());
    assert!(judge::exec_metric(&rec, Stat::Min, None, Some(0.0)).held());
    let v = judge::exec_metric(&rec, Stat::Max, Some(5.0), None);
    assert!(
        v.subjects()
            .unwrap()
            .iter()
            .any(|o| o.as_str().contains("1 unparseable")),
        "{v:?}"
    );
    assert!(matches!(
        judge::exec_metric(&[at(None)], Stat::Max, Some(1.0), None),
        Verdict::Vacuous {}
    ));
    assert_eq!(parse_number("12\n"), Some(12.0));
    assert_eq!(parse_number("3."), Some(3.0));
    assert_eq!(parse_number("{ok,5}"), Some(5.0));
    assert_eq!(parse_number("Error: nodedown"), None);
}

#[test]
fn exec_metric_is_recorded_from_its_fixture_through_the_load() {
    let c = with_broker(Some("NotRequired"));
    let s = load_spec(
        STARVE,
        r#":assertions ((:id "runq" :kind ExecMetric :target "broker" :context "stage-ctx" :target-namespace "stage"
                         :pod "broker-0" :container "broker" :command ("rabbitmqctl" "eval" "erlang:statistics(total_run_queue_lengths).")
                         :stat max :min 8 :interval "0s" :window "1s" :record-from "starve"))"#,
        // The load comes OFF before the observe: only a record that began at
        // `starve` holds the peak.
        r#"(:place "starve") (:wait "0s") (:undo "starve") (:observe "runq")"#,
    );
    let (r, d) = run_on(&c, &s);
    let v = &r
        .results
        .iter()
        .find(|x| x.assertion_id == "runq")
        .unwrap()
        .verdict;
    assert!(
        v.held(),
        "the peak of 9 under starvation was on record: {v:?}"
    );
    let lines = std::fs::read_to_string(d.path().join("exec-runq.jsonl")).unwrap();
    assert!(lines.lines().count() >= 3, "{lines}");
}

#[test]
fn log_min_count_requires_the_reproduction_to_be_seen_enough_times() {
    use judge::PodRead::Text;
    let reads = vec![("a".to_string(), Text("connection reset\nok\n".into()))];
    let pats = vec!["connection reset".to_string()];
    assert!(judge::logs(&reads, &pats, Presence::Present, 1, false).held());
    assert!(judge::logs(&reads, &pats, Presence::Present, 2, false).falsified());
}

#[test]
fn log_match_any_holds_when_one_form_of_the_failure_appears() {
    use judge::PodRead::Text;
    let reads = vec![("a".to_string(), Text("connection reset\nok\n".into()))];
    let forms = vec!["connection reset".to_string(), "init failed".to_string()];
    assert!(
        judge::logs(&reads, &forms, Presence::Present, 1, true).held(),
        "one form is enough"
    );
    assert!(
        judge::logs(&reads, &forms, Presence::Present, 1, false).falsified(),
        "all forms required"
    );
    let quiet = vec![("a".to_string(), Text("ok\n".into()))];
    assert!(
        judge::logs(&quiet, &forms, Presence::Present, 1, true).falsified(),
        "no form at all"
    );
}

#[test]
fn a_rollout_restart_is_transient_and_its_stamp_is_allow_listed() {
    let c = seeded();
    let restart = r#"(:name "storm" :effect mutate :cleanup (:transient (:reason "the restore sync"))
      :action (:rollout-restart (:context "stage-ctx" :namespace "stage" :deployments ("svc"))))"#;
    let (r, _d) = run_on(&c, &spec(restart, r#"(:place "storm")"#));
    assert_eq!(calls_with(&c, "rollout restart deployment svc").len(), 1);
    assert!(
        r.restore.is_empty() && r.restore.allowed >= 1,
        "{:?}",
        r.restore
    );
    let e = refusal(
        &restart.replace(
            r#"(:transient (:reason "the restore sync"))"#,
            "restore-from-snapshot",
        ),
        "",
    );
    assert!(e.contains("rollout restart has no inverse"), "{e}");
}

fn ab_spec(a_logs: &str) -> (FakeCluster, VisitaSpec) {
    let c = with_broker(Some("NotRequired"));
    c.put("stage", pod("svc-a1", "any-1"));
    c.pod_logs
        .borrow_mut()
        .insert("svc-a1".into(), a_logs.into());
    let pins = r#"(:name "pin-a" :effect mutate :cleanup restore-from-snapshot
          :action (:annotate (:context "stage-ctx" :namespace "stage" :resource "configmap" :name "svc-conf" :key "branch" :value "legacy")))
        (:name "pin-b" :effect mutate :cleanup restore-from-snapshot
          :action (:annotate (:context "stage-ctx" :namespace "stage" :resource "configmap" :name "svc-conf" :key "branch" :value "opt-in")))"#;
    let s = load_spec(
        &format!("{pins} {STARVE}"),
        r#":assertions ((:id "a-reproduced" :kind LogWindow :target "svc" :context "stage-ctx" :target-namespace "stage"
                         :selector "app=svc" :patterns ("connection reset") :presence present :min-count 1)
                        (:id "b-clean" :kind LogWindow :target "svc" :context "stage-ctx" :target-namespace "stage"
                         :selector "app=svc" :patterns ("init failed") :presence absent))
           :preconditions ("a-reproduced")"#,
        r#"(:phase "A") (:place "pin-a") (:place "starve") (:observe "a-reproduced") (:undo "starve") (:undo "pin-a")
           (:phase "B") (:place "pin-b") (:place "starve") (:observe "b-clean") (:undo "starve")"#,
    );
    (c, s)
}

#[test]
fn a_two_phase_ab_certifies_only_when_side_a_reproduced() {
    let (c, s) = ab_spec("connection reset\n");
    let (r, _d) = run_on(&c, &s);
    assert_eq!(r.receipt.verdict, CertVerdict::Certified, "{:?}", r.results);
    assert_eq!(
        calls_with(&c, "--subresource resize").len(),
        4,
        "load on and off in each phase"
    );
    assert!(r.restore.is_empty(), "{:?}", r.restore);

    let (c, s) = ab_spec("nothing happened\n");
    let (r, _d) = run_on(&c, &s);
    assert_eq!(
        r.receipt.verdict,
        CertVerdict::Inconclusive,
        "B clean proves nothing if A never failed"
    );
    drop(c);
}

#[test]
fn a_phase_cannot_place_what_it_has_not_undone() {
    let e = load_spec(STARVE, "", r#"(:place "starve") (:place "starve")"#)
        .validate(&allow())
        .unwrap_err();
    assert!(e.contains("placed twice without an undo"), "{e}");
    let e = load_spec(STARVE, "", r#"(:undo "starve")"#)
        .validate(&allow())
        .unwrap_err();
    assert!(e.contains("not placed by an earlier step"), "{e}");
    assert!(
        load_spec(
            STARVE,
            "",
            r#"(:place "starve") (:undo "starve") (:place "starve")"#
        )
        .validate(&allow())
        .is_ok()
    );
}

#[test]
fn a_record_starts_at_its_own_phase_placement_not_an_earlier_one() {
    // Both records name `starve`, which is placed once per phase. Phase A
    // saturates; phase B is quiet. runq-b must see ONLY phase B.
    let c = with_broker(Some("NotRequired"));
    let quiet = r#"(:name "quiet" :effect mutate :cleanup restore-from-snapshot
      :action (:patch (:context "stage-ctx" :namespace "stage" :resource "configmap" :name "svc-conf"
                       :patch "{\"data\":{\"phase\":\"quiet\"}}")))"#;
    let runq = |id: &str| {
        format!(
            r#"(:id "{id}" :kind ExecMetric :target "broker" :context "stage-ctx" :target-namespace "stage"
                :pod "broker-0" :container "broker" :command ("rabbitmqctl" "eval" "erlang:statistics(total_run_queue_lengths).")
                :stat max :min 8 :interval "0s" :record-from "starve")"#
        )
    };
    let s = load_spec(
        &format!("{STARVE} {quiet}"),
        &format!(":assertions ({} {})", runq("runq-a"), runq("runq-b")),
        r#"(:phase "A") (:place "starve") (:wait "0s") (:observe "runq-a") (:undo "starve")
           (:phase "B") (:place "quiet") (:place "starve") (:wait "0s") (:observe "runq-b") (:undo "starve")"#,
    );
    let (r, _d) = run_on(&c, &s);
    let v = |id: &str| {
        r.results
            .iter()
            .find(|x| x.assertion_id == id)
            .unwrap()
            .verdict
            .clone()
    };
    assert!(v("runq-a").held(), "phase A saturated: {:?}", v("runq-a"));
    assert!(
        v("runq-b").falsified(),
        "phase B never saturated; A's peak must not leak into B: {:?}",
        v("runq-b")
    );
    assert!(
        ensaio::visita::run::records_starting_at(&s, 1)
            .iter()
            .all(|a| a.id == "runq-a")
    );
}

// ── run 5: a rollout already done before recording; repeated subjects ──────

#[test]
fn a_deployment_already_on_the_build_when_recording_began_is_unobserved_at_once() {
    // svc is ALREADY on the expected image and complete (the image arrived with
    // an earlier sync): the observe must not wait out its window for a rollout
    // that already happened.
    let c = seeded();
    {
        let mut objs = c.objects.borrow_mut();
        let d = objs
            .get_mut(&("stage".into(), "Deployment".into(), "svc".into()))
            .unwrap();
        merge(
            d,
            &json!({"metadata": {"generation": 9},
                         "spec": {"strategy": {"rollingUpdate": {"maxSurge": 1, "maxUnavailable": 0}},
                                  "template": {"spec": {"containers": [{"name": "app", "image": "r/svc:new-2"}]}}},
                         "status": {"observedGeneration": 9, "replicas": 3, "updatedReplicas": 3,
                                    "readyReplicas": 3, "availableReplicas": 3}}),
        );
    }
    let src = format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL}
             :fixtures ({PIN})
             :assertions ((:id "roll" :kind RolloutSurgeBound :target "svc" :context "stage-ctx"
                           :target-namespace "stage" :names ("svc") :max-surge 1 :max-unavailable 0
                           :window "30s" :interval "0s" :expect-image "new-" :record-from "pin"))
             :steps ((:place "pin") (:observe "roll")))"#
    );
    let s = load_one::<VisitaSpec>(&src).unwrap();
    let t = std::time::Instant::now();
    let (r, _d) = run_on(&c, &s);
    assert!(
        t.elapsed() < std::time::Duration::from_secs(10),
        "returned at once, not after the 30s window"
    );
    let v = &r
        .results
        .iter()
        .find(|x| x.assertion_id == "roll")
        .unwrap()
        .verdict;
    match v {
        Verdict::Skipped { reason } => assert!(
            reason.contains("already on the expected build when recording began"),
            "{reason}"
        ),
        other => panic!("expected Unobserved (Skipped), got {other:?}"),
    }
}

#[test]
fn subjects_name_each_deployment_once_with_each_generation_once() {
    // Two Deployments' samples interleaved, several per generation: the report
    // must not repeat (name, generation), and the subject count is the number
    // of Deployments watched.
    let base = BTreeMap::from([("svc".to_string(), 7), ("api".to_string(), 3)]);
    let mut trace = Vec::new();
    for (i, (u, a)) in [(1, 3), (2, 3), (3, 3), (3, 3)].into_iter().enumerate() {
        let mut s1 = sample("svc", 8, 3, 4, 3, u);
        s1.at = format!("t{i}");
        let mut s2 = sample("api", 4, 3, 4, a, 1 + i64::try_from(i).unwrap().min(2));
        s2.at = format!("t{i}");
        trace.push(s1);
        trace.push(s2);
    }
    trace.push(sample("svc", 8, 3, 3, 3, 3));
    trace.push(sample("api", 4, 3, 3, 3, 3));
    let v = judge::rollout(&trace, &base, 1, 0, None);
    assert!(v.held(), "{v:?}");
    let subjects: Vec<&str> = v.subjects().unwrap().iter().map(|o| o.as_str()).collect();
    assert_eq!(
        subjects.len(),
        2,
        "one subject per Deployment: {subjects:?}"
    );
    for (name, generation) in [("svc", "gen 8"), ("api", "gen 4")] {
        let line = subjects.iter().find(|s| s.starts_with(name)).unwrap();
        assert_eq!(
            line.matches(generation).count(),
            1,
            "{generation} once in {line}"
        );
    }
}

#[test]
fn a_record_that_begins_after_an_hpa_raise_judges_the_scale_up_not_a_loss() {
    // Load A/B 2026-10-01, names neutralised: minReplicas raised 3 -> 8, then
    // a restart storm whose record began after the raise. Its first sample
    // shows desired 8 with 3 available — five pods STARTING, nothing removed.
    let rec =
        |at: &str, generation: i64, desired: i64, replicas: i64, updated: i64, available: i64| {
            let mut s = recorded(at, generation, desired, replicas, updated, available);
            s.name = "api-deployment".into();
            s
        };
    let trace = vec![
        rec("t26:21", 8685, 8, 8, 8, 3),
        rec("t26:37", 8686, 8, 8, 1, 8),
        rec("t26:47", 8686, 8, 9, 2, 8),
        rec("t27:02", 8686, 8, 9, 3, 8),
        rec("t27:12", 8686, 8, 9, 4, 8),
        rec("t27:27", 8686, 8, 9, 5, 8),
        rec("t27:40", 8686, 8, 9, 8, 8),
        rec("t27:50", 8686, 8, 8, 8, 8),
    ];
    let base = BTreeMap::from([("api-deployment".to_string(), 8684)]);
    let events = judge::rollout_events(&trace, &base, 1, 0, None);
    assert!(events.iter().all(|e| !e.is_breach()), "{events:?}");
    assert!(judge::rollout(&trace, &base, 1, 0, None).held());
    // A pod genuinely lost after the reference still breaches.
    let mut lost = trace.clone();
    lost.insert(3, rec("t26:55", 8686, 8, 9, 2, 7));
    assert!(judge::rollout(&lost, &base, 1, 0, None).falsified());
}

// ── a pinned or dispatched branch must contain its trunk ──────────────────

const CONTAINS: &str =
    r#":branch-contains ((:repo "example-org/env" :branch "my-branch" :trunk "main"))"#;

fn pin_spec() -> VisitaSpec {
    load_one::<VisitaSpec>(&format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL} :fixtures ({PIN}) {CONTAINS} :steps ((:place "pin")))"#
    ))
    .unwrap()
}

#[test]
fn a_branch_behind_its_trunk_refuses_the_run_before_anything_is_written() {
    let c = seeded();
    c.behind.borrow_mut().insert("my-branch".into(), vec![1]);
    let s = pin_spec();
    s.validate(&allow()).unwrap();
    let dir = tempdir::Dir::new();
    let ev = Evidence::open(dir.path()).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None));
    let err = run(&s, &env).unwrap_err();
    assert!(
        err.contains("`my-branch` is 1 commit(s) behind `main`"),
        "{err}"
    );
    assert_eq!(c.mutating_calls(), 0, "nothing written");
    assert!(
        calls_with(&c, "get").is_empty(),
        "refused before even the snapshot"
    );
}

#[test]
fn trunk_moving_mid_run_refuses_the_pin_before_its_write() {
    let c = seeded();
    // Contained at run start, 3 behind by the time the pin is placed.
    c.behind.borrow_mut().insert("my-branch".into(), vec![0, 3]);
    let (r, _d) = run_on(&c, &pin_spec());
    assert!(
        r.aborted
            .as_deref()
            .unwrap_or("")
            .contains("3 commit(s) behind"),
        "{:?}",
        r.aborted
    );
    assert!(
        calls_with(&c, "annotate").is_empty(),
        "the pin was never written"
    );
    assert!(r.placed.is_empty());
    assert_eq!(r.receipt.verdict, CertVerdict::Inconclusive);
}

#[test]
fn a_branch_containing_its_trunk_places_normally() {
    let c = seeded();
    let (r, _d) = run_on(&c, &pin_spec());
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert_eq!(
        calls_with(&c, "compare/main...my-branch").len(),
        2,
        "read at start and before the pin"
    );
    assert_eq!(r.receipt.verdict, CertVerdict::Certified);
}

#[test]
fn a_malformed_branch_check_is_refused() {
    let e = refusal(
        PIN,
        r#":branch-contains ((:repo "no-owner" :branch "b" :trunk "main"))"#,
    );
    assert!(e.contains(":repo owner/name"), "{e}");
}

// ── :apply-manifest — creates only what is absent, deletes only what it made ──

const PROBE_YAML: &str = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: probe-conf\ndata:\n  k: v\n---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: probe\nspec:\n  replicas: 1\n";

fn manifest_fixture(yaml: &str, extra: &str) -> String {
    format!(
        r#"(:name "probe" :effect create :cleanup own-inverse
      :action (:apply-manifest (:context "stage-ctx" :namespace "stage" :yaml "{}" {extra})))"#,
        yaml.replace('"', "\\\"")
    )
}

#[test]
fn an_applied_manifest_is_created_recorded_and_deleted_by_identity() {
    let c = seeded();
    let s = spec(
        &manifest_fixture(PROBE_YAML, r#":await-available ("probe")"#),
        r#"(:place "probe")"#,
    );
    let (r, d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert_eq!(calls_with(&c, " create -f ").len(), 1);
    let index = std::fs::read_to_string(d.path().join("steps.jsonl")).unwrap();
    let created = index
        .lines()
        .find(|l| l.contains("\"created\""))
        .expect("created identities on record");
    for id in [
        "\"kind\":\"ConfigMap\"",
        "\"name\":\"probe-conf\"",
        "\"name\":\"probe\"",
        "\"uid\":\"uid-1\"",
        "\"uid\":\"uid-2\"",
    ] {
        assert!(created.contains(id), "{id} in {created}");
    }
    assert_eq!(
        calls_with(&c, " delete ").len(),
        2,
        "exactly the two created objects"
    );
    assert!(
        calls_with(&c, "delete deployment.v1.apps probe").len() == 1
            && calls_with(&c, "delete configmap probe-conf").len() == 1
    );
    assert!(
        !c.objects
            .borrow()
            .keys()
            .any(|(_, _, n)| n.starts_with("probe")),
        "gone after the undo"
    );
    assert_eq!(r.receipt.verdict, CertVerdict::Certified);
}

#[test]
fn an_applied_manifest_naming_any_existing_object_is_refused_before_any_write() {
    let c = seeded();
    let both = format!(
        "{PROBE_YAML}---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: svc-conf\ndata:\n  pool_size: \"99\"\n"
    );
    let (r, _d) = run_on(
        &c,
        &spec(&manifest_fixture(&both, ""), r#"(:place "probe")"#),
    );
    assert!(
        r.aborted
            .as_deref()
            .unwrap_or("")
            .contains("ConfigMap/svc-conf already exist"),
        "{:?}",
        r.aborted
    );
    assert!(calls_with(&c, " create ").is_empty(), "nothing written");
    assert!(
        calls_with(&c, " delete ").is_empty(),
        "the undo deletes nothing it did not create"
    );
    assert_eq!(
        c.get("stage", "ConfigMap", "svc-conf")["data"]["pool_size"],
        "4"
    );
    assert_eq!(r.receipt.verdict, CertVerdict::Inconclusive);
}

#[test]
fn a_manifest_deployment_that_never_becomes_available_aborts_and_is_deleted() {
    let c = seeded();
    *c.created_unavailable.borrow_mut() = true;
    let (r, _d) = run_on(
        &c,
        &spec(
            &manifest_fixture(PROBE_YAML, r#":await-available ("probe") :timeout "0s""#),
            r#"(:place "probe")"#,
        ),
    );
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("not Available"),
        "{:?}",
        r.aborted
    );
    assert!(
        !c.objects
            .borrow()
            .keys()
            .any(|(_, _, n)| n.starts_with("probe")),
        "deleted after the failed placement"
    );
}

#[test]
fn a_created_object_that_will_not_go_is_an_undo_failure_at_the_bound() {
    let c = seeded();
    *c.deletes_ignored.borrow_mut() = true;
    let (r, _d) = run_on(
        &c,
        &spec(
            &manifest_fixture(PROBE_YAML, r#":timeout "0s""#),
            r#"(:place "probe")"#,
        ),
    );
    assert!(
        r.undo_errors
            .iter()
            .any(|e| e.contains("still present 0s after delete")),
        "{:?}",
        r.undo_errors
    );
    assert_eq!(r.receipt.verdict, CertVerdict::NotCertified);
}

#[test]
fn a_manifest_file_is_read_relative_to_the_spec() {
    let dir = tempdir::Dir::new();
    std::fs::create_dir_all(dir.path().join("manifests")).unwrap();
    std::fs::write(dir.path().join("manifests/probe.yaml"), PROBE_YAML).unwrap();
    let src = format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL}
             :fixtures ((:name "probe" :effect create :cleanup own-inverse
                         :action (:apply-manifest (:context "stage-ctx" :namespace "stage" :file "manifests/probe.yaml"))))
             :steps ((:place "probe")))"#
    );
    let unresolved = load_one::<VisitaSpec>(&src).unwrap();
    assert!(
        unresolved
            .validate(&allow())
            .unwrap_err()
            .contains("manifests/probe.yaml")
    );
    let s = load_one::<VisitaSpec>(&src)
        .unwrap()
        .relative_to(dir.path());
    let c = seeded();
    let (r, _d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert_eq!(calls_with(&c, " delete ").len(), 2);
}

#[test]
fn apply_manifest_parses_from_the_lisp_surface_and_its_misuses_are_refused() {
    let s = spec(
        &manifest_fixture(PROBE_YAML, r#":await-available ("probe") :timeout "90s""#),
        r#"(:place "probe")"#,
    );
    match &s.fixtures[0].action {
        Some(ensaio::visita::Action::ApplyManifest {
            context,
            namespace,
            yaml,
            file,
            await_available,
            timeout,
        }) => {
            assert_eq!(
                (context.as_str(), namespace.as_str()),
                ("stage-ctx", "stage")
            );
            assert_eq!(yaml.as_deref(), Some(PROBE_YAML));
            assert!(file.is_none());
            assert_eq!(await_available, &vec!["probe".to_string()]);
            assert_eq!(timeout.as_deref(), Some("90s"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(s.fixtures[0].cleanup, ensaio::fixture::Cleanup::OwnInverse);
    s.validate(&allow()).unwrap();

    let e = refusal(
        &manifest_fixture(PROBE_YAML, "").replace("own-inverse", "(:transient (:reason \"x\"))"),
        "",
    );
    assert!(e.contains("must declare :cleanup own-inverse"), "{e}");
    let e = refusal(&manifest_fixture(PROBE_YAML, r#":file "x.yaml""#), "");
    assert!(e.contains("both :file and :yaml"), "{e}");
    let e = refusal(
        &manifest_fixture(PROBE_YAML, "").replace(":effect create", ":effect mutate"),
        "",
    );
    assert!(e.contains("declare :effect create"), "{e}");
    let e = refusal(
        &manifest_fixture(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n  namespace: other\n",
            "",
        ),
        "",
    );
    assert!(e.contains("names namespace `other`"), "{e}");
    let e = refusal(
        &manifest_fixture(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  generateName: x-\n",
            "",
        ),
        "",
    );
    assert!(e.contains("generateName"), "{e}");
    let e = refusal(&PIN.replace("restore-from-snapshot", "own-inverse"), "");
    assert!(e.contains("own-inverse applies only to"), "{e}");
}

// ── :undo-phase after-restore — a proxy outlives the restore that needs it ──

const PROXY_YAML: &str =
    "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: proxy\nspec:\n  replicas: 1\n";

const SYNC: &str = r#"(:name "sync" :effect mutate :cleanup (:transient (:reason "the natural revision"))
      :action (:patch (:context "stage-ctx" :namespace "stage" :resource "configmap" :name "svc-conf"
                       :patch "{\"data\":{\"pool_size\":\"4\"}}")))"#;

fn proxy(extra: &str) -> String {
    format!(
        r#"(:name "proxy" :effect create :cleanup own-inverse {extra}
      :action (:apply-manifest (:context "stage-ctx" :namespace "stage" :yaml "{}" :timeout "0s")))"#,
        PROXY_YAML.replace('"', "\\\"")
    )
}

fn proxy_spec(natural: &str, fixtures: &str, extra: &str, steps: &str) -> VisitaSpec {
    load_one::<VisitaSpec>(&format!(
        r#"(defvisita :name "t" :claim "c" {natural} :fixtures ({fixtures}) :restore ("sync") {extra} :steps ({steps}))"#
    ))
    .unwrap()
}

const WHOLE_NAMESPACE: &str = r#"
  :natural-state ((:context "stage-ctx" :namespace "stage" :kinds ("deployments"))
                  (:context "stage-ctx" :namespace "stage" :kinds ("secrets") :selector "project=stage"))"#;

fn first(c: &FakeCluster, word: &str) -> usize {
    *calls_with(c, word)
        .first()
        .unwrap_or_else(|| panic!("no call with `{word}`"))
}

#[test]
fn an_after_restore_fixture_is_undone_only_after_the_restore_converged() {
    let c = seeded();
    let s = proxy_spec(
        WHOLE_NAMESPACE,
        &format!("{PIN} {} {SYNC}", proxy(":undo-phase after-restore")),
        "",
        r#"(:place "pin") (:place "proxy")"#,
    );
    let (r, d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    let (unpin, sync, delete) = (
        first(&c, "branch-pin-"),
        first(&c, r#""pool_size":"4""#),
        first(&c, "delete deployment.v1.apps proxy"),
    );
    assert!(
        unpin < sync && sync < delete,
        "unpin {unpin} < restore sync {sync} < proxy delete {delete}"
    );
    assert!(
        calls_with(&c, "get deployments")
            .iter()
            .any(|i| *i > sync && *i < delete),
        "the restore check ran between the restore and the proxy's delete"
    );
    let held: Value =
        serde_json::from_slice(&std::fs::read(d.path().join("restore-diff-held.json")).unwrap())
            .unwrap();
    assert_eq!(
        held["extra"],
        json!([]),
        "the still-placed proxy is not an extra object: {held}"
    );
    assert!(
        r.restore.is_empty(),
        "and the final diff finds it gone: {:?}",
        r.restore
    );
    assert_eq!(r.undone.last().map(String::as_str), Some("proxy"));
    assert_eq!(r.receipt.verdict, CertVerdict::Certified);
}

#[test]
fn an_after_restore_object_left_behind_shows_in_the_final_diff() {
    let c = seeded();
    *c.deletes_ignored.borrow_mut() = true;
    let s = proxy_spec(
        WHOLE_NAMESPACE,
        &format!("{} {SYNC}", proxy(":undo-phase after-restore")),
        "",
        r#"(:place "proxy")"#,
    );
    let (r, d) = run_on(&c, &s);
    let held: Value =
        serde_json::from_slice(&std::fs::read(d.path().join("restore-diff-held.json")).unwrap())
            .unwrap();
    assert_eq!(held["extra"], json!([]), "{held}");
    assert!(
        r.restore
            .extra
            .iter()
            .any(|k| k.ends_with("Deployment/proxy")),
        "{:?}",
        r.restore
    );
    assert_eq!(r.receipt.verdict, CertVerdict::NotCertified);
}

#[test]
fn on_a_guard_trip_an_after_restore_fixture_is_still_undone_last() {
    let c = with_broker(Some("NotRequired"));
    *c.starve_on_resize.borrow_mut() = true;
    let s = proxy_spec(
        BROKER_STATE,
        &format!("{STARVE} {} {SYNC}", proxy(":undo-phase after-restore")),
        GUARD,
        r#"(:place "proxy") (:place "starve") (:wait "5s")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("not Ready"),
        "{:?}",
        r.aborted
    );
    let resize_back = *calls_with(&c, "--subresource resize").last().unwrap();
    let (sync, delete) = (
        first(&c, r#""pool_size":"4""#),
        first(&c, "delete deployment.v1.apps proxy"),
    );
    assert!(
        resize_back < sync && sync < delete,
        "resize back {resize_back} < restore {sync} < proxy delete {delete}"
    );
}

#[test]
fn undo_phase_parses_and_is_refused_where_there_is_nothing_to_defer() {
    let s = proxy_spec(
        WHOLE_NAMESPACE,
        &format!("{} {SYNC}", proxy(":undo-phase after-restore")),
        "",
        r#"(:place "proxy")"#,
    );
    assert_eq!(
        s.fixtures[0].undo_phase,
        ensaio::fixture::UndoPhase::AfterRestore
    );
    assert_eq!(
        s.fixtures[1].undo_phase,
        ensaio::fixture::UndoPhase::InOrder,
        "the default"
    );
    let e = refusal(
        &SYNC.replace(":cleanup", ":undo-phase after-restore :cleanup"),
        "",
    );
    assert!(e.contains("after-restore"), "{e}");
    let e = refusal(
        &dispatch("stop", "").replace(":cleanup", ":undo-phase after-restore :cleanup"),
        "",
    );
    assert!(e.contains("after-restore"), "{e}");
}

// ── :exec — a declared mutating command, undone by its own argv or declared transient ──

fn exec_fixture(target: &str, extra: &str, cleanup: &str) -> String {
    format!(
        r#"(:name "squeeze" :effect mutate :cleanup {cleanup}
      :action (:exec (:context "stage-ctx" :namespace "stage" {target} :container "broker"
                      :argv ("rabbitmqctl" "set_vm_memory_high_watermark" "0.2") {extra})))"#
    )
}

const UNDO_ARGV: &str = r#":undo-argv ("rabbitmqctl" "set_vm_memory_high_watermark" "0.4")"#;

fn exec_pods(c: &FakeCluster, arg: &str) -> Vec<String> {
    c.calls
        .borrow()
        .iter()
        .filter(|a| a.iter().any(|x| x == "exec") && a.iter().any(|x| x == arg))
        .map(|a| a[6].clone())
        .collect()
}

fn unready(name: &str) -> Value {
    let mut p = broker_named(name, Some("NotRequired"));
    p["status"]["conditions"] = json!([{"type": "Ready", "status": "False"}]);
    p
}

#[test]
fn an_exec_runs_in_the_first_ready_pod_or_all_and_its_undo_argv_runs_where_it_ran() {
    let c = with_broker(Some("NotRequired"));
    c.put("stage", broker_named("broker-2", Some("NotRequired")));
    c.put("stage", unready("broker-1"));
    let s = load_spec(
        &exec_fixture(r#":selector "app=broker""#, UNDO_ARGV, "own-inverse"),
        "",
        r#"(:place "squeeze")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert_eq!(
        exec_pods(&c, "0.2"),
        vec!["broker-0"],
        "the first Ready pod by name"
    );
    assert_eq!(
        exec_pods(&c, "0.4"),
        vec!["broker-0"],
        "undone where it ran"
    );
    assert_eq!(r.undone, vec!["squeeze"]);

    let c = with_broker(Some("NotRequired"));
    c.put("stage", broker_named("broker-2", Some("NotRequired")));
    c.put("stage", unready("broker-1"));
    let s = load_spec(
        &exec_fixture(
            r#":selector "app=broker""#,
            &format!(":all #t {UNDO_ARGV}"),
            "own-inverse",
        ),
        "",
        r#"(:place "squeeze")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert_eq!(
        exec_pods(&c, "0.2"),
        vec!["broker-0", "broker-2"],
        "every Ready pod, never the unready one"
    );
    assert_eq!(exec_pods(&c, "0.4"), vec!["broker-0", "broker-2"]);
}

#[test]
fn a_failing_exec_aborts_the_trial_and_is_still_undone() {
    let c = with_broker(Some("NotRequired"));
    c.exec_fails.borrow_mut().push("0.2".into());
    let s = load_spec(
        &exec_fixture(r#":pod "broker-0""#, UNDO_ARGV, "own-inverse"),
        "",
        r#"(:place "squeeze") (:wait "0s")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("exit 1"),
        "{:?}",
        r.aborted
    );
    assert_eq!(
        exec_pods(&c, "0.4"),
        vec!["broker-0"],
        "the undo ran where the failed exec was attempted"
    );
    assert_eq!(r.receipt.verdict, CertVerdict::Inconclusive);
}

#[test]
fn a_transient_exec_runs_nothing_on_undo_and_a_selector_with_no_ready_pod_is_refused() {
    let c = with_broker(Some("NotRequired"));
    let s = load_spec(
        &exec_fixture(
            r#":pod "broker-0""#,
            "",
            r#"(:transient (:reason "the watermark resets on restart"))"#,
        ),
        "",
        r#"(:place "squeeze")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert_eq!(
        c.calls
            .borrow()
            .iter()
            .filter(|a| a.iter().any(|x| x == "exec"))
            .count(),
        1
    );

    let c = with_broker(Some("NotRequired"));
    c.put("stage", unready("broker-0"));
    let s = load_spec(
        &exec_fixture(r#":selector "app=broker""#, UNDO_ARGV, "own-inverse"),
        "",
        r#"(:place "squeeze")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("no Ready pod"),
        "{:?}",
        r.aborted
    );
    assert!(
        exec_pods(&c, "0.4").is_empty(),
        "nothing ran, so nothing is undone"
    );
}

#[test]
fn on_a_trip_an_exec_on_the_tripped_pod_is_undone_first() {
    let c = with_broker(Some("NotRequired"));
    *c.starve_on_resize.borrow_mut() = true;
    let conf = r#"(:name "conf" :effect mutate :cleanup restore-from-snapshot
            :action (:patch (:context "stage-ctx" :namespace "stage" :resource "configmap" :name "svc-conf"
                             :patch "{\"data\":{\"pool_size\":\"9\"}}")))"#;
    let s = load_spec(
        &format!(
            "{} {conf} {STARVE}",
            exec_fixture(r#":selector "app=broker""#, UNDO_ARGV, "own-inverse")
        ),
        GUARD,
        r#"(:place "squeeze") (:place "conf") (:place "starve") (:wait "5s")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted.as_deref().unwrap_or("").contains("not Ready"),
        "{:?}",
        r.aborted
    );
    let undo_exec = first(&c, "0.4");
    let conf_back = first(&c, r#""pool_size":"4""#);
    assert!(
        undo_exec < conf_back,
        "the exec on the tripped pod {undo_exec} before the other undo {conf_back}"
    );
}

#[test]
fn exec_parses_from_the_lisp_surface_and_an_irreversible_one_must_say_so() {
    let s = load_spec(
        &exec_fixture(
            r#":selector "app=broker""#,
            &format!(":all #t {UNDO_ARGV}"),
            "own-inverse",
        ),
        "",
        r#"(:place "squeeze")"#,
    );
    match &s.fixtures[0].action {
        Some(ensaio::visita::Action::Exec {
            context,
            namespace,
            pod,
            selector,
            container,
            all,
            argv,
            undo_argv,
        }) => {
            assert_eq!(
                (context.as_str(), namespace.as_str(), container.as_str()),
                ("stage-ctx", "stage", "broker")
            );
            assert_eq!(
                (pod.as_deref(), selector.as_deref(), *all),
                (None, Some("app=broker"), true)
            );
            assert_eq!(
                argv,
                &vec![
                    "rabbitmqctl".to_string(),
                    "set_vm_memory_high_watermark".into(),
                    "0.2".into()
                ]
            );
            assert_eq!(undo_argv.as_ref().unwrap()[2], "0.4");
        }
        other => panic!("{other:?}"),
    }
    s.validate(&allow()).unwrap();

    let e = refusal(&exec_fixture(r#":pod "broker-0""#, "", "own-inverse"), "");
    assert!(
        e.contains("without :undo-argv") && e.contains(":transient"),
        "{e}"
    );
    let e = refusal(
        &exec_fixture(r#":pod "broker-0""#, "", "restore-from-snapshot"),
        "",
    );
    assert!(e.contains("without :undo-argv"), "{e}");
    let e = refusal(
        &exec_fixture(
            r#":pod "broker-0""#,
            UNDO_ARGV,
            r#"(:transient (:reason "x"))"#,
        ),
        "",
    );
    assert!(e.contains("declare :cleanup own-inverse"), "{e}");
    let e = refusal(
        &exec_fixture(
            r#":pod "broker-0" :selector "app=broker""#,
            UNDO_ARGV,
            "own-inverse",
        ),
        "",
    );
    assert!(e.contains("one of :pod or :selector"), "{e}");
    let e = refusal(
        &exec_fixture(
            r#":pod "broker-0""#,
            &format!(":all #t {UNDO_ARGV}"),
            "own-inverse",
        ),
        "",
    );
    assert!(e.contains(":all applies to a :selector"), "{e}");
    let e = refusal(
        &exec_fixture(r#":pod "broker-0""#, ":undo-argv ()", "own-inverse"),
        "",
    );
    assert!(e.contains("empty"), "{e}");
}

const DRILL_EXPECT: &str = r#":expect ((:path "status.pass" :equals true)
                   (:path "status.rtoSeconds" :max 60)
                   (:path "status.mode" :equals "drill")
                   (:path "status.revertVerdict" :one-of ("confirmed"))
                   (:path "status.reads[0].failures" :equals 0))"#;

fn drill_assertion(extra: &str) -> String {
    format!(
        r#":assertions ((:id "drill" :kind ObjectFields :target "drill" :context "stage-ctx"
                         :api-version "drills.example.io/v1" :resource-kind "DrillSession" :name "drill-a"
                         {extra}))"#
    )
}

fn drill_spec(extra: &str) -> VisitaSpec {
    load_one::<VisitaSpec>(&format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL} {} :steps ((:observe "drill")))"#,
        drill_assertion(extra)
    ))
    .expect("spec compiles")
}

fn session(terminal: bool, run: &str, pass: bool, rto: i64, verdict: &str) -> Value {
    json!({"apiVersion": "drills.example.io/v1", "kind": "DrillSession", "metadata": {"name": "drill-a"},
           "status": {"terminal": terminal, "runId": run, "pass": pass, "rtoSeconds": rto, "mode": "drill",
                      "revertVerdict": verdict, "reads": [{"failures": 0}]}})
}

fn pred(v: Value) -> ensaio::FieldPredicate {
    serde_json::from_value(v).expect("predicate parses")
}

fn drill_verdict(c: &FakeCluster, extra: &str) -> (Verdict, tempdir::Dir) {
    let s = drill_spec(extra);
    let (r, d) = run_on(c, &s);
    (
        r.results
            .into_iter()
            .find(|x| x.assertion_id == "drill")
            .unwrap()
            .verdict,
        d,
    )
}

const WAIT_DRILL: &str = r#":wait-until ((:path "status.terminal" :equals true) (:path "status.runId" :equals "t-drill"))
                            :timeout "2s" :interval "10ms""#;

#[test]
fn object_fields_parses_every_predicate_from_the_lisp_surface() {
    use ensaio::{FieldTest, Scalar};
    let s = drill_spec(&format!(
        r#":target-namespace "stage" :wait-until (:path "status.terminal" :equals true) :timeout "25m" :interval "10s"
           {DRILL_EXPECT}"#
    ));
    s.validate(&allow()).unwrap();
    let o = &s.assertions[0].operator;
    assert_eq!(o.api_version.as_deref(), Some("drills.example.io/v1"));
    assert_eq!(o.resource_kind.as_deref(), Some("DrillSession"));
    assert_eq!(o.name.as_deref(), Some("drill-a"));
    let wait = o.wait_until.as_ref().unwrap();
    assert_eq!(wait.len(), 1, "a single predicate is a list of one");
    assert_eq!(
        wait[0].test,
        FieldTest::Equals(Scalar::Bool(true)),
        "a bare true is a boolean"
    );
    let tests: Vec<&FieldTest> = o.expect.as_ref().unwrap().iter().map(|p| &p.test).collect();
    assert_eq!(
        tests[1],
        &FieldTest::Range {
            min: None,
            max: Some(60.0)
        }
    );
    assert_eq!(tests[2], &FieldTest::Equals(Scalar::Text("drill".into())));
    assert_eq!(
        tests[3],
        &FieldTest::OneOf(vec![Scalar::Text("confirmed".into())])
    );
    assert_eq!(tests[4], &FieldTest::Equals(Scalar::Number(0.0)));
    assert_eq!(
        o.expect.as_ref().unwrap()[4].path.as_str(),
        "status.reads[0].failures"
    );

    let both = drill_spec(&format!("{WAIT_DRILL} {DRILL_EXPECT}"));
    assert_eq!(
        both.assertions[0]
            .operator
            .wait_until
            .as_ref()
            .unwrap()
            .len(),
        2
    );

    assert_eq!(
        pred(json!({"path": "a.b", "present": true})).test,
        FieldTest::Present
    );
    assert_eq!(
        pred(json!({"path": "a.b", "absent": "true"})).test,
        FieldTest::Absent
    );
    assert_eq!(
        pred(json!({"path": "a", "min": 1, "max": 2})).test,
        FieldTest::Range {
            min: Some(1.0),
            max: Some(2.0)
        }
    );
}

#[test]
fn object_fields_missing_parameters_are_refused_at_load() {
    let e = refusal(
        "",
        r#":assertions ((:id "o" :kind ObjectFields :target "x" :context "stage-ctx"))"#,
    );
    assert!(
        e.contains(
            "assertion `o` (ObjectFields): missing :api-version, :resource-kind, :name, :expect"
        ),
        "{e}"
    );
    let e = refusal(
        "",
        r#":assertions ((:id "o" :kind ObjectFields :target "x" :api-version "v1" :resource-kind "ConfigMap" :name "c"
                         :expect ((:path "data.k" :present true))))"#,
    );
    assert!(e.contains("missing :context"), "{e}");
    let e = refusal(
        "",
        r#":assertions ((:id "o" :kind ObjectFields :target "x" :context "stage-ctx" :api-version "v1" :resource-kind "ConfigMap" :name "c"
                         :expect ()))"#,
    );
    assert!(e.contains("missing :expect"), "{e}");
    let e = refusal(
        "",
        &drill_assertion(&format!(r#":timeout "soon" :interval "1x" {DRILL_EXPECT}"#)),
    );
    assert!(e.contains("`soon`") && e.contains("`1x`"), "{e}");
    let e = refusal(
        "",
        &drill_assertion(DRILL_EXPECT).replace("drills.example.io/v1", "a/b/c"),
    );
    assert!(e.contains(":api-version `a/b/c`"), "{e}");
}

#[test]
fn object_fields_paths_and_predicates_are_checked_when_the_spec_compiles() {
    let bad = |p: &str| {
        let src = format!(
            r#"(defvisita :name "t" :claim "c" {NATURAL} {} :steps ())"#,
            drill_assertion(&format!(":expect ({p})"))
        );
        load_one::<VisitaSpec>(&src)
            .expect_err("refused at load")
            .to_string()
    };
    assert!(bad(r#"(:path "status..pass" :equals true)"#).contains("empty segment"));
    assert!(bad(r#"(:path "status.reads[x]" :equals 0)"#).contains("not a number"));
    assert!(bad(r#"(:path "status.reads[0" :equals 0)"#).contains("not an index"));
    assert!(bad(r#"(:path "" :equals 0)"#).contains("path is empty"));
    assert!(bad(r#"(:path "status.pass")"#).contains("tests nothing"));
    assert!(bad(r#"(:path "status.rto" :equals 1 :max 2)"#).contains("gives 2 tests"));
    assert!(bad(r#"(:path "status.rto" :min 9 :max 2)"#).contains("above :max"));
    assert!(bad(r#"(:path "status.x" :one-of ())"#).contains("one-of"));
    assert!(bad(r#"(:path "status.x" :present false)"#).contains(":present takes true"));
}

#[test]
fn object_fields_judge_held_falsified_timeout_and_blind() {
    use judge::{ObjectWatch, Seen};
    let wait = vec![pred(json!({"path": "status.terminal", "equals": true}))];
    let expect = vec![
        pred(json!({"path": "status.pass", "equals": true})),
        pred(json!({"path": "status.rtoSeconds", "max": 60})),
        pred(json!({"path": "status.revertVerdict", "oneOf": ["confirmed"]})),
        pred(json!({"path": "status.mode", "equals": "rehearsal"})),
    ];
    let subject = "DrillSession/drill-a";

    let good = session(true, "t-drill", true, 41, "confirmed");
    let mut good = good;
    good["status"]["mode"] = json!("rehearsal");
    let v = judge::object_fields(
        subject,
        &ObjectWatch::Settled(Seen::Object(good.clone())),
        &wait,
        &expect,
    );
    assert!(v.held(), "{v:?}");
    let subjects: Vec<&str> = v
        .subjects()
        .unwrap()
        .iter()
        .map(ensaio::Observation::as_str)
        .collect();
    assert!(
        subjects
            .iter()
            .any(|o| o.contains("status.rtoSeconds") && o.contains("41")),
        "{subjects:?}"
    );

    let mut bad = good.clone();
    bad["status"]["rtoSeconds"] = json!(75);
    bad["status"]["revertVerdict"] = json!("pending");
    bad["status"].as_object_mut().unwrap().remove("mode");
    let v = judge::object_fields(
        subject,
        &ObjectWatch::Settled(Seen::Object(bad)),
        &wait,
        &expect,
    );
    let Verdict::Falsified { findings, .. } = &v else {
        panic!("{v:?}")
    };
    let f: Vec<&str> = findings.iter().map(ensaio::Observation::as_str).collect();
    assert_eq!(f.len(), 3, "{f:?}");
    assert!(
        f.contains(&"status.rtoSeconds: observed 75, wanted <= 60"),
        "{f:?}"
    );
    assert!(
        f.contains(&r#"status.revertVerdict: observed "pending", wanted one of ["confirmed"]"#),
        "{f:?}"
    );
    assert!(
        f.contains(&r#"status.mode: observed absent, wanted = "rehearsal""#),
        "{f:?}"
    );

    let running = session(false, "t-drill", false, 0, "");
    let v = judge::object_fields(
        subject,
        &ObjectWatch::TimedOut {
            after: "25m".into(),
            last: Seen::Object(running.clone()),
        },
        &wait,
        &expect,
    );
    let Verdict::Falsified {
        findings, subjects, ..
    } = &v
    else {
        panic!("a timeout is never held: {v:?}")
    };
    assert!(
        findings
            .iter()
            .next()
            .unwrap()
            .as_str()
            .starts_with("timed out after 25m before :wait-until held"),
        "{findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|o| o.as_str() == "wait-until status.terminal: observed false, wanted = true"),
        "{findings:?}"
    );
    assert!(
        subjects
            .iter()
            .any(|o| o.as_str().contains("status.pass") && o.as_str().contains("false")),
        "{subjects:?}"
    );

    let v = judge::object_fields(
        subject,
        &ObjectWatch::TimedOut {
            after: "25m".into(),
            last: Seen::Absent("NotFound".into()),
        },
        &wait,
        &expect,
    );
    let Verdict::Falsified { findings, .. } = &v else {
        panic!("{v:?}")
    };
    assert!(
        findings.iter().any(|o| o.as_str().contains("never found")),
        "{findings:?}"
    );

    let v = judge::object_fields(
        subject,
        &ObjectWatch::Blind {
            reason: "Error from server (Forbidden)".into(),
            last: Some(Seen::Object(running)),
        },
        &wait,
        &expect,
    );
    let Verdict::Skipped { reason } = &v else {
        panic!("blind is unobserved, never a claim: {v:?}")
    };
    assert!(
        reason.starts_with("unobserved (blind)") && reason.contains("Forbidden"),
        "{reason}"
    );
    assert!(reason.contains("status.terminal = false"), "{reason}");
    let v = judge::object_fields(
        subject,
        &ObjectWatch::Blind {
            reason: "connection refused".into(),
            last: None,
        },
        &wait,
        &expect,
    );
    assert!(matches!(v, Verdict::Skipped { .. }), "{v:?}");
}

#[test]
fn object_fields_waits_through_the_api_until_the_session_is_terminal() {
    let c = seeded();
    *c.object_reads.borrow_mut() = vec![
        ObjRead::State(None),
        ObjRead::State(Some(session(false, "t-rehearsal", true, 30, "confirmed"))),
        ObjRead::Fail("Unable to connect to the server: dial tcp: i/o timeout"),
        ObjRead::State(Some(session(true, "t-rehearsal", true, 30, "confirmed"))),
        ObjRead::State(Some(session(false, "t-drill", false, 0, ""))),
        ObjRead::State(Some(session(true, "t-drill", true, 44, "confirmed"))),
    ];
    let (v, d) = drill_verdict(&c, &format!("{WAIT_DRILL} {DRILL_EXPECT}"));
    assert!(v.held(), "{v:?}");
    let reads = c
        .calls
        .borrow()
        .iter()
        .filter(|a| a.iter().any(|x| x == "drillsession.v1.drills.example.io"))
        .count();
    assert_eq!(reads, 6, "read until the drill's own run was terminal");
    let rec: Value =
        serde_json::from_str(&std::fs::read_to_string(d.path().join("object-drill.json")).unwrap())
            .unwrap();
    assert_eq!(rec["object"], "DrillSession/drill-a");
    assert_eq!(rec["fields"]["status.rtoSeconds"], 44);
    assert_eq!(rec["fields"]["status.runId"], "t-drill");
    let samples = std::fs::read_to_string(d.path().join("object-drill.jsonl")).unwrap();
    assert_eq!(samples.lines().count(), 6, "{samples}");
}

#[test]
fn object_fields_that_never_settle_time_out_with_the_last_values() {
    let c = seeded();
    *c.object_reads.borrow_mut() = vec![ObjRead::State(Some(session(
        false, "t-drill", false, 0, "",
    )))];
    let (v, _d) = drill_verdict(
        &c,
        &format!("{} {DRILL_EXPECT}", WAIT_DRILL.replace("2s", "200ms")),
    );
    let Verdict::Falsified { findings, .. } = &v else {
        panic!("{v:?}")
    };
    assert!(
        findings
            .iter()
            .next()
            .unwrap()
            .as_str()
            .starts_with("timed out after 200ms"),
        "{findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|o| o.as_str().contains("status.terminal: observed false")),
        "{findings:?}"
    );
}

#[test]
fn object_fields_behind_a_forbidden_or_unreachable_api_are_unobserved() {
    let c = seeded();
    *c.object_reads.borrow_mut() = vec![ObjRead::Fail(
        r#"Error from server (Forbidden): drillsessions "drill-a" is forbidden: User "u" cannot get resource"#,
    )];
    let (v, _d) = drill_verdict(&c, &format!("{WAIT_DRILL} {DRILL_EXPECT}"));
    let Verdict::Skipped { reason } = &v else {
        panic!("{v:?}")
    };
    assert!(reason.contains("Forbidden"), "{reason}");
    let reads = c
        .calls
        .borrow()
        .iter()
        .filter(|a| a.iter().any(|x| x == "drillsession.v1.drills.example.io"))
        .count();
    assert_eq!(
        reads, 1,
        "a forbidden read is not retried until the timeout"
    );

    let c = seeded();
    *c.object_reads.borrow_mut() = vec![
        ObjRead::State(Some(session(false, "t-drill", false, 0, ""))),
        ObjRead::Fail(
            "Unable to connect to the server: dial tcp 10.0.0.1:443: connect: connection refused",
        ),
    ];
    let (v, _d) = drill_verdict(
        &c,
        &format!("{} {DRILL_EXPECT}", WAIT_DRILL.replace("2s", "200ms")),
    );
    let Verdict::Skipped { reason } = &v else {
        panic!("{v:?}")
    };
    assert!(
        reason.contains("connection refused") && reason.contains("status.terminal = false"),
        "{reason}"
    );
}

#[test]
fn object_fields_without_wait_until_judge_the_first_answer_and_a_missing_object_fails() {
    let c = seeded();
    let (v, _d) = drill_verdict(
        &c,
        &format!(r#":timeout "1s" :interval "10ms" {DRILL_EXPECT}"#),
    );
    let Verdict::Falsified { findings, .. } = &v else {
        panic!("{v:?}")
    };
    assert!(
        findings
            .iter()
            .any(|o| o.as_str() == "status.pass: observed absent, wanted = true"),
        "{findings:?}"
    );
}

// ── a lost runner: delete our own job's pod, then judge the run's jobs ──

const CI: &str = r#"(:name "ci" :effect mutate :cleanup stop
      :action (:dispatch-workflow (:repo "example-org/example-repo" :workflow "pipeline.yml" :git-ref "my-branch"
               :inputs ((:name "runner_loss_retry" :value "on"))
               :await (:job "suite (spot)" :step "Check out code")
               :accept-failure ("suite (spot)")
               :poll "0s" :timeout "5s")))"#;

const LOSE_RUNNER: &str = r#"(:name "lose-runner" :effect mutate
      :cleanup (:transient (:reason "an ephemeral runner pod; its controller replaces it"))
      :action (:delete-runner-pod (:context "stage-ctx" :namespace "stage" :run-of "ci" :job "suite (spot)")))"#;

const SHUTDOWN: &str = "The runner has received a shutdown signal";

fn chain_assertion(timeout: &str) -> String {
    format!(
        r#":assertions ((:id "chain" :kind WorkflowJobs :target "pipeline" :run-of "ci" :timeout "{timeout}" :interval "0s"
                       :jobs ((:job "suite (spot)" :conclusion "failure" :log-contains "{SHUTDOWN}")
                              (:job "suite (triage)" :conclusion "success")
                              (:job "suite (retry)" :conclusion "success")
                              (:job "suite" :conclusion "success"))))"#
    )
}

fn chain_spec(fixtures: &str, extra: &str, steps: &str) -> VisitaSpec {
    load_one::<VisitaSpec>(&format!(
        r#"(defvisita :name "t" :claim "c" {NATURAL} :fixtures ({fixtures}) {extra} :steps ({steps}))"#
    ))
    .expect("spec compiles")
}

fn chain_jobs(
    runner: &str,
    spot: (&str, &str),
    triage: (&str, &str),
    retry: (&str, &str),
    verdict: (&str, &str),
) -> Value {
    let job = |id: i64, name: &str, (status, conclusion): (&str, &str), runner: &str| {
        json!({"id": id, "name": name, "status": status,
               "conclusion": if conclusion.is_empty() { Value::Null } else { json!(conclusion) },
               "runner_name": runner})
    };
    json!({"total_count": 4, "jobs": [
        job(1, "suite (spot)", spot, runner),
        job(2, "suite (triage)", triage, "GitHub Actions 7"),
        job(3, "suite (retry)", retry, "ci-spot-x-runner-retry"),
        job(4, "suite", verdict, "GitHub Actions 8"),
    ]})
}

fn with_runner_pod() -> FakeCluster {
    let c = seeded();
    c.put(
        "stage",
        json!({"kind": "Pod", "metadata": {"name": "runner-abc", "uid": "pod-uid-1"}, "spec": {"nodeName": "ip-10-0-1-2.ec2.internal"}}),
    );
    *c.run_views.borrow_mut() = vec![view(
        "in_progress",
        json!([{"name": "suite (spot)", "status": "in_progress", "conclusion": null,
                "steps": [{"name": "Check out code", "status": "completed", "conclusion": "success"}]}]),
    )];
    *c.cancel_works.borrow_mut() = true;
    c
}

fn running() -> Value {
    chain_jobs(
        "runner-abc",
        ("in_progress", ""),
        ("queued", ""),
        ("queued", ""),
        ("queued", ""),
    )
}

fn has_pod(c: &FakeCluster, name: &str) -> bool {
    c.objects
        .borrow()
        .contains_key(&("stage".to_string(), "Pod".to_string(), name.to_string()))
}

#[test]
fn the_pod_deleted_is_exactly_the_runner_of_our_own_running_job() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![running()];
    let s = chain_spec(
        &format!("{CI} {LOSE_RUNNER}"),
        "",
        r#"(:place "ci") (:place "lose-runner")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    let deletes = calls_with(&c, "delete pod");
    assert_eq!(deletes.len(), 1, "one pod deleted");
    assert!(
        c.calls.borrow()[deletes[0]]
            .iter()
            .any(|a| a == "runner-abc"),
        "{:?}",
        c.calls.borrow()[deletes[0]]
    );
    assert!(!has_pod(&c, "runner-abc"), "the runner pod is gone");
    assert!(has_pod(&c, "svc-1"), "every other pod is untouched");
}

#[test]
fn a_job_that_is_not_running_is_refused_before_any_write() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![chain_jobs(
        "runner-abc",
        ("completed", "success"),
        ("queued", ""),
        ("queued", ""),
        ("queued", ""),
    )];
    let s = chain_spec(
        &format!("{CI} {LOSE_RUNNER}"),
        "",
        r#"(:place "ci") (:place "lose-runner")"#,
    );
    let (r, _d) = run_on(&c, &s);
    let why = r.aborted.as_deref().unwrap_or("");
    assert!(
        why.contains("refused, nothing written")
            && why.contains("only the pod of our own running job"),
        "{why}"
    );
    assert!(calls_with(&c, "delete pod").is_empty(), "nothing deleted");
    assert!(has_pod(&c, "runner-abc"));
}

#[test]
fn a_wait_polls_until_the_job_is_running_then_deletes_its_pod() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![
        chain_jobs(
            "",
            ("queued", ""),
            ("queued", ""),
            ("queued", ""),
            ("queued", ""),
        ),
        running(),
    ];
    let waiting = LOSE_RUNNER.replace(
        r#":job "suite (spot)""#,
        r#":job "suite (spot)" :wait "5s""#,
    );
    let s = chain_spec(
        &format!("{CI} {waiting}"),
        "",
        r#"(:place "ci") (:place "lose-runner")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert!(
        calls_with(&c, "actions/runs/424242/jobs").len() >= 2,
        "polled for the job"
    );
    assert!(!has_pod(&c, "runner-abc"), "the runner pod is gone");
}

#[test]
fn a_wait_that_expires_refuses_and_deletes_nothing() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![chain_jobs(
        "",
        ("queued", ""),
        ("queued", ""),
        ("queued", ""),
        ("queued", ""),
    )];
    let waiting = LOSE_RUNNER.replace(
        r#":job "suite (spot)""#,
        r#":job "suite (spot)" :wait "0s""#,
    );
    let s = chain_spec(
        &format!("{CI} {waiting}"),
        "",
        r#"(:place "ci") (:place "lose-runner")"#,
    );
    let (r, _d) = run_on(&c, &s);
    let why = r.aborted.as_deref().unwrap_or("");
    assert!(
        why.contains("refused, nothing written") && why.contains("over 0s"),
        "{why}"
    );
    assert!(calls_with(&c, "delete pod").is_empty());
    assert!(has_pod(&c, "runner-abc"));
}

#[test]
fn a_wait_that_is_not_a_duration_is_refused_at_load() {
    let e = refusal(
        &format!(
            "{CI} {}",
            LOSE_RUNNER.replace(
                r#":job "suite (spot)""#,
                r#":job "suite (spot)" :wait "soon""#
            )
        ),
        "",
    );
    assert!(e.contains("`soon` is not a duration"), "{e}");
}

#[test]
fn a_runner_that_is_not_a_pod_in_the_namespace_is_refused_before_any_write() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![chain_jobs(
        "runner-elsewhere",
        ("in_progress", ""),
        ("queued", ""),
        ("queued", ""),
        ("queued", ""),
    )];
    let s = chain_spec(
        &format!("{CI} {LOSE_RUNNER}"),
        "",
        r#"(:place "ci") (:place "lose-runner")"#,
    );
    let (r, _d) = run_on(&c, &s);
    let why = r.aborted.as_deref().unwrap_or("");
    assert!(
        why.contains("refused, nothing written") && why.contains("runner-elsewhere"),
        "{why}"
    );
    assert!(calls_with(&c, "delete pod").is_empty(), "nothing deleted");
}

fn chain_verdict(c: &FakeCluster, timeout: &str) -> Verdict {
    let s = chain_spec(
        &format!("{CI} {LOSE_RUNNER}"),
        &chain_assertion(timeout),
        r#"(:place "ci") (:place "lose-runner") (:observe "chain")"#,
    );
    let (r, _d) = run_on(c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    r.results
        .iter()
        .find(|x| x.assertion_id == "chain")
        .unwrap()
        .verdict
        .clone()
}

#[test]
fn a_recovered_chain_holds() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![
        running(),
        chain_jobs(
            "runner-abc",
            ("completed", "failure"),
            ("completed", "success"),
            ("in_progress", ""),
            ("queued", ""),
        ),
        chain_jobs(
            "runner-abc",
            ("completed", "failure"),
            ("completed", "success"),
            ("completed", "success"),
            ("completed", "success"),
        ),
    ];
    c.job_logs.borrow_mut().insert(
        "1".into(),
        format!("2026-10-21T10:20:00Z ##[error]{SHUTDOWN}. This can happen when ...\n"),
    );
    let v = chain_verdict(&c, "5s");
    let Verdict::Held { subjects, .. } = &v else {
        panic!("{v:?}")
    };
    assert_eq!(subjects.iter().count(), 4, "{subjects:?}");
    assert!(
        subjects
            .iter()
            .any(|o| o.as_str() == "job `suite`: completed success on runner GitHub Actions 8"),
        "{subjects:?}"
    );
}

#[test]
fn a_failed_verdict_is_falsified_with_the_job_named() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![
        running(),
        chain_jobs(
            "runner-abc",
            ("completed", "failure"),
            ("completed", "success"),
            ("completed", "success"),
            ("completed", "failure"),
        ),
    ];
    c.job_logs
        .borrow_mut()
        .insert("1".into(), format!("##[error]{SHUTDOWN}\n"));
    let v = chain_verdict(&c, "5s");
    let Verdict::Falsified { findings, .. } = &v else {
        panic!("{v:?}")
    };
    assert!(
        findings
            .iter()
            .any(|o| o.as_str() == "job `suite`: concluded failure, wanted success"),
        "{findings:?}"
    );
}

#[test]
fn a_spot_failure_without_the_shutdown_text_is_falsified() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![
        running(),
        chain_jobs(
            "runner-abc",
            ("completed", "failure"),
            ("completed", "success"),
            ("completed", "success"),
            ("completed", "success"),
        ),
    ];
    c.job_logs
        .borrow_mut()
        .insert("1".into(), "--- FAIL: TestSomething\n".into());
    let v = chain_verdict(&c, "5s");
    let Verdict::Falsified { findings, .. } = &v else {
        panic!("{v:?}")
    };
    assert!(
        findings
            .iter()
            .any(|o| o.as_str().contains("log does not contain")),
        "{findings:?}"
    );
}

#[test]
fn a_chain_still_running_at_the_timeout_is_falsified_not_passed() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![running()];
    let v = chain_verdict(&c, "0s");
    let Verdict::Falsified { findings, .. } = &v else {
        panic!("{v:?}")
    };
    assert!(
        findings
            .iter()
            .any(|o| o.as_str().contains("still in_progress after 0s")),
        "{findings:?}"
    );
}

#[test]
fn an_unreadable_log_is_skipped_not_passed() {
    let c = with_runner_pod();
    *c.run_jobs.borrow_mut() = vec![
        running(),
        chain_jobs(
            "runner-abc",
            ("completed", "failure"),
            ("completed", "success"),
            ("completed", "success"),
            ("completed", "success"),
        ),
    ];
    let v = chain_verdict(&c, "5s");
    let Verdict::Skipped { reason } = &v else {
        panic!("{v:?}")
    };
    assert!(
        reason.contains("unobserved (blind)") && reason.contains("could not be read"),
        "{reason}"
    );
}

#[test]
fn delete_runner_pod_needs_a_transient_cleanup_and_a_dispatch_to_belong_to() {
    let e = refusal(
        &LOSE_RUNNER.replace(
            r#"(:transient (:reason "an ephemeral runner pod; its controller replaces it"))"#,
            "restore-from-snapshot",
        ),
        "",
    );
    assert!(e.contains("has no inverse"), "{e}");
    let e = refusal(
        &format!(
            "{PIN} {}",
            LOSE_RUNNER.replace(r#":run-of "ci""#, r#":run-of "pin""#)
        ),
        "",
    );
    assert!(e.contains("is not a dispatch-workflow fixture"), "{e}");
}

#[test]
fn workflow_jobs_refuses_what_it_could_not_judge() {
    let e = refusal(
        CI,
        r#":assertions ((:id "w" :kind WorkflowJobs :target "p" :run-of "ci"))"#,
    );
    assert!(e.contains("missing :jobs"), "{e}");
    let e = refusal(
        CI,
        r#":assertions ((:id "w" :kind WorkflowJobs :target "p" :run-of "ci" :jobs ((:job "x"))))"#,
    );
    assert!(e.contains("expects nothing"), "{e}");
    let e = refusal(
        CI,
        r#":assertions ((:id "w" :kind WorkflowJobs :target "p" :run-of "ci" :jobs ((:job "x" :conclusion "green"))))"#,
    );
    assert!(e.contains("is not a job conclusion"), "{e}");
    let e = refusal(
        CI,
        r#":assertions ((:id "w" :kind WorkflowJobs :target "p" :run-of "nope" :jobs ((:job "x" :conclusion "success"))))"#,
    );
    assert!(e.contains("is not a dispatch-workflow fixture"), "{e}");
    assert!(
        !e.contains("missing :context"),
        "a run is read through GitHub, not a cluster: {e}"
    );
}

// ── the shipped runner-loss qualifications, rehearsed end to end ──────────

fn shipped(name: &str) -> VisitaSpec {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../specs");
    let src =
        std::fs::read_to_string(root.join(format!("qual-runnerloss-{name}.visita.lisp"))).unwrap();
    let src = src
        .replace(r#":interval "60s""#, r#":interval "0s""#)
        .replace(r#":poll "30s""#, r#":poll "0s""#);
    let mut s = load_one::<VisitaSpec>(&src).unwrap();
    s.fill_target_context("stage-ctx");
    s
}

fn rehearsal_cluster() -> FakeCluster {
    let c = FakeCluster::default();
    c.put("argocd", json!({"kind": "Application", "metadata": {"name": "ci-spot-runner-scale-set-x"}, "spec": {"project": "ci"}}));
    c.put("ci-spot", json!({"kind": "AutoscalingRunnerSet", "metadata": {"name": "ci-spot"}, "spec": {"minRunners": 0}}));
    c.put(
        "argocd",
        json!({"kind": "Secret", "metadata": {"name": "cluster-x", "labels": {"argocd.argoproj.io/secret-type": "cluster", "runner-scale-set": "master"}}, "data": {}}),
    );
    c.put(
        "ci-spot",
        json!({"kind": "Pod", "metadata": {"name": "runner-abc", "uid": "pod-uid-1"}, "spec": {"nodeName": "ip-10-0-1-2.ec2.internal"}}),
    );
    *c.run_views.borrow_mut() = vec![view(
        "in_progress",
        json!([{"name": "suite (spot)", "status": "in_progress", "conclusion": null,
                "steps": [{"name": "Check out code", "status": "completed", "conclusion": "success"}]}]),
    )];
    *c.cancel_works.borrow_mut() = true;
    c
}

fn six(runner: &str, states: [(&str, &str); 6]) -> Value {
    let names = [
        "suite (spot)",
        "suite (triage)",
        "suite (retry)",
        "suite (retry triage)",
        "suite (on-demand escalation)",
        "suite",
    ];
    let jobs: Vec<Value> = names
        .iter()
        .zip(states)
        .enumerate()
        .map(|(i, (n, (status, conclusion)))| {
            json!({"id": i64::try_from(i).unwrap() + 1, "name": n, "status": status,
                   "conclusion": if conclusion.is_empty() { Value::Null } else { json!(conclusion) },
                   "runner_name": if i == 0 { runner } else { "GitHub Actions 3" }})
        })
        .collect();
    json!({"total_count": 6, "jobs": jobs})
}

fn held_chain(c: &FakeCluster, spec: &str, results: Vec<Value>) -> ensaio::visita::VisitaReport {
    *c.run_jobs.borrow_mut() = results;
    let s = shipped(spec);
    let (r, _d) = run_on(c, &s);
    r
}

#[test]
fn rehearsal_recovers_a_lost_runner_and_leaves_the_cluster_as_it_was() {
    let c = rehearsal_cluster();
    c.job_logs
        .borrow_mut()
        .insert("1".into(), format!("##[error]{SHUTDOWN}\n"));
    let live = ("in_progress", "");
    let done = |x: &'static str| ("completed", x);
    let r = held_chain(
        &c,
        "recovers",
        vec![
            six(
                "runner-abc",
                [
                    live,
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                ],
            ),
            six(
                "runner-abc",
                [
                    done("failure"),
                    done("success"),
                    live,
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                ],
            ),
            six(
                "runner-abc",
                [
                    done("failure"),
                    done("success"),
                    done("success"),
                    done("success"),
                    done("skipped"),
                    done("success"),
                ],
            ),
        ],
    );
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    let v = &r
        .results
        .iter()
        .find(|x| x.assertion_id == "chain")
        .unwrap()
        .verdict;
    assert!(matches!(v, Verdict::Held { .. }), "{v:?}");
    assert_eq!(
        calls_with(&c, "delete pod").len(),
        1,
        "exactly the one runner pod"
    );
    assert!(!has_pod_in(&c, "ci-spot", "runner-abc"));
    assert!(
        r.restore.is_empty(),
        "the natural state came back: {:?}",
        r.restore
    );
    assert_eq!(r.receipt.verdict, CertVerdict::Certified, "{:?}", r.results);
}

#[test]
fn rehearsal_switch_off_keeps_the_check_red_and_names_nothing_retried() {
    let c = rehearsal_cluster();
    c.job_logs
        .borrow_mut()
        .insert("1".into(), format!("##[error]{SHUTDOWN}\n"));
    let live = ("in_progress", "");
    let done = |x: &'static str| ("completed", x);
    let r = held_chain(
        &c,
        "off",
        vec![
            six(
                "runner-abc",
                [
                    live,
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                ],
            ),
            six(
                "runner-abc",
                [
                    done("failure"),
                    done("success"),
                    done("skipped"),
                    done("skipped"),
                    done("skipped"),
                    done("failure"),
                ],
            ),
        ],
    );
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    let v = &r
        .results
        .iter()
        .find(|x| x.assertion_id == "chain")
        .unwrap()
        .verdict;
    assert!(matches!(v, Verdict::Held { .. }), "{v:?}");
    assert_eq!(r.receipt.verdict, CertVerdict::Certified, "{:?}", r.results);
}

#[test]
fn rehearsal_falsifies_when_the_retry_ran_with_the_switch_off() {
    let c = rehearsal_cluster();
    c.job_logs
        .borrow_mut()
        .insert("1".into(), format!("##[error]{SHUTDOWN}\n"));
    let live = ("in_progress", "");
    let done = |x: &'static str| ("completed", x);
    let r = held_chain(
        &c,
        "off",
        vec![
            six(
                "runner-abc",
                [
                    live,
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                    ("queued", ""),
                ],
            ),
            six(
                "runner-abc",
                [
                    done("failure"),
                    done("success"),
                    done("success"),
                    done("success"),
                    done("skipped"),
                    done("success"),
                ],
            ),
        ],
    );
    let v = &r
        .results
        .iter()
        .find(|x| x.assertion_id == "chain")
        .unwrap()
        .verdict;
    let Verdict::Falsified { findings, .. } = v else {
        panic!("{v:?}")
    };
    assert!(
        findings
            .iter()
            .any(|o| o.as_str() == "job `suite (retry)`: concluded success, wanted skipped"),
        "{findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|o| o.as_str() == "job `suite`: concluded success, wanted failure"),
        "{findings:?}"
    );
    assert_ne!(
        r.receipt.verdict,
        CertVerdict::Certified,
        "a falsified chain is not certified"
    );
}

#[test]
fn rehearsal_test_failure_touches_no_pod() {
    let c = rehearsal_cluster();
    let done = |x: &'static str| ("completed", x);
    let r = held_chain(
        &c,
        "test-failure",
        vec![six(
            "runner-abc",
            [
                done("failure"),
                done("success"),
                done("skipped"),
                done("skipped"),
                done("skipped"),
                done("failure"),
            ],
        )],
    );
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert!(
        calls_with(&c, "delete pod").is_empty(),
        "no runner is touched"
    );
    let v = &r
        .results
        .iter()
        .find(|x| x.assertion_id == "chain")
        .unwrap()
        .verdict;
    assert!(matches!(v, Verdict::Held { .. }), "{v:?}");
}

fn has_pod_in(c: &FakeCluster, ns: &str, name: &str) -> bool {
    c.objects
        .borrow()
        .contains_key(&(ns.to_string(), "Pod".to_string(), name.to_string()))
}

// ── GitHub rate limits and 5xx are waited out, never an abort that orphans a run ──

const RATE: &str =
    "failed to get run: HTTP 403: API rate limit exceeded for user ID 1. Please wait";

fn steps_text(d: &tempdir::Dir) -> String {
    std::fs::read_to_string(d.path().join("steps.jsonl")).unwrap_or_default()
}

#[test]
fn a_rate_limited_poll_waits_and_the_run_is_still_placed() {
    let c = seeded();
    *c.run_views.borrow_mut() = vec![view(
        "in_progress",
        deploy_job(("completed", "success"), ("completed", "success")),
    )];
    *c.view_failures.borrow_mut() = vec![RATE.into(), RATE.into(), "HTTP 502 Bad Gateway".into()];
    *c.cancel_works.borrow_mut() = true;
    let s = spec(
        &format!("{PIN} {}", dispatch("stop", "")),
        r#"(:place "pin") (:place "deploy")"#,
    );
    let (r, d) = run_on(&c, &s);
    assert!(r.aborted.is_none(), "{:?}", r.aborted);
    assert_eq!(
        steps_text(&d).matches("\"transient\"").count(),
        3,
        "three waits are on record"
    );
    assert!(r.restore.is_empty(), "{:?}", r.restore);
}

#[test]
fn a_rate_limited_cancel_is_retried_until_the_run_stops() {
    let c = seeded();
    *c.run_views.borrow_mut() = vec![view(
        "in_progress",
        deploy_job(("completed", "success"), ("in_progress", "")),
    )];
    *c.cancel_failures.borrow_mut() = vec![RATE.into(), RATE.into()];
    *c.cancel_works.borrow_mut() = true;
    let s = spec(
        &format!("{PIN} {}", dispatch("stop", "")),
        r#"(:place "pin") (:place "deploy")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(r.undo_errors.is_empty(), "{:?}", r.undo_errors);
    assert_eq!(
        calls_with(&c, "run cancel").len(),
        3,
        "two refused, one accepted"
    );
    assert!(r.restore.is_empty(), "{:?}", r.restore);
}

#[test]
fn a_rate_limit_that_outlasts_the_patience_is_an_error_not_a_pass() {
    let c = seeded();
    *c.run_views.borrow_mut() = vec![view(
        "in_progress",
        deploy_job(("completed", "success"), ("in_progress", "")),
    )];
    *c.view_failures.borrow_mut() = vec![RATE.into(); 10_000];
    let s = spec(
        &format!("{PIN} {}", dispatch("stop", "")),
        r#"(:place "pin") (:place "deploy")"#,
    );
    let (r, _d) = run_on(&c, &s);
    assert!(
        r.aborted
            .as_deref()
            .unwrap_or("")
            .contains("API rate limit exceeded"),
        "{:?}",
        r.aborted
    );
}

#[test]
fn a_rate_limited_job_read_is_not_a_blind_observation() {
    let c = with_runner_pod();
    *c.jobs_failures.borrow_mut() = vec![RATE.into(), RATE.into()];
    *c.run_jobs.borrow_mut() = vec![
        running(),
        chain_jobs(
            "runner-abc",
            ("completed", "failure"),
            ("completed", "success"),
            ("completed", "success"),
            ("completed", "success"),
        ),
    ];
    c.job_logs
        .borrow_mut()
        .insert("1".into(), format!("##[error]{SHUTDOWN}\n"));
    let v = chain_verdict(&c, "5s");
    assert!(
        matches!(v, Verdict::Held { .. }),
        "a rate limit is waited out, not skipped: {v:?}"
    );
}

#[test]
fn only_rate_limits_and_gateway_errors_are_transient() {
    assert!(ensaio::visita::env::transient("HTTP 502 Bad Gateway"));
    assert!(ensaio::visita::env::transient(
        "You have exceeded a secondary rate limit"
    ));
    assert!(!ensaio::visita::env::transient(
        "HTTP 403: Resource not accessible by integration"
    ));
    assert!(!ensaio::visita::env::transient("HTTP 404: Not Found"));
}
