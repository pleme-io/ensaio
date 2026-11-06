//! The natural state of a borrowed environment, and the proof it came back.
//!
//! A [`NaturalState`] is a content-addressed capture of the declared objects,
//! with apiserver bookkeeping stripped (`managedFields`, `resourceVersion`,
//! `uid`, `generation`, `creationTimestamp`, `status` unless a selection keeps
//! part of it). It is stored in the operator's evidence directory, never in the
//! target.
//!
//! **Secret values never reach the evidence.** A `Secret`'s `data` and
//! `stringData` values are replaced by their BLAKE3 digest before anything is
//! written, so a change is still detected and the plaintext is not copied off
//! the cluster. Tier: truly-unrepresentable in the stored snapshot (the value
//! is gone before the struct exists).
//!
//! The diff flattens each object to named leaf paths, keying list elements by
//! their `name` where they have one (`containers[api].env[STAKATER_X].value`),
//! so a reordered list is not a change and an allow-list can name exactly the
//! fields that legitimately move (reloader env, `restartedAt`, HPA-owned
//! replicas).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::command::{Operator, Verb, now};
use super::spec::Selection;

/// Paths every visita ignores, on top of what a spec adds. Each is a field a
/// controller moves on its own, so a restored environment legitimately differs
/// there.
pub const DEFAULT_ALLOW: &[&str] = &[
    // Deployment revision counter: bumps on every rollout, including the
    // restoring one.
    "Deployment/*:metadata.annotations[\"deployment.kubernetes.io/revision\"]",
    // `kubectl rollout restart` stamp.
    "Deployment/*:spec.template.metadata.annotations[\"kubectl.kubernetes.io/restartedAt\"]",
    // stakater reloader writes a hash env per watched ConfigMap/Secret.
    "Deployment/*:spec.template.spec.containers[*].env[STAKATER_*",
    // HPA-owned.
    "Deployment/*:spec.replicas",
    // Argo writes `operation` for a requested sync and clears it afterwards.
    "Application/*:operation*",
    "Application/*:metadata.annotations[\"argocd.argoproj.io/refresh\"]",
];

/// One captured object's identity.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey {
    pub context: String,
    pub namespace: String,
    pub kind: String,
    pub name: String,
}

impl std::fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}/{}/{}",
            self.context, self.namespace, self.kind, self.name
        )
    }
}

/// The captured natural state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct NaturalState {
    pub captured_at: String,
    /// BLAKE3 over the canonical JSON of `objects`.
    pub address: String,
    pub objects: BTreeMap<String, CapturedObject>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CapturedObject {
    pub key: ObjectKey,
    pub object: Value,
    /// `metadata.generation` at capture — the rollout baseline. Kept beside
    /// the object, never in it, so it is not part of the restore diff.
    #[serde(default)]
    pub generation: Option<i64>,
}

impl NaturalState {
    /// The captured object for a key, if the natural state covers it.
    #[must_use]
    pub fn get(&self, context: &str, namespace: &str, kind: &str, name: &str) -> Option<&Value> {
        self.objects
            .values()
            .find(|c| {
                c.key.context == context
                    && c.key.namespace == namespace
                    && c.key.kind.eq_ignore_ascii_case(kind)
                    && c.key.name == name
            })
            .map(|c| &c.object)
    }

    /// The captured `metadata.generation` of one object.
    #[must_use]
    pub fn generation(
        &self,
        context: &str,
        namespace: &str,
        kind: &str,
        name: &str,
    ) -> Option<i64> {
        self.objects
            .values()
            .find(|c| {
                c.key.context == context
                    && c.key.namespace == namespace
                    && c.key.kind.eq_ignore_ascii_case(kind)
                    && c.key.name == name
            })
            .and_then(|c| c.generation)
    }
}

/// `live` without the objects named in `held` — objects a still-placed
/// fixture created, which the natural state never had.
#[must_use]
pub fn without(mut live: NaturalState, held: &[ObjectKey]) -> NaturalState {
    live.objects.retain(|_, o| {
        !held.iter().any(|h| {
            h.context == o.key.context
                && h.namespace == o.key.namespace
                && h.kind.eq_ignore_ascii_case(&o.key.kind)
                && h.name == o.key.name
        })
    });
    live
}

