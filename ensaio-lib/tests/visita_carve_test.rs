use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use ensaio::visita::cli::prepare;
use ensaio::visita::command::{Evidence, Exec, Operator, Runner};
use ensaio::visita::config::{FixtureClass, VisitaConfig};
use ensaio::visita::lease::{self, LeaseRequest, RunPlan};
use ensaio::visita::tier::TierRefusal;
use ensaio::visita::{OperatorEnvironment, VisitaSpec, run, snapshot};
use ensaio::{CertVerdict, load_one};
use serde_json::{Value, json};

#[derive(Default)]
struct Cluster {
    objects: RefCell<BTreeMap<(String, String, String), Value>>,
    calls: RefCell<Vec<Vec<String>>>,
    uids: RefCell<u32>,
    deletes_ignored: RefCell<bool>,
}

fn kind_of(resource: &str) -> (&'static str, bool) {
    match resource {
        "leases.coordination.k8s.io" | "lease.v1.coordination.k8s.io" | "lease" => ("Lease", false),
        "namespace" | "namespaces" => ("Namespace", true),
        "node" | "nodes" => ("Node", true),
        "resourcequota" => ("ResourceQuota", false),
        "limitrange" => ("LimitRange", false),
        "networkpolicy.v1.networking.k8s.io" | "networkpolicies" => ("NetworkPolicy", false),
        "endpoints" => ("Endpoints", false),
        "service" => ("Service", false),
        "applications.argoproj.io" => ("Application", false),
        "applicationsets.argoproj.io" => ("ApplicationSet", false),
        "configmap" | "configmaps" => ("ConfigMap", false),
        other => panic!("fake cluster: unknown resource {other}"),
    }
}

fn cluster_scoped(kind: &str) -> bool {
    matches!(kind, "Namespace" | "Node")
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

fn selects(o: &Value, sel: &str) -> bool {
    sel.split(',').all(|term| match term.split_once('=') {
        Some((k, v)) => o["metadata"]["labels"][k].as_str() == Some(v),
        None => o["metadata"]["labels"].get(term).is_some(),
    })
}

impl Cluster {
    fn put(&self, ns: &str, mut obj: Value) {
        let kind = obj["kind"].as_str().unwrap().to_string();
        let name = obj["metadata"]["name"].as_str().unwrap().to_string();
        let ns = if cluster_scoped(&kind) {
            String::new()
        } else {
            ns.to_string()
        };
        if obj["apiVersion"].is_null() {
            obj["apiVersion"] = json!("v1");
        }
        if obj["metadata"]["uid"].is_null() {
            *self.uids.borrow_mut() += 1;
            obj["metadata"]["uid"] = json!(format!("uid-{}", self.uids.borrow()));
        }
        self.objects.borrow_mut().insert((ns, kind, name), obj);
    }

    fn get(&self, ns: &str, kind: &str, name: &str) -> Option<Value> {
        let ns = if cluster_scoped(kind) { "" } else { ns };
        self.objects
            .borrow()
            .get(&(ns.to_string(), kind.to_string(), name.to_string()))
            .cloned()
    }

    fn count(&self, kind: &str) -> usize {
        self.objects
            .borrow()
            .keys()
            .filter(|(_, k, _)| k == kind)
            .count()
    }

    fn writes(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|a| {
                a.iter()
                    .any(|x| ["create", "delete", "patch", "annotate"].contains(&x.as_str()))
            })
            .count()
    }
}

