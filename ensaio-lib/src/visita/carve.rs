use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::spec::{Peer, Quantity};

pub const RUN_LABEL: &str = "ensaio.pleme.io/run";
pub const SKIP_RECONCILE: &str = "argocd.argoproj.io/skip-reconcile";

fn quantities(qs: &[Quantity]) -> Value {
    Value::Object(
        qs.iter()
            .map(|q| (q.name.clone(), Value::String(q.value.clone())))
            .collect(),
    )
}

#[must_use]
pub fn namespace_objects(
    name: &str,
    run: &str,
    quota: &[Quantity],
    limit_default: &[Quantity],
    limit_default_request: &[Quantity],
) -> Vec<Value> {
    let mut out = vec![json!({
        "apiVersion": "v1", "kind": "Namespace",
        "metadata": {"name": name, "labels": {RUN_LABEL: run}},
    })];
    if !quota.is_empty() {
        out.push(json!({
            "apiVersion": "v1", "kind": "ResourceQuota",
            "metadata": {"name": "ensaio-carve", "labels": {RUN_LABEL: run}},
            "spec": {"hard": quantities(quota)},
        }));
    }
    if !limit_default.is_empty() || !limit_default_request.is_empty() {
        let mut limit = serde_json::Map::new();
        limit.insert("type".into(), json!("Container"));
        if !limit_default.is_empty() {
            limit.insert("default".into(), quantities(limit_default));
        }
        if !limit_default_request.is_empty() {
            limit.insert("defaultRequest".into(), quantities(limit_default_request));
        }
        out.push(json!({
            "apiVersion": "v1", "kind": "LimitRange",
            "metadata": {"name": "ensaio-carve", "labels": {RUN_LABEL: run}},
            "spec": {"limits": [Value::Object(limit)]},
        }));
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPeer {
    pub to: Value,
    pub ports: Vec<Value>,
}

#[must_use]
pub fn dns_peer() -> ResolvedPeer {
    ResolvedPeer {
        to: json!({
            "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "kube-system"}},
            "podSelector": {"matchLabels": {"k8s-app": "kube-dns"}},
        }),
        ports: vec![
            json!({"protocol": "UDP", "port": 53}),
            json!({"protocol": "TCP", "port": 53}),
        ],
    }
}

#[must_use]
pub fn api_server_peers(endpoints: &Value) -> Vec<ResolvedPeer> {
    let subsets = endpoints
        .get("subsets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    subsets
        .iter()
        .flat_map(|s| {
            let ports: Vec<Value> = s
                .get("ports")
                .and_then(Value::as_array)
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| p.get("port").cloned())
                        .map(|port| json!({"protocol": "TCP", "port": port}))
                        .collect()
                })
                .unwrap_or_default();
            s.get("addresses")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|a| a.get("ip").and_then(Value::as_str).map(str::to_string))
                .map(move |ip| {
                    let cidr = if ip.contains(':') {
                        format!("{ip}/128")
                    } else {
                        format!("{ip}/32")
                    };
                    ResolvedPeer {
                        to: json!({"ipBlock": {"cidr": cidr}}),
                        ports: ports.clone(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn service_peer(service: &Value) -> Result<ResolvedPeer, String> {
    let ns = service
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .ok_or("the service names no namespace")?;
    let selector = service
        .pointer("/spec/selector")
        .filter(|s| s.as_object().is_some_and(|m| !m.is_empty()))
        .ok_or("the service has no pod selector, so no policy can name its pods")?;
    let ports = service
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .map(|p| {
                    json!({
                        "protocol": p.get("protocol").cloned().unwrap_or_else(|| json!("TCP")),
                        "port": p.get("targetPort").cloned().or_else(|| p.get("port").cloned()).unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ResolvedPeer {
        to: json!({
            "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": ns}},
            "podSelector": {"matchLabels": selector},
        }),
        ports,
    })
}

#[must_use]
pub fn namespace_peer(name: &str) -> ResolvedPeer {
    ResolvedPeer {
        to: json!({"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": name}}}),
        ports: Vec::new(),
    }
}

fn rule(direction: &str, peer: &ResolvedPeer) -> Value {
    let key = if direction == "egress" { "to" } else { "from" };
    let mut r = serde_json::Map::new();
    r.insert(key.into(), json!([peer.to]));
    if !peer.ports.is_empty() {
        r.insert("ports".into(), Value::Array(peer.ports.clone()));
    }
    Value::Object(r)
}

#[must_use]
pub fn fence_objects(run: &str, egress: &[ResolvedPeer], ingress: &[ResolvedPeer]) -> Vec<Value> {
    let labels = json!({RUN_LABEL: run});
    let mut out = vec![json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
        "metadata": {"name": "ensaio-fence-deny", "labels": labels},
        "spec": {"podSelector": {}, "policyTypes": ["Ingress", "Egress"]},
    })];
    if !egress.is_empty() {
        out.push(json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
            "metadata": {"name": "ensaio-fence-egress", "labels": labels},
            "spec": {"podSelector": {}, "policyTypes": ["Egress"],
                     "egress": egress.iter().map(|p| rule("egress", p)).collect::<Vec<_>>()},
        }));
    }
    if !ingress.is_empty() {
        out.push(json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
            "metadata": {"name": "ensaio-fence-ingress", "labels": labels},
            "spec": {"podSelector": {}, "policyTypes": ["Ingress"],
                     "ingress": ingress.iter().map(|p| rule("ingress", p)).collect::<Vec<_>>()},
        }));
    }
    out
}