/// Capture every declared selection.
///
/// # Errors
///
/// A read that failed, or a declared name pattern that matched nothing. The
/// second is refused rather than recorded as empty: a trial that believes it
/// protects `api-deployment` while the snapshot holds no such object would
/// report "restored" over something it never looked at.
pub fn capture(
    op: &Operator<'_>,
    selections: &[Selection],
    label: &str,
) -> Result<NaturalState, String> {
    let mut objects = BTreeMap::new();
    for sel in selections {
        let found = read_selection(op, sel, label)?;
        for pat in &sel.names {
            if !found.iter().any(|c| glob(pat, &c.key.name)) {
                return Err(format!(
                    "selection {}/{} [{}]: name `{pat}` matched no object",
                    sel.context,
                    sel.namespace,
                    sel.kinds.join(",")
                ));
            }
        }
        for c in found {
            objects.insert(c.key.to_string(), c);
        }
    }
    Ok(seal(objects))
}

/// Read one selection, normalized and name-filtered.
///
/// # Errors
///
/// When the read fails or does not return a `List`.
pub fn read_selection(
    op: &Operator<'_>,
    sel: &Selection,
    label: &str,
) -> Result<Vec<CapturedObject>, String> {
    let verb = Verb::Get {
        context: sel.context.clone(),
        namespace: sel.namespace.clone(),
        resources: sel.kinds.join(","),
        selector: sel.selector.clone(),
    };
    let list = op.json(&format!("{label}:{}", sel.namespace), &verb)?;
    let items = list
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{label}: response carries no `items`"))?;
    let mut out = Vec::new();
    for item in items {
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or_default();
        let name = item
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !sel.names.is_empty() && !sel.names.iter().any(|p| glob(p, name)) {
            continue;
        }
        out.push(CapturedObject {
            key: ObjectKey {
                context: sel.context.clone(),
                namespace: sel.namespace.clone(),
                kind: kind.to_string(),
                name: name.to_string(),
            },
            object: normalize(item.clone(), &sel.keep_status),
            generation: item.pointer("/metadata/generation").and_then(Value::as_i64),
        });
    }
    Ok(out)
}

fn seal(objects: BTreeMap<String, CapturedObject>) -> NaturalState {
    let canonical = serde_json::to_vec(&objects).unwrap_or_default();
    NaturalState {
        captured_at: now(),
        address: blake3::hash(&canonical).to_hex().to_string(),
        objects,
    }
}

fn digest(raw: &str) -> Value {
    Value::String(format!("blake3:{}", blake3::hash(raw.as_bytes()).to_hex()))
}

/// Replace every Secret value in a kubectl JSON response — a `List` or one
/// object — with its BLAKE3 digest: `data`, `stringData`, and the
/// `last-applied-configuration` annotation, which carries `stringData` in
/// plaintext for any Secret ever `kubectl apply`'d.
///
/// Applied at the recording boundary ([`super::command::Operator::run`]), so
/// neither the raw evidence files nor any caller ever holds a Secret value.
pub fn redact_secrets(v: &mut Value) {
    if let Some(items) = v.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            redact_secrets(item);
        }
        return;
    }
    let Some(map) = v.as_object_mut() else {
        return;
    };
    if map.get("kind").and_then(Value::as_str) != Some("Secret") {
        return;
    }
    for field in ["data", "stringData"] {
        if let Some(d) = map.get_mut(field).and_then(Value::as_object_mut) {
            for val in d.values_mut() {
                *val = digest(val.as_str().unwrap_or_default());
            }
        }
    }
    if let Some(ann) = map
        .get_mut("metadata")
        .and_then(|m| m.get_mut("annotations"))
        .and_then(Value::as_object_mut)
    {
        if let Some(la) = ann.get_mut("kubectl.kubernetes.io/last-applied-configuration") {
            *la = digest(la.as_str().unwrap_or_default());
        }
    }
}

/// Strip bookkeeping; hash secret values; keep only the declared status paths.
#[must_use]
pub fn normalize(mut obj: Value, keep_status: &[String]) -> Value {
    redact_secrets(&mut obj);
    let kept: Vec<(String, Value)> = keep_status
        .iter()
        .filter_map(|p| {
            let ptr = format!("/{}", p.replace('.', "/"));
            obj.pointer(&ptr).map(|v| (p.clone(), v.clone()))
        })
        .collect();
    if let Some(map) = obj.as_object_mut() {
        map.remove("status");
        if let Some(meta) = map.get_mut("metadata").and_then(Value::as_object_mut) {
            for k in [
                "managedFields",
                "resourceVersion",
                "uid",
                "generation",
                "creationTimestamp",
                "selfLink",
            ] {
                meta.remove(k);
            }
            if let Some(ann) = meta.get_mut("annotations").and_then(Value::as_object_mut) {
                ann.remove("kubectl.kubernetes.io/last-applied-configuration");
                if ann.is_empty() {
                    meta.remove("annotations");
                }
            }
        }
        if !kept.is_empty() {
            let mut status = serde_json::Map::new();
            for (path, v) in kept {
                status.insert(path, v);
            }
            map.insert("keptStatus".into(), Value::Object(status));
        }
    }
    obj
}

