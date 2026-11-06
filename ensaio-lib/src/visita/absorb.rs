use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Displacement {
    pub path: String,
    pub natural: Value,
    pub wanted: Value,
}

fn pointer_push(at: &str, key: &str) -> String {
    format!("{at}/{}", key.replace('~', "~0").replace('/', "~1"))
}

#[must_use]
pub fn displaced(forward: &Value, natural: &Value) -> Vec<Displacement> {
    let mut out = Vec::new();
    walk_displaced(forward, natural, "", &mut out);
    out
}

fn walk_displaced(forward: &Value, natural: &Value, at: &str, out: &mut Vec<Displacement>) {
    let Value::Object(fm) = forward else {
        if !natural.is_null() && natural != forward {
            out.push(Displacement {
                path: at.to_string(),
                natural: natural.clone(),
                wanted: forward.clone(),
            });
        }
        return;
    };
    for (k, fv) in fm {
        let path = pointer_push(at, k);
        match natural.get(k) {
            None | Some(Value::Null) => {}
            Some(nv) if fv.is_object() && nv.is_object() => walk_displaced(fv, nv, &path, out),
            Some(nv) if nv != fv => out.push(Displacement {
                path,
                natural: nv.clone(),
                wanted: fv.clone(),
            }),
            Some(_) => {}
        }
    }
}

#[must_use]
pub fn annotation_patch(key: &str, value: &str) -> Value {
    serde_json::json!({ "metadata": { "annotations": { key: value } } })
}

#[must_use]
pub fn describe(d: &[Displacement]) -> String {
    d.iter()
        .map(|x| {
            format!(
                "{}: held live as {}, the fixture writes {}",
                x.path, x.natural, x.wanted
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Change {
    pub address: String,
    pub path: String,
    pub before: String,
    pub after: String,
}

#[derive(Serialize, Debug, Default, PartialEq)]
pub struct PlanDelta {
    pub absorbed: Vec<Change>,
    pub ours: Vec<Change>,
    pub kept: Vec<Change>,
    pub vanished: Vec<Change>,
    pub unexpected: Vec<Change>,
}

impl PlanDelta {
    #[must_use]
    pub fn held(&self) -> bool {
        self.unexpected.is_empty() && self.vanished.is_empty()
    }
}

type Changes = BTreeMap<(String, String), (String, String)>;

const SENSITIVE: &str = "(sensitive)";
const UNKNOWN: &str = "(known after apply)";
const ABSENT: &str = "(absent)";

fn usable<'p>(plan: &'p Value, which: &str) -> Result<&'p Vec<Value>, String> {
    if plan.get("format_version").is_none() {
        return Err(format!(
            "{which}: not `terraform show -json` output (no format_version)"
        ));
    }
    if plan.get("errored").and_then(Value::as_bool) == Some(true) {
        return Err(format!("{which}: the plan errored"));
    }
    plan.get("resource_changes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{which}: no resource_changes, so it is a state dump or an empty plan, not a plan to compare"))
}

pub fn plan_changes(plan: &Value) -> Result<Changes, String> {
    let mut out = BTreeMap::new();
    for rc in usable(plan, "plan")? {
        let actions: Vec<&str> = rc
            .pointer("/change/actions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if actions.iter().all(|a| *a == "no-op" || *a == "read") {
            continue;
        }
        let address = rc
            .get("address")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        out.insert(
            (address.clone(), "@actions".into()),
            (String::new(), actions.join(",")),
        );
        let at = |p: &str| rc.pointer(p).cloned().unwrap_or(Value::Null);
        let side = Side {
            before: at("/change/before"),
            after: at("/change/after"),
            unknown: at("/change/after_unknown"),
            before_sensitive: at("/change/before_sensitive"),
            after_sensitive: at("/change/after_sensitive"),
        };
        let mut leaves = Vec::new();
        side.walk(&Cursor::root(), &mut leaves);
        for (path, b, a) in leaves {
            out.insert((address.clone(), path), (b, a));
        }
    }
    Ok(out)
}

struct Side {
    before: Value,
    after: Value,
    unknown: Value,
    before_sensitive: Value,
    after_sensitive: Value,
}

struct Cursor {
    path: String,
    segments: Vec<Value>,
}

impl Cursor {
    fn root() -> Self {
        Self {
            path: String::new(),
            segments: Vec::new(),
        }
    }

    fn child(&self, key: Value) -> Self {
        let text = match &key {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut segments = self.segments.clone();
        segments.push(key);
        Self {
            path: pointer_push(&self.path, &text),
            segments,
        }
    }

    fn get<'v>(&self, root: &'v Value) -> &'v Value {
        let mut v = root;
        for s in &self.segments {
            v = match (s, v) {
                (Value::String(k), Value::Object(m)) => m.get(k).unwrap_or(&Value::Null),
                (Value::Number(n), Value::Array(a)) => n
                    .as_u64()
                    .and_then(|i| a.get(usize::try_from(i).ok()?))
                    .unwrap_or(&Value::Null),
                (_, Value::Bool(true)) => return &Value::Bool(true),
                _ => &Value::Null,
            };
        }
        v
    }
}