impl Runner for Cluster {
    fn run(&self, argv: &[String]) -> Exec {
        self.calls.borrow_mut().push(argv.to_vec());
        let ok = |s: String| Exec {
            status: 0,
            stdout: s,
            stderr: String::new(),
        };
        let notfound = |what: &str| Exec {
            status: 1,
            stdout: String::new(),
            stderr: format!("Error from server (NotFound): \"{what}\" not found"),
        };
        assert_eq!(argv[1], "--context");
        let ns = argv[4].clone();
        match argv[5].as_str() {
            "get" if argv[7] != "-o" => {
                let (kind, cs) = kind_of(&argv[6]);
                let ns = if cs { String::new() } else { ns };
                match self
                    .objects
                    .borrow()
                    .get(&(ns, kind.into(), argv[7].clone()))
                {
                    Some(o) => ok(o.to_string()),
                    None => notfound(&argv[7]),
                }
            }
            "get" => {
                let kinds: Vec<(&str, bool)> = argv[6].split(',').map(kind_of).collect();
                let sel = argv
                    .iter()
                    .position(|a| a == "-l")
                    .map(|i| argv[i + 1].clone());
                let items: Vec<Value> = self
                    .objects
                    .borrow()
                    .iter()
                    .filter(|((n, k, _), _)| {
                        kinds.iter().any(|(kk, cs)| kk == k && (*cs || *n == ns))
                    })
                    .filter(|(_, o)| sel.as_deref().is_none_or(|s| selects(o, s)))
                    .map(|(_, o)| o.clone())
                    .collect();
                ok(json!({"kind": "List", "items": items}).to_string())
            }
            "create" => {
                let list: Value =
                    serde_json::from_str(&std::fs::read_to_string(&argv[7]).unwrap()).unwrap();
                let mut made = Vec::new();
                for o in list["items"].as_array().unwrap() {
                    let kind = o["kind"].as_str().unwrap();
                    let name = o["metadata"]["name"].as_str().unwrap();
                    let at = o["metadata"]["namespace"]
                        .as_str()
                        .map_or(ns.clone(), str::to_string);
                    if self.get(&at, kind, name).is_some() {
                        return Exec {
                            status: 1,
                            stdout: String::new(),
                            stderr: format!(
                                "Error from server (AlreadyExists): {kind} \"{name}\" already exists"
                            ),
                        };
                    }
                    self.put(&at, o.clone());
                    made.push(self.get(&at, kind, name).unwrap());
                }
                ok(json!({"kind": "List", "items": made}).to_string())
            }
            "delete" => {
                if *self.deletes_ignored.borrow() {
                    return ok(String::new());
                }
                let (kind, cs) = kind_of(&argv[6]);
                let key = (
                    if cs { String::new() } else { ns },
                    kind.to_string(),
                    argv[7].clone(),
                );
                let gone = self.objects.borrow_mut().remove(&key);
                if kind == "Namespace" {
                    self.objects
                        .borrow_mut()
                        .retain(|(n, _, _), _| n != &argv[7]);
                }
                match gone {
                    Some(_) => ok(String::new()),
                    None => notfound(&argv[7]),
                }
            }
            "patch" => {
                assert_eq!(argv[8..11], ["--type", "merge", "-p"]);
                let (kind, cs) = kind_of(&argv[6]);
                let key = (
                    if cs { String::new() } else { ns },
                    kind.to_string(),
                    argv[7].clone(),
                );
                let patch: Value = serde_json::from_str(&argv[11]).unwrap();
                let mut objs = self.objects.borrow_mut();
                match objs.get_mut(&key) {
                    Some(o) => {
                        merge(o, &patch);
                        ok(String::new())
                    }
                    None => notfound(&argv[7]),
                }
            }
            "annotate" => {
                let (kind, _) = kind_of(&argv[7]);
                let mut objs = self.objects.borrow_mut();
                let o = objs.get_mut(&(ns, kind.into(), argv[8].clone())).unwrap();
                let kv = &argv[9];
                if let Some(k) = kv.strip_suffix('-') {
                    if let Some(a) = o["metadata"]["annotations"].as_object_mut() {
                        a.remove(k);
                    }
                } else {
                    let (k, v) = kv.split_once('=').unwrap();
                    merge(&mut o["metadata"], &json!({"annotations": {k: v}}));
                }
                ok(String::new())
            }
            other => panic!("fake cluster: verb {other}"),
        }
    }
}

fn seeded() -> Cluster {
    let c = Cluster::default();
    c.put(
        "",
        json!({"kind": "Namespace", "metadata": {"name": "lab"}}),
    );
    c.put(
        "lab",
        json!({"kind": "ConfigMap", "metadata": {"name": "anchor"}, "data": {"k": "v"}}),
    );
    c.put(
        "default",
        json!({"kind": "Endpoints", "metadata": {"name": "kubernetes"},
               "subsets": [{"addresses": [{"ip": "10.0.0.1"}], "ports": [{"port": 6443}]}]}),
    );
    c.put(
        "web",
        json!({"kind": "Service", "metadata": {"name": "api", "namespace": "web"},
               "spec": {"selector": {"app": "api"}, "ports": [{"port": 80, "targetPort": 8080, "protocol": "TCP"}]}}),
    );
    c.put(
        "",
        json!({"kind": "Node", "metadata": {"name": "node-b", "labels": {"pool": "lab"}},
               "spec": {"taints": [{"key": "other", "effect": "NoSchedule"}]}}),
    );
    c.put("", json!({"kind": "Node", "metadata": {"name": "node-a", "labels": {"pool": "lab"}}, "spec": {}}));
    c.put(
        "argocd",
        json!({"kind": "Application", "metadata": {"name": "plain"},
               "spec": {"syncPolicy": {"automated": {"prune": true, "selfHeal": true}, "syncOptions": ["CreateNamespace=true"]}}}),
    );
    c.put(
        "argocd",
        json!({"kind": "Application", "metadata": {"name": "owned",
               "ownerReferences": [{"kind": "ApplicationSet", "name": "fleet"}]},
               "spec": {"syncPolicy": {"automated": {}}}}),
    );
    c.put(
        "argocd",
        json!({"kind": "ApplicationSet", "metadata": {"name": "fleet"}, "spec": {}}),
    );
    c
}

const NATURAL: &str = r#":natural-state ((:context "lab-ctx" :namespace "lab" :kinds ("configmaps") :names ("anchor"))
                  (:context "lab-ctx" :namespace "default" :kinds ("namespaces") :selector "ensaio.pleme.io/run")
                  (:context "lab-ctx" :namespace "lab" :kinds ("networkpolicies"))
                  (:context "lab-ctx" :namespace "default" :kinds ("nodes"))
                  (:context "lab-ctx" :namespace "argocd" :kinds ("applications.argoproj.io")))"#;

fn spec(fixtures: &str, steps: &str) -> VisitaSpec {
    load_one::<VisitaSpec>(&format!(
        r#"(defvisita :name "carve" :claim "c" {NATURAL} :fixtures ({fixtures}) :steps ({steps}))"#
    ))
    .unwrap()
}

const CARVE_NS: &str = r#"(:name "ns" :effect create :cleanup own-inverse
  :action (:carve-namespace (:context "lab-ctx" :name "carved"
           :quota ((:name "pods" :value "10"))
           :limit-default ((:name "cpu" :value "500m"))
           :limit-default-request ((:name "cpu" :value "100m")) :timeout "5s")))"#;