/// Flatten to `path → leaf`. Object keys containing `.` are bracketed so a
/// path is unambiguous; list elements carrying a string `name` are keyed by it.
#[must_use]
pub fn flatten(v: &Value) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    walk(v, String::new(), &mut out);
    out
}

fn walk(v: &Value, path: String, out: &mut BTreeMap<String, Value>) {
    match v {
        Value::Object(m) if !m.is_empty() => {
            for (k, child) in m {
                let seg = if k.contains('.') || k.contains('[') {
                    format!("[\"{k}\"]")
                } else if path.is_empty() {
                    k.clone()
                } else {
                    format!(".{k}")
                };
                walk(child, format!("{path}{seg}"), out);
            }
        }
        Value::Array(a) if !a.is_empty() => {
            for (i, child) in a.iter().enumerate() {
                let key = child
                    .get("name")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("[#{i}]"), |n| format!("[{n}]"));
                walk(child, format!("{path}{key}"), out);
            }
        }
        leaf => {
            out.insert(path, leaf.clone());
        }
    }
}

/// `*` matches any run of characters, including none. No other metacharacter.
#[must_use]
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] != '*' && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// One changed leaf.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Change {
    pub path: String,
    pub natural: Option<Value>,
    pub live: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ObjectDiff {
    pub object: String,
    pub changes: Vec<Change>,
}

/// Live versus natural, modulo the allow-list.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct RestoreDiff {
    /// Objects compared — the denominator. Zero means nothing was protected.
    pub checked: usize,
    /// In the natural state, gone from the live one.
    pub missing: Vec<String>,
    /// In the live state and the selection, absent from the natural one.
    pub extra: Vec<String>,
    pub differing: Vec<ObjectDiff>,
    /// Changes the allow-list absorbed, counted so an allow-list that hides
    /// everything is visible rather than silent.
    pub allowed: usize,
}

impl RestoreDiff {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && self.differing.is_empty()
    }
}

/// Compare. `allow` entries are `Kind/name-glob:path-glob`.
#[must_use]
pub fn diff(natural: &NaturalState, live: &NaturalState, allow: &[String]) -> RestoreDiff {
    let mut out = RestoreDiff::default();
    let keys: BTreeSet<&String> = natural.objects.keys().chain(live.objects.keys()).collect();
    for k in keys {
        match (natural.objects.get(k), live.objects.get(k)) {
            (Some(_), None) => {
                out.checked += 1;
                out.missing.push(k.clone());
            }
            (None, Some(_)) => out.extra.push(k.clone()),
            (Some(n), Some(l)) => {
                out.checked += 1;
                let (fa, fb) = (flatten(&n.object), flatten(&l.object));
                let paths: BTreeSet<&String> = fa.keys().chain(fb.keys()).collect();
                let mut changes = Vec::new();
                for p in paths {
                    let (a, b) = (fa.get(p), fb.get(p));
                    if a == b {
                        continue;
                    }
                    let subject = format!("{}/{}:{p}", n.key.kind, n.key.name);
                    if allow.iter().any(|g| glob(g, &subject)) {
                        out.allowed += 1;
                        continue;
                    }
                    changes.push(Change {
                        path: p.clone(),
                        natural: a.cloned(),
                        live: b.cloned(),
                    });
                }
                if !changes.is_empty() {
                    out.differing.push(ObjectDiff {
                        object: k.clone(),
                        changes,
                    });
                }
            }
            (None, None) => {}
        }
    }
    out
}

/// The allow-list a spec runs with: the defaults, then its own.
#[must_use]
pub fn effective_allow(spec_allow: &[String]) -> Vec<String> {
    DEFAULT_ALLOW
        .iter()
        .map(|s| (*s).to_string())
        .chain(spec_allow.iter().cloned())
        .collect()
}