impl Side {
    fn render(&self, v: &Value, sensitive: &Value) -> String {
        if sensitive == &Value::Bool(true) {
            return SENSITIVE.into();
        }
        match v {
            Value::Null => ABSENT.into(),
            Value::String(s) => s.clone(),
            other => mask(other, sensitive).to_string(),
        }
    }

    fn walk(&self, c: &Cursor, out: &mut Vec<(String, String, String)>) {
        let (b, a) = (c.get(&self.before), c.get(&self.after));
        let (bs, as_) = (c.get(&self.before_sensitive), c.get(&self.after_sensitive));
        if c.get(&self.unknown) == &Value::Bool(true) {
            out.push((c.path.clone(), self.render(b, bs), UNKNOWN.into()));
            return;
        }
        match (b, a) {
            (Value::Object(bm), Value::Object(am)) => {
                let keys: std::collections::BTreeSet<&String> =
                    bm.keys().chain(am.keys()).collect();
                for k in keys {
                    self.walk(&c.child(Value::String(k.clone())), out);
                }
            }
            (Value::Array(ba), Value::Array(aa)) if ba.len() == aa.len() => {
                for i in 0..ba.len() {
                    self.walk(&c.child(Value::from(i)), out);
                }
            }
            (b, a) if b == a && bs == as_ => {}
            (b, a) => out.push((c.path.clone(), self.render(b, bs), self.render(a, as_))),
        }
    }
}

fn mask(v: &Value, sensitive: &Value) -> Value {
    match (v, sensitive) {
        (_, Value::Bool(true)) => Value::String(SENSITIVE.into()),
        (Value::Object(m), Value::Object(s)) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), mask(x, s.get(k).unwrap_or(&Value::Null))))
                .collect(),
        ),
        (Value::Array(a), Value::Array(s)) => Value::Array(
            a.iter()
                .enumerate()
                .map(|(i, x)| mask(x, s.get(i).unwrap_or(&Value::Null)))
                .collect(),
        ),
        (other, _) => other.clone(),
    }
}

pub fn plan_delta(
    baseline: &Value,
    candidate: &Value,
    expect: &[String],
) -> Result<PlanDelta, String> {
    usable(baseline, "baseline")?;
    usable(candidate, "candidate")?;
    let live = |p: &Value| p.pointer("/prior_state/values/root_module").cloned();
    if live(baseline) != live(candidate) {
        return Err("the two plans were taken against different live state (prior_state resources differ); plan both again".into());
    }
    let base = plan_changes(baseline)?;
    let cand = plan_changes(candidate)?;
    let mut d = PlanDelta::default();
    let change = |(address, path): &(String, String), (before, after): &(String, String)| Change {
        address: address.clone(),
        path: path.clone(),
        before: before.clone(),
        after: after.clone(),
    };
    for (k, v) in &cand {
        let c = change(k, v);
        if base.get(k) == Some(v) {
            d.absorbed.push(c);
        } else if expected(&c, expect) {
            d.ours.push(c);
        } else {
            d.unexpected.push(c);
        }
    }
    for (k, v) in &base {
        if cand.contains_key(k) {
            continue;
        }
        let c = change(k, v);
        if c.path != "@actions" && expected(&c, expect) {
            d.kept.push(c);
        } else if c.path == "@actions" && cand.keys().any(|(a, _)| a == &c.address) {
            d.kept.push(c);
        } else {
            d.vanished.push(c);
        }
    }
    Ok(d)
}