const FENCE: &str = r#"(:name "fence" :effect create :cleanup own-inverse
  :action (:fence (:context "lab-ctx" :namespace "lab"
           :egress (dns api-server (:service (:namespace "web" :name "api")))
           :ingress ((:namespace "web")) :timeout "5s")))"#;

const NODE: &str = r#"(:name "node" :effect mutate :cleanup own-inverse
  :action (:carve-node (:context "lab-ctx" :selector "pool=lab"
           :taint-key "ensaio.pleme.io/carved" :taint-value "lab" :label-key "ensaio.pleme.io/carved" :label-value "lab")))"#;

fn pause(app: &str) -> String {
    format!(
        r#"(:name "pause" :effect mutate :cleanup own-inverse
  :action (:argocd-pause (:context "lab-ctx" :namespace "argocd" :application "{app}")))"#
    )
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ensaio-carve-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_on(c: &Cluster, s: &VisitaSpec, tag: &str) -> ensaio::visita::VisitaReport {
    let dir = scratch(tag);
    let ev = Evidence::open(&dir).unwrap();
    let env = OperatorEnvironment::new(Operator::new(c, &ev, None)).with_run("carve-run");
    let r = run(s, &env).unwrap();
    s.validate(&["lab-ctx".into()]).unwrap();
    r
}

#[test]
fn carve_namespace_creates_a_labelled_quota_bound_namespace_and_takes_it_back() {
    let c = seeded();
    let s = spec(CARVE_NS, r#"(:place "ns")"#);
    s.validate(&["lab-ctx".into()]).unwrap();
    let dir = scratch("ns");
    let ev = Evidence::open(&dir).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None)).with_run("carve-run");
    let r = run(&s, &env).unwrap();
    assert_eq!(
        r.receipt.verdict,
        CertVerdict::Certified,
        "{:#?}",
        r.restore
    );
    let created = std::fs::read_to_string(dir.join("steps.jsonl")).unwrap();
    assert!(
        created.contains("\"kind\":\"ResourceQuota\"")
            && created.contains("\"kind\":\"LimitRange\"")
    );
    assert!(c.get("", "Namespace", "carved").is_none());
    assert_eq!(c.count("ResourceQuota"), 0);
}