#[must_use]
pub fn peer_word(p: &Peer) -> String {
    match p {
        Peer::Dns => "dns".into(),
        Peer::ApiServer => "api-server".into(),
        Peer::Service { namespace, name } => format!("service {namespace}/{name}"),
        Peer::Namespace(n) => format!("namespace {n}"),
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NodeCarve {
    pub context: String,
    pub node: String,
    pub uid: Option<String>,
    pub taint_key: String,
    pub label_key: String,
}

pub fn pick_node(
    nodes: &[Value],
    taint_key: &str,
    label_key: &str,
) -> Result<(String, Option<String>), String> {
    let mut candidates: Vec<&Value> = nodes
        .iter()
        .filter(|n| n.pointer("/spec/unschedulable").and_then(Value::as_bool) != Some(true))
        .collect();
    candidates.sort_by_key(|n| {
        n.pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    });
    let n = candidates
        .first()
        .ok_or("the selector selects no schedulable node")?;
    let name = n
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if taints(n)
        .iter()
        .any(|t| t.get("key").and_then(Value::as_str) == Some(taint_key))
    {
        return Err(format!(
            "node `{name}` already carries a taint `{taint_key}`; it is not ours to take"
        ));
    }
    if n.pointer("/metadata/labels")
        .and_then(|l| l.get(label_key))
        .is_some()
    {
        return Err(format!(
            "node `{name}` already carries the label `{label_key}`; it is not ours to take"
        ));
    }
    Ok((
        name,
        n.pointer("/metadata/uid")
            .and_then(Value::as_str)
            .map(str::to_string),
    ))
}

fn taints(node: &Value) -> Vec<Value> {
    node.pointer("/spec/taints")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[must_use]
pub fn carve_node_patch(
    node: &Value,
    taint_key: &str,
    taint_value: Option<&str>,
    label_key: &str,
    label_value: &str,
) -> Value {
    let mut t = taints(node);
    let mut taint = serde_json::Map::new();
    taint.insert("key".into(), json!(taint_key));
    if let Some(v) = taint_value {
        taint.insert("value".into(), json!(v));
    }
    taint.insert("effect".into(), json!("NoSchedule"));
    t.push(Value::Object(taint));
    json!({"metadata": {"labels": {label_key: label_value}}, "spec": {"taints": t}})
}

#[must_use]
pub fn uncarve_node_patch(node: &Value, taint_key: &str, label_key: &str) -> Value {
    let t: Vec<Value> = taints(node)
        .into_iter()
        .filter(|x| {
            !(x.get("key").and_then(Value::as_str) == Some(taint_key)
                && x.get("effect").and_then(Value::as_str) == Some("NoSchedule"))
        })
        .collect();
    let taints = if t.is_empty() {
        Value::Null
    } else {
        Value::Array(t)
    };
    json!({"metadata": {"labels": {label_key: Value::Null}}, "spec": {"taints": taints}})
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "mode")]
pub enum PauseMode {
    SyncPolicy { captured: Option<Value> },
    SkipReconcile { captured: Option<String> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArgoPaused {
    pub context: String,
    pub namespace: String,
    pub application: String,
    pub pause: PauseMode,
}

#[must_use]
pub fn owning_appset(app: &Value) -> Option<String> {
    app.pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)?
        .iter()
        .find(|o| o.get("kind").and_then(Value::as_str) == Some("ApplicationSet"))
        .and_then(|o| o.get("name").and_then(Value::as_str))
        .map(str::to_string)
}

#[must_use]
pub fn appset_preserves_skip(appset: &Value) -> bool {
    let preserved = appset
        .pointer("/spec/preservedFields/annotations")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|k| k.as_str() == Some(SKIP_RECONCILE)));
    let ignored = appset
        .pointer("/spec/ignoreApplicationDifferences")
        .and_then(Value::as_array)
        .is_some_and(|rules| {
            rules.iter().any(|r| {
                r.get("jsonPointers")
                    .and_then(Value::as_array)
                    .is_some_and(|ps| {
                        ps.iter().filter_map(Value::as_str).any(|p| {
                            p == "/metadata/annotations"
                                || p == "/metadata"
                                || p == "/metadata/annotations/argocd.argoproj.io~1skip-reconcile"
                        })
                    })
            })
        });
    preserved || ignored
}

#[must_use]
pub fn exact_merge_patch(live: &Value, want: &Value) -> Value {
    match (live, want) {
        (Value::Object(l), Value::Object(w)) => {
            let mut out = serde_json::Map::new();
            for (k, wv) in w {
                match l.get(k) {
                    Some(lv) if lv == wv => {}
                    Some(lv) if lv.is_object() && wv.is_object() => {
                        out.insert(k.clone(), exact_merge_patch(lv, wv));
                    }
                    _ => {
                        out.insert(k.clone(), wv.clone());
                    }
                }
            }
            for k in l.keys() {
                if !w.contains_key(k) {
                    out.insert(k.clone(), Value::Null);
                }
            }
            Value::Object(out)
        }
        _ => want.clone(),
    }
}

#[must_use]
pub fn pause_patch(app: &Value) -> Value {
    if app.pointer("/spec/syncPolicy/automated").is_some() {
        json!({"spec": {"syncPolicy": {"automated": null}}})
    } else {
        json!({})
    }
}

#[must_use]
pub fn unpause_patch(live_app: &Value, captured: Option<&Value>) -> Value {
    let live = live_app.pointer("/spec/syncPolicy").cloned();
    match (live, captured) {
        (None, None) => json!({}),
        (Some(_), None) => json!({"spec": {"syncPolicy": null}}),
        (None, Some(want)) => json!({"spec": {"syncPolicy": want}}),
        (Some(l), Some(want)) => json!({"spec": {"syncPolicy": exact_merge_patch(&l, want)}}),
    }
}