fn expected(c: &Change, expect: &[String]) -> bool {
    let actions_only_update = c.path == "@actions" && c.after == "update";
    expect.iter().any(|e| {
        let (addr, path) = e.split_once('#').unwrap_or((e.as_str(), ""));
        let addr_ok = addr == "*" || addr == c.address;
        addr_ok
            && (path.is_empty()
                || (actions_only_update && c.path == "@actions")
                || c.path == path
                || c.path.starts_with(&format!("{path}/")))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_key_absent_live_is_ours_to_take() {
        let f = json!({"metadata": {"labels": {"failover-app": "b"}}});
        let n = json!({"metadata": {"labels": {"karpenter": "x"}}});
        assert!(displaced(&f, &n).is_empty());
    }

    #[test]
    fn a_key_held_live_at_another_value_is_displaced() {
        let f = json!({"metadata": {"labels": {"karpenter": "master", "crossplane": "b"}}});
        let n = json!({"metadata": {"labels": {"karpenter": "fix-arm64", "crossplane": "b"}}});
        let d = displaced(&f, &n);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "/metadata/labels/karpenter");
        assert_eq!(d[0].natural, json!("fix-arm64"));
    }

    #[test]
    fn a_removal_of_a_live_key_is_displaced() {
        let f = json!({"metadata": {"annotations": {"crossplane": null}}});
        let n = json!({"metadata": {"annotations": {"crossplane": "pin"}}});
        assert_eq!(displaced(&f, &n).len(), 1);
    }

    #[test]
    fn a_dotted_or_slashed_key_is_one_pointer_segment() {
        let f = json!({"metadata": {"labels": {"app.kubernetes.io/name": "x"}}});
        let n = json!({"metadata": {"labels": {"app.kubernetes.io/name": "y"}}});
        assert_eq!(
            displaced(&f, &n)[0].path,
            "/metadata/labels/app.kubernetes.io~1name"
        );
    }

    fn rc(address: &str, actions: &[&str], before: Value, after: Value) -> Value {
        json!({"address": address, "change": {"actions": actions, "before": before, "after": after, "after_unknown": {}}})
    }

    fn plan(rcs: Vec<Value>) -> Value {
        json!({"format_version": "1.2", "prior_state": {"values": {}}, "resource_changes": rcs})
    }

    fn labels(l: Value) -> Value {
        json!({"metadata": [{"labels": l}]})
    }

    const OURS: &str = "argocd_cluster.this#/metadata/0/labels/failover-app";

    #[test]
    fn drift_already_pending_on_trunk_is_absorbed_and_only_our_keys_remain() {
        let live = json!({"karpenter": "fix-arm64", "vector": "v-branch", "crossplane": "master"});
        let base = plan(vec![
            rc(
                "argocd_cluster.this",
                &["update"],
                labels(live.clone()),
                labels(json!({"karpenter": "master", "vector": "master", "crossplane": "master"})),
            ),
            rc("aws_route53_record.ro", &["no-op"], json!({}), json!({})),
        ]);
        let cand = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            labels(live),
            labels(
                json!({"karpenter": "master", "vector": "master", "crossplane": "master", "failover-app": "master"}),
            ),
        )]);
        let d = plan_delta(&base, &cand, &[OURS.into()]).unwrap();
        assert_eq!(d.absorbed.len(), 3, "{d:?}");
        assert_eq!(d.ours.len(), 1);
        assert_eq!(d.ours[0].after, "master");
        assert!(d.held());
    }

    #[test]
    fn an_update_the_branch_alone_introduces_counts_its_action_as_ours() {
        let base = plan(vec![]);
        let cand = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            labels(json!({})),
            labels(json!({"failover-app": "m"})),
        )]);
        let d = plan_delta(&base, &cand, &[OURS.into()]).unwrap();
        assert!(d.held(), "{d:?}");
        assert_eq!(d.ours.len(), 2);
    }

    #[test]
    fn a_replace_the_trunk_plan_only_updates_is_unexpected() {
        let b = labels(json!({"a": "1"}));
        let a = labels(json!({"a": "2"}));
        let base = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            b.clone(),
            a.clone(),
        )]);
        let cand = plan(vec![rc("argocd_cluster.this", &["delete", "create"], b, a)]);
        let d = plan_delta(&base, &cand, &[OURS.into()]).unwrap();
        assert_eq!(d.unexpected.len(), 1);
        assert_eq!(d.unexpected[0].path, "@actions");
    }

    #[test]
    fn a_change_outside_the_declared_occupancy_is_unexpected() {
        let base = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            labels(json!({"a": "1"})),
            labels(json!({"a": "1", "z": "0"})),
        )]);
        let cand = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            labels(json!({"a": "1"})),
            labels(json!({"a": "2", "z": "0"})),
        )]);
        let d = plan_delta(&base, &cand, &[OURS.into()]).unwrap();
        assert_eq!(d.unexpected.len(), 1);
        assert!(!d.held());
    }

    #[test]
    fn a_prefix_matches_whole_segments_only() {
        let base = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            labels(json!({})),
            labels(json!({"z": "0"})),
        )]);
        let cand = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            labels(json!({})),
            labels(json!({"z": "0", "failover-app-x": "1"})),
        )]);
        let d = plan_delta(&base, &cand, &[OURS.into()]).unwrap();
        assert_eq!(d.unexpected.len(), 1);
    }

    #[test]
    fn a_pending_trunk_change_the_candidate_drops_is_vanished() {
        let base = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            labels(json!({"k": "x"})),
            labels(json!({"k": "master"})),
        )]);
        let cand = plan(vec![]);
        let d = plan_delta(&base, &cand, &[]).unwrap();
        assert_eq!(d.vanished.len(), 2);
        assert!(!d.held());
    }

    #[test]
    fn a_trunk_revert_of_a_live_pin_the_branch_declares_it_keeps_is_kept() {
        let live = json!({"crossplane": "pinned", "x": "1"});
        let base = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            labels(live.clone()),
            labels(json!({"x": "1"})),
        )]);
        let cand = plan(vec![rc(
            "argocd_cluster.this",
            &["update"],
            labels(live.clone()),
            labels(json!({"crossplane": "pinned", "x": "1", "failover-app": "m"})),
        )]);
        let keep = "argocd_cluster.this#/metadata/0/labels/crossplane".to_string();
        let d = plan_delta(&base, &cand, &[OURS.into(), keep]).unwrap();
        assert!(d.held(), "{d:?}");
        assert_eq!(d.kept.len(), 1);
        let d = plan_delta(&base, &cand, &[OURS.into()]).unwrap();
        assert_eq!(
            d.vanished.len(),
            1,
            "an undeclared drop of a trunk change is vanished"
        );
    }

    #[test]
    fn sensitive_values_never_print() {
        let mut r = rc(
            "argocd_cluster.this",
            &["create"],
            Value::Null,
            json!({"config": [{"bearer_token": "s3cret"}]}),
        );
        r["change"]["after_sensitive"] = json!({"config": [{"bearer_token": true}]});
        let c = plan_changes(&plan(vec![r])).unwrap();
        assert!(!format!("{c:?}").contains("s3cret"), "{c:?}");
        assert!(format!("{c:?}").contains(SENSITIVE));
    }

    #[test]
    fn a_state_dump_an_errored_plan_or_different_live_state_is_refused() {
        let good = plan(vec![]);
        assert!(plan_delta(&json!({"format_version": "1.0", "values": {}}), &good, &[]).is_err());
        let mut errored = plan(vec![]);
        errored["errored"] = json!(true);
        assert!(plan_delta(&good, &errored, &[]).is_err());
        let mut moved = plan(vec![]);
        moved["prior_state"] =
            json!({"values": {"root_module": {"resources": [{"address": "x"}]}}});
        assert!(plan_delta(&good, &moved, &[]).is_err());
        let mut outputs_only = plan(vec![]);
        outputs_only["prior_state"] = json!({"values": {"outputs": {"cluster": {"value": 1}}}});
        assert!(
            plan_delta(&good, &outputs_only, &[]).is_ok(),
            "outputs come from config, not live state"
        );
    }
}