#[test]
fn carve_namespace_is_refused_before_any_write_when_the_namespace_exists() {
    let c = seeded();
    c.put(
        "",
        json!({"kind": "Namespace", "metadata": {"name": "carved"}}),
    );
    let r = run_on(&c, &spec(CARVE_NS, r#"(:place "ns")"#), "ns-exists");
    assert!(
        r.aborted.as_deref().unwrap().contains("already exist"),
        "{:?}",
        r.aborted
    );
    assert_eq!(c.writes(), 0);
    assert!(c.get("", "Namespace", "carved").is_some());
}

#[test]
fn negative_control_a_namespace_left_behind_is_caught_by_the_diff() {
    let c = seeded();
    *c.deletes_ignored.borrow_mut() = true;
    let r = run_on(&c, &spec(CARVE_NS, r#"(:place "ns")"#), "ns-left");
    assert_ne!(r.receipt.verdict, CertVerdict::Certified);
    assert!(
        r.restore
            .extra
            .iter()
            .any(|e| e.contains("Namespace/carved")),
        "{:?}",
        r.restore.extra
    );
}

#[test]
fn fence_renders_default_deny_plus_declared_peers_and_removes_them() {
    let c = seeded();
    let s = spec(FENCE, r#"(:place "fence")"#);
    let dir = scratch("fence");
    let ev = Evidence::open(&dir).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None)).with_run("carve-run");
    let fixture = &s.fixtures[0];
    ensaio::EnsaioEnvironment::place_fixture(&env, fixture).unwrap();
    let deny = c.get("lab", "NetworkPolicy", "ensaio-fence-deny").unwrap();
    assert_eq!(deny["spec"]["policyTypes"], json!(["Ingress", "Egress"]));
    let egress = c
        .get("lab", "NetworkPolicy", "ensaio-fence-egress")
        .unwrap();
    let rules = egress["spec"]["egress"].as_array().unwrap();
    assert_eq!(rules.len(), 3);
    assert_eq!(rules[1]["to"][0]["ipBlock"]["cidr"], "10.0.0.1/32");
    assert_eq!(rules[1]["ports"][0]["port"], 6443);
    assert_eq!(
        rules[2]["to"][0]["podSelector"]["matchLabels"]["app"],
        "api"
    );
    assert_eq!(rules[2]["ports"][0]["port"], 8080);
    let ingress = c
        .get("lab", "NetworkPolicy", "ensaio-fence-ingress")
        .unwrap();
    assert_eq!(
        ingress["spec"]["ingress"][0]["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
        "web"
    );
    ensaio::EnsaioEnvironment::clean_fixture(&env, fixture).unwrap();
    assert_eq!(c.count("NetworkPolicy"), 0);
    let r = run_on(&c, &s, "fence-run");
    assert_eq!(
        r.receipt.verdict,
        CertVerdict::Certified,
        "{:#?}",
        r.restore
    );
}

#[test]
fn fence_refuses_a_service_without_a_selector_before_any_write() {
    let c = seeded();
    c.put(
        "web",
        json!({"kind": "Service", "metadata": {"name": "api", "namespace": "web"}, "spec": {}}),
    );
    let r = run_on(&c, &spec(FENCE, r#"(:place "fence")"#), "fence-nosel");
    assert!(r.aborted.as_deref().unwrap().contains("no pod selector"));
    assert_eq!(c.writes(), 0);
}

#[test]
fn carve_node_adds_exactly_one_noschedule_taint_and_label_and_removes_only_those() {
    let c = seeded();
    let s = spec(NODE, r#"(:place "node")"#);
    let dir = scratch("node");
    let ev = Evidence::open(&dir).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None));
    let f = &s.fixtures[0];
    ensaio::EnsaioEnvironment::place_fixture(&env, f).unwrap();
    let a = c.get("", "Node", "node-a").unwrap();
    assert_eq!(
        a["spec"]["taints"],
        json!([{"key": "ensaio.pleme.io/carved", "value": "lab", "effect": "NoSchedule"}])
    );
    assert_eq!(a["metadata"]["labels"]["ensaio.pleme.io/carved"], "lab");
    assert!(
        c.get("", "Node", "node-b").unwrap()["metadata"]["labels"]
            .get("ensaio.pleme.io/carved")
            .is_none()
    );
    assert!(
        !c.calls
            .borrow()
            .iter()
            .any(|a| a.iter().any(|x| x == "cordon" || x == "drain"))
    );
    ensaio::EnsaioEnvironment::clean_fixture(&env, f).unwrap();
    let a = c.get("", "Node", "node-a").unwrap();
    assert!(a["spec"].get("taints").is_none());
    assert_eq!(a["metadata"]["labels"], json!({"pool": "lab"}));
    let r = run_on(&c, &s, "node-run");
    assert_eq!(
        r.receipt.verdict,
        CertVerdict::Certified,
        "{:#?}",
        r.restore
    );
}

#[test]
fn carve_node_keeps_taints_it_did_not_add() {
    let c = seeded();
    let n = c.get("", "Node", "node-a").unwrap();
    let mut n = n;
    n["spec"]["taints"] = json!([{"key": "keep", "effect": "NoExecute"}]);
    c.put("", n);
    let r = run_on(&c, &spec(NODE, r#"(:place "node")"#), "node-keep");
    assert_eq!(
        r.receipt.verdict,
        CertVerdict::Certified,
        "{:#?}",
        r.restore
    );
    assert_eq!(
        c.get("", "Node", "node-a").unwrap()["spec"]["taints"],
        json!([{"key": "keep", "effect": "NoExecute"}])
    );
}

#[test]
fn carve_node_refuses_a_node_already_carrying_the_key() {
    let c = seeded();
    let mut n = c.get("", "Node", "node-a").unwrap();
    n["metadata"]["labels"]["ensaio.pleme.io/carved"] = json!("someone");
    c.put("", n);
    let r = run_on(&c, &spec(NODE, r#"(:place "node")"#), "node-held");
    assert!(
        r.aborted
            .as_deref()
            .unwrap()
            .contains("already carries the label")
    );
    assert_eq!(c.writes(), 0);
}

#[test]
fn argocd_pause_removes_automated_and_restores_the_sync_policy_exactly() {
    let c = seeded();
    let s = spec(&pause("plain"), r#"(:place "pause")"#);
    let dir = scratch("pause");
    let ev = Evidence::open(&dir).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None));
    let f = &s.fixtures[0];
    let before = c.get("argocd", "Application", "plain").unwrap();
    ensaio::EnsaioEnvironment::place_fixture(&env, f).unwrap();
    let paused = c.get("argocd", "Application", "plain").unwrap();
    assert!(paused["spec"]["syncPolicy"].get("automated").is_none());
    assert_eq!(
        paused["spec"]["syncPolicy"]["syncOptions"],
        json!(["CreateNamespace=true"])
    );
    let mut drifted = paused;
    drifted["spec"]["syncPolicy"]["retry"] = json!({"limit": 2});
    c.put("argocd", drifted);
    ensaio::EnsaioEnvironment::clean_fixture(&env, f).unwrap();
    assert_eq!(
        c.get("argocd", "Application", "plain").unwrap()["spec"],
        before["spec"]
    );
    let r = run_on(&c, &s, "pause-run");
    assert_eq!(
        r.receipt.verdict,
        CertVerdict::Certified,
        "{:#?}",
        r.restore
    );
}

#[test]
fn argocd_pause_refuses_an_applicationset_app_whose_set_drops_the_annotation() {
    let c = seeded();
    let r = run_on(
        &c,
        &spec(&pause("owned"), r#"(:place "pause")"#),
        "pause-owned",
    );
    let why = r.aborted.unwrap();
    assert!(
        why.contains("ApplicationSet `fleet`") && why.contains("preservedFields"),
        "{why}"
    );
    assert_eq!(c.writes(), 0);
}

#[test]
fn argocd_pause_uses_skip_reconcile_when_the_set_preserves_it() {
    let c = seeded();
    c.put(
        "argocd",
        json!({"kind": "ApplicationSet", "metadata": {"name": "fleet"},
               "spec": {"preservedFields": {"annotations": ["argocd.argoproj.io/skip-reconcile"]}}}),
    );
    let s = spec(&pause("owned"), r#"(:place "pause")"#);
    let dir = scratch("pause-skip");
    let ev = Evidence::open(&dir).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None));
    let f = &s.fixtures[0];
    ensaio::EnsaioEnvironment::place_fixture(&env, f).unwrap();
    let app = c.get("argocd", "Application", "owned").unwrap();
    assert_eq!(
        app["metadata"]["annotations"]["argocd.argoproj.io/skip-reconcile"],
        "true"
    );
    assert_eq!(app["spec"]["syncPolicy"], json!({"automated": {}}));
    ensaio::EnsaioEnvironment::clean_fixture(&env, f).unwrap();
    let app = c.get("argocd", "Application", "owned").unwrap();
    assert!(
        app["metadata"]["annotations"]
            .get("argocd.argoproj.io/skip-reconcile")
            .is_none()
    );
}

#[test]
fn carve_fixtures_must_declare_their_own_inverse() {
    for (f, word) in [
        (
            NODE.replace("own-inverse", "restore-from-snapshot"),
            "carve-node",
        ),
        (
            CARVE_NS.replace("own-inverse", "(:transient (:reason \"x\"))"),
            "carve-namespace",
        ),
        (
            pause("plain").replace("own-inverse", "(:retained (:reason \"x\"))"),
            "argocd-pause",
        ),
    ] {
        let err = spec(&f, r#"(:place "x")"#)
            .validate(&["lab-ctx".into()])
            .unwrap_err();
        assert!(
            err.contains(&format!("{word} must declare :cleanup own-inverse")),
            "{err}"
        );
    }
    let bad = FENCE.replace(":ingress ((:namespace \"web\"))", ":ingress (api-server)");
    assert!(
        spec(&bad, "")
            .validate(&["lab-ctx".into()])
            .unwrap_err()
            .contains("api-server is an egress peer")
    );
    let bad = CARVE_NS.replace(":effect create", ":effect mutate");
    assert!(
        spec(&bad, "")
            .validate(&["lab-ctx".into()])
            .unwrap_err()
            .contains("declare :effect create")
    );
}

fn config(yaml: &str) -> VisitaConfig {
    let dir = scratch("cfg");
    let p = dir.join("visita.yaml");
    std::fs::write(&p, yaml).unwrap();
    VisitaConfig::from_layers(&[shikumi::ProgressiveLayer::try_from_config_override(&p).unwrap()])
        .unwrap()
}

const REGISTRY: &str = r"
targets:
  lab:
    context: lab-ctx
    tier: development
    namespace: lab
    carve:
      allow: [carve-namespace, fence, carve-node]
  prod:
    context: prod-ctx
    tier: production
    carve:
      allow: [annotate]
      requiresWindow: true
      windowTag: prod-window
";

#[test]
fn config_folds_prescribed_defaults_under_the_registry_file() {
    let c = config(REGISTRY);
    assert!(c.lease.enabled);
    assert_eq!(c.lease.ttl, "2h");
    assert_eq!(c.targets["lab"].carve.allow.len(), 3);
    assert_eq!(c.target_for_context("prod-ctx").unwrap().0, "prod");
    assert_eq!(c.lease_namespace(&c.targets["lab"]), "lab");
    c.check().unwrap();
    let dir = scratch("cfg2");
    let a = dir.join("a.yaml");
    let b = dir.join("b.yaml");
    std::fs::write(&a, REGISTRY).unwrap();
    std::fs::write(
        &b,
        "targets:\n  extra:\n    context: x\n    tier: staging\nlease:\n  ttl: 30m\n",
    )
    .unwrap();
    let merged = VisitaConfig::from_layers(&[
        shikumi::ProgressiveLayer::try_from_config_override(&a).unwrap(),
        shikumi::ProgressiveLayer::try_from_config_override(&b).unwrap(),
    ])
    .unwrap();
    assert_eq!(merged.targets.len(), 3);
    assert_eq!(merged.lease.ttl, "30m");
    assert!(merged.lease.enabled);
}

#[test]
fn config_refuses_unknown_keys_and_a_production_target_without_a_window() {
    let dir = scratch("cfg3");
    let p = dir.join("v.yaml");
    std::fs::write(&p, "targets:\n  t:\n    context: c\n    tier: production\n    carve:\n      allow: [annotate]\n").unwrap();
    let c = VisitaConfig::from_layers(&[
        shikumi::ProgressiveLayer::try_from_config_override(&p).unwrap()
    ])
    .unwrap();
    assert!(c.check().unwrap_err().contains("production"));
    std::fs::write(&p, "targetz: {}\n").unwrap();
    assert!(
        VisitaConfig::from_layers(&[
            shikumi::ProgressiveLayer::try_from_config_override(&p).unwrap()
        ])
        .is_err()
    );
}

#[test]
fn every_fixture_class_has_a_word_and_maps_from_its_action() {
    let words: Vec<String> = FixtureClass::ALL.iter().map(|c| c.word()).collect();
    assert!(words.iter().all(|w| !w.is_empty()));
    assert!(
        words.contains(&"carve-namespace".to_string())
            && words.contains(&"argocd-pause".to_string())
    );
    let s = spec(&format!("{CARVE_NS} {FENCE} {NODE} {}", pause("plain")), "");
    let classes: Vec<FixtureClass> = s
        .fixtures
        .iter()
        .map(|f| FixtureClass::of(f.action.as_ref().unwrap()))
        .collect();
    assert_eq!(
        classes,
        [
            FixtureClass::CarveNamespace,
            FixtureClass::Fence,
            FixtureClass::CarveNode,
            FixtureClass::ArgocdPause
        ]
    );
}

#[test]
fn tier_guard_refuses_a_class_the_target_does_not_allow_naming_target_tier_and_class() {
    let cfg = config(REGISTRY);
    let err = prepare(
        spec(&pause("plain"), ""),
        &cfg,
        &["lab-ctx".into()],
        &[],
        &[],
        0,
    )
    .err()
    .unwrap();
    assert!(
        err.contains("target `lab`")
            && err.contains("Development")
            && err.contains("`argocd-pause`"),
        "{err}"
    );
    let refusals = ensaio::visita::tier::check(&spec(&pause("plain"), ""), &cfg, &[], 0);
    assert!(matches!(
        &refusals[..],
        [TierRefusal::ClassNotAllowed {
            class: FixtureClass::ArgocdPause,
            ..
        }]
    ));
    assert!(prepare(spec(CARVE_NS, ""), &cfg, &["lab-ctx".into()], &[], &[], 0).is_ok());
}

#[test]
fn a_raw_context_outside_the_registry_keeps_working_unguarded() {
    let cfg = config(REGISTRY);
    let s = spec(&pause("plain").replace("lab-ctx", "other-ctx"), "");
    let l = prepare(
        s,
        &cfg,
        &["lab-ctx".into(), "other-ctx".into()],
        &[],
        &[],
        0,
    )
    .unwrap();
    assert_eq!(l.target.unwrap().0, "lab");
}

const PROD: &str = r#"(defvisita :name "p" :claim "c" :target "prod"
  :natural-state ((:namespace "ns" :kinds ("configmaps")))
  :fixtures ((:name "a" :effect mutate :cleanup restore-from-snapshot
              :action (:annotate (:namespace "ns" :resource "configmap" :name "x" :key "k" :value "v"))))
  :steps ((:place "a")))"#;

fn window(tag: &str, start: &str, end: &str) -> guardrail::model::ChangeWindow {
    serde_json::from_value(json!({"name": "w", "tag": tag, "start": start, "end": end})).unwrap()
}

#[test]
fn a_window_bound_target_writes_only_inside_an_open_window() {
    let cfg = config(REGISTRY);
    let at = guardrail::windows::parse_utc("2026-10-14T12:00:00Z").unwrap();
    let s = load_one::<VisitaSpec>(PROD).unwrap();
    let err = prepare(s.clone(), &cfg, &[], &["prod".into()], &[], at)
        .err()
        .unwrap();
    assert!(
        err.contains("requires a change window")
            && err.contains("prod-window")
            && err.contains("Production"),
        "{err}"
    );
    let shut = [window(
        "prod-window",
        "2026-10-14T00:00:00Z",
        "2026-10-14T06:00:00Z",
    )];
    assert!(prepare(s.clone(), &cfg, &[], &["prod".into()], &shut, at).is_err());
    let other = [window(
        "someone-else",
        "2026-10-14T00:00:00Z",
        "2026-10-15T00:00:00Z",
    )];
    assert!(prepare(s.clone(), &cfg, &[], &["prod".into()], &other, at).is_err());
    let open = [window(
        "prod-window",
        "2026-10-14T11:00:00Z",
        "2026-10-14T13:00:00Z",
    )];
    let l = prepare(s, &cfg, &[], &["prod".into()], &open, at).unwrap();
    assert_eq!(l.spec.natural_state[0].context, "prod-ctx");
    assert!(l.spec.contexts().iter().all(|c| c == "prod-ctx"));
}

#[test]
fn a_spec_target_must_be_named_on_the_command_line_and_known() {
    let cfg = config(REGISTRY);
    let s = load_one::<VisitaSpec>(PROD).unwrap();
    assert!(
        prepare(s.clone(), &cfg, &["prod-ctx".into()], &[], &[], 0)
            .err()
            .unwrap()
            .contains("pass --target prod")
    );
    let unknown =
        load_one::<VisitaSpec>(&PROD.replace(":target \"prod\"", ":target \"nope\"")).unwrap();
    assert!(
        prepare(unknown, &cfg, &[], &["prod".into()], &[], 0)
            .err()
            .unwrap()
            .contains("not in the visita target registry")
    );
    let none = load_one::<VisitaSpec>(&PROD.replace(":target \"prod\"", "")).unwrap();
    assert!(
        none.validate(&[])
            .unwrap_err()
            .contains(":context is empty")
    );
}

#[test]
fn window_files_are_read_from_the_guardrail_directory() {
    let dir = scratch("windows");
    std::fs::write(
        dir.join("jira.json"),
        r#"{"changeWindows":[{"name":"CHG-1","tag":"prod-window","start":"2026-10-14T11:00:00Z","end":"2026-10-14T13:00:00Z"}]}"#,
    )
    .unwrap();
    std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
    let ws = ensaio::visita::tier::open_windows(&dir);
    assert_eq!(ws.len(), 1);
    assert!(ensaio::visita::tier::open_windows(&dir.join("absent")).is_empty());
}

fn lease_req(s: &VisitaSpec, operator: &str, run: &str, ttl: Duration) -> LeaseRequest {
    LeaseRequest::new(s, "lab-ctx", "lab", operator, run, ttl)
}

#[test]
fn a_run_holds_a_visible_lease_and_releases_it_last() {
    let c = seeded();
    let s = spec(CARVE_NS, r#"(:place "ns")"#);
    let dir = scratch("lease");
    let ev = Evidence::open(&dir).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None))
        .with_run("carve-r1")
        .with_lease(lease_req(&s, "alice", "carve-r1", Duration::from_secs(600)));
    let r = run(&s, &env).unwrap();
    assert_eq!(
        r.receipt.verdict,
        CertVerdict::Certified,
        "{:#?}",
        r.restore
    );
    let calls = c.calls.borrow();
    let create_lease = calls
        .iter()
        .position(|a| a[5] == "create" && a[4] == "lab")
        .unwrap();
    let first_ns_write = calls
        .iter()
        .position(|a| a[5] == "create" && a[4] == "carved")
        .unwrap();
    assert!(create_lease < first_ns_write);
    let release = calls.iter().rposition(|a| a[5] == "delete").unwrap();
    assert!(calls[release][6].starts_with("lease"));
    assert_eq!(c.count("Lease"), 0);
}

#[test]
fn a_lease_carries_holder_expiry_and_scope() {
    let s = spec(CARVE_NS, "");
    let req = lease_req(&s, "alice", "carve-r1", Duration::from_secs(60));
    let now = chrono::Utc::now();
    let o = req.object(now);
    assert_eq!(o["spec"]["holderIdentity"], "alice/carve-r1");
    assert_eq!(lease::scope_of(&o), req.scope);
    assert!(req.scope.contains("ns/carved"));
    assert!(!lease::expired(&o, now));
    assert!(lease::expired(&o, now + chrono::Duration::seconds(61)));
    assert_eq!(
        lease::run_id("Qual Carve!", now).len(),
        "qual-carve-".len() + 16
    );
}

#[test]
fn another_unexpired_lease_over_the_same_selection_refuses_the_run_before_any_write() {
    let c = seeded();
    let s = spec(CARVE_NS, r#"(:place "ns")"#);
    let theirs =
        lease_req(&s, "bob", "theirs", Duration::from_secs(600)).object(chrono::Utc::now());
    c.put("lab", theirs);
    let dir = scratch("lease-clash");
    let ev = Evidence::open(&dir).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None)).with_lease(lease_req(
        &s,
        "alice",
        "mine",
        Duration::from_secs(600),
    ));
    let err = run(&s, &env).err().unwrap();
    assert!(err.contains("held by `bob/theirs`"), "{err}");
    assert_eq!(c.writes(), 0);
}

#[test]
fn an_expired_or_disjoint_lease_does_not_block() {
    let c = seeded();
    let s = spec(CARVE_NS, r#"(:place "ns")"#);
    let past = chrono::Utc::now() - chrono::Duration::hours(3);
    c.put(
        "lab",
        lease_req(&s, "bob", "old", Duration::from_secs(60)).object(past),
    );
    let other = spec(&FENCE.replace("\"lab\"", "\"elsewhere\""), "");
    let mut disjoint = lease_req(&other, "carol", "apart", Duration::from_secs(600));
    disjoint.scope = ["ns/elsewhere".to_string()].into_iter().collect();
    c.put("lab", disjoint.object(chrono::Utc::now()));
    let dir = scratch("lease-ok");
    let ev = Evidence::open(&dir).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None)).with_lease(lease_req(
        &s,
        "alice",
        "mine",
        Duration::from_secs(600),
    ));
    assert_eq!(
        run(&s, &env).unwrap().receipt.verdict,
        CertVerdict::Certified
    );
    assert_eq!(c.count("Lease"), 2);
}

#[test]
fn the_reaper_finishes_a_dead_run_from_its_persisted_plan_and_proves_the_restore() {
    let c = seeded();
    let s = spec(
        &format!("{CARVE_NS} {NODE}"),
        r#"(:place "ns") (:place "node")"#,
    );
    let state = scratch("reap");
    let run_dir = lease::run_dir(&state, "carve-dead");
    {
        let ev = Evidence::open(&run_dir).unwrap();
        let op = Operator::new(&c, &ev, None);
        let natural = snapshot::capture(&op, &s.natural_state, "natural-state").unwrap();
        ev.write("natural-state.json", &serde_json::to_vec(&natural).unwrap())
            .unwrap();
        let plan = RunPlan {
            run: "carve-dead".into(),
            operator: "alice".into(),
            target: Some("lab".into()),
            lease: None,
            spec: s.clone(),
            placed: Vec::new(),
            ledger: Value::Null,
            finished: false,
        };
        let env = OperatorEnvironment::new(Operator::new(&c, &ev, None))
            .with_run("carve-dead")
            .with_lease(lease_req(&s, "alice", "carve-dead", Duration::from_secs(1)))
            .with_plan(run_dir.clone(), plan);
        env.acquire_lease().unwrap();
        let placed: Vec<&ensaio::fixture::Fixture> = s.fixtures.iter().collect();
        for f in &placed {
            ensaio::EnsaioEnvironment::place_fixture(&env, f).unwrap();
        }
        env.track(&placed, false);
    }
    assert!(c.get("", "Namespace", "carved").is_some());
    let mut l = c.get("lab", "Lease", "visita-carve-dead").unwrap();
    l["metadata"]["annotations"][lease::EXPIRES] = json!("2020-01-01T00:00:00.000000Z");
    c.put("lab", l);
    let leases: Vec<Value> = c
        .objects
        .borrow()
        .values()
        .filter(|o| o["kind"] == "Lease")
        .cloned()
        .collect();
    assert!(
        ensaio::visita::reap::select(&leases, "bob", Some("carve-dead"), chrono::Utc::now())
            .unwrap_err()
            .contains("held by `alice/carve-dead`")
    );
    assert!(
        ensaio::visita::reap::select(&leases, "bob", None, chrono::Utc::now())
            .unwrap()
            .is_empty()
    );
    let chosen = ensaio::visita::reap::select(&leases, "alice", None, chrono::Utc::now()).unwrap();
    assert_eq!(chosen[0].run, "carve-dead");
    let plan = lease::read_plan(&run_dir).unwrap();
    assert_eq!(plan.placed, ["ns", "node"]);
    assert!(plan.lease.is_some());
    let natural: snapshot::NaturalState =
        serde_json::from_slice(&std::fs::read(run_dir.join("natural-state.json")).unwrap())
            .unwrap();
    let ev = Evidence::open(&run_dir.join("reap")).unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None))
        .with_plan(run_dir.clone(), plan.clone());
    let report = ensaio::visita::reap::reap(&env, &plan, &natural).unwrap();
    assert_eq!(report.verdict.tag(), "held", "{report:#?}");
    assert!(c.get("", "Namespace", "carved").is_none());
    assert_eq!(c.count("Lease"), 0);
    assert!(
        c.get("", "Node", "node-a").unwrap()["spec"]
            .get("taints")
            .is_none()
    );
    assert!(lease::read_plan(&run_dir).unwrap().finished);
    assert!(run_dir.join("reap/reap.json").exists());
}

#[test]
fn the_engenho_qualifications_load_and_the_guard_control_refuses() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../specs");
    let layer =
        shikumi::ProgressiveLayer::try_from_config_override(root.join("fixtures/qual-visita.yaml"))
            .unwrap();
    let cfg = VisitaConfig::from_layers(&[layer]).unwrap();
    for name in ["namespace", "fence", "node", "argocd-pause", "reap"] {
        let src =
            std::fs::read_to_string(root.join(format!("qual-carve-{name}.visita.lisp"))).unwrap();
        let l = prepare(
            load_one::<VisitaSpec>(&src).unwrap(),
            &cfg,
            &[],
            &["lab".into()],
            &[],
            0,
        )
        .unwrap();
        assert_eq!(l.target.unwrap().0, "lab", "{name}");
    }
    let src = std::fs::read_to_string(root.join("qual-carve-guard-refused.visita.lisp")).unwrap();
    let err = prepare(
        load_one::<VisitaSpec>(&src).unwrap(),
        &cfg,
        &[],
        &["lab-locked".into()],
        &[],
        0,
    )
    .err()
    .unwrap();
    assert!(
        err.contains("does not allow class `carve-node`")
            && err.contains("no window tagged `lab-locked`"),
        "{err}"
    );
}

#[test]
fn negative_control_a_reap_that_cannot_undo_is_not_held() {
    let c = seeded();
    let s = spec(CARVE_NS, r#"(:place "ns")"#);
    let dir = scratch("reap-neg");
    let ev = Evidence::open(&dir).unwrap();
    let op = Operator::new(&c, &ev, None);
    let natural = snapshot::capture(&op, &s.natural_state, "natural-state").unwrap();
    let env = OperatorEnvironment::new(Operator::new(&c, &ev, None));
    ensaio::EnsaioEnvironment::place_fixture(&env, &s.fixtures[0]).unwrap();
    let plan = RunPlan {
        run: "r".into(),
        operator: "alice".into(),
        target: None,
        lease: None,
        spec: s.clone(),
        placed: vec!["ns".into()],
        ledger: env.ledger(),
        finished: false,
    };
    *c.deletes_ignored.borrow_mut() = true;
    let env2 = OperatorEnvironment::new(Operator::new(&c, &ev, None));
    let report = ensaio::visita::reap::reap(&env2, &plan, &natural).unwrap();
    assert_ne!(report.verdict.tag(), "held");
}

#[test]
fn the_runner_loss_qualifications_load_through_the_tier_guard() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../specs");
    let layer =
        shikumi::ProgressiveLayer::try_from_config_override(root.join("fixtures/qual-visita.yaml"))
            .unwrap();
    let cfg = VisitaConfig::from_layers(&[layer]).unwrap();
    for name in ["recovers", "escalates", "off", "test-failure"] {
        let src = std::fs::read_to_string(root.join(format!("qual-runnerloss-{name}.visita.lisp")))
            .unwrap();
        let spec = load_one::<VisitaSpec>(&src)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .relative_to(&root);
        let l = prepare(spec, &cfg, &[], &["ci".into()], &[], 0)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(l.target.unwrap().0, "ci", "{name}");
    }
}

#[test]
fn a_target_that_does_not_allow_delete_runner_pod_refuses_the_loss_trials() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../specs");
    let layer =
        shikumi::ProgressiveLayer::try_from_config_override(root.join("fixtures/qual-visita.yaml"))
            .unwrap();
    let cfg = VisitaConfig::from_layers(&[layer]).unwrap();
    let src = std::fs::read_to_string(root.join("qual-runnerloss-recovers.visita.lisp")).unwrap();
    let dispatch_only = ["ci-no-delete".to_string()];
    let retarget = |s: &str| s.replace(r#":target "ci""#, r#":target "ci-no-delete""#);
    assert!(src.contains(r#":target "ci""#), "the spec names its target");
    let err = prepare(
        load_one::<VisitaSpec>(&retarget(&src)).unwrap(),
        &cfg,
        &[],
        &dispatch_only,
        &[],
        0,
    )
    .err()
    .unwrap();
    assert!(
        err.contains("does not allow class `delete-runner-pod`"),
        "{err}"
    );
    let src =
        std::fs::read_to_string(root.join("qual-runnerloss-test-failure.visita.lisp")).unwrap();
    prepare(
        load_one::<VisitaSpec>(&retarget(&src)).unwrap(),
        &cfg,
        &[],
        &dispatch_only,
        &[],
        0,
    )
    .expect("the test-failure trial deletes nothing, so a dispatch-only target runs it");
}
