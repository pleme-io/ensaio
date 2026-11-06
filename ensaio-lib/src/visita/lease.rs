use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::spec::VisitaSpec;

pub const LEASE_LABEL: &str = "ensaio.pleme.io/lease";
pub const EXPIRES: &str = "ensaio.pleme.io/expires";
pub const SCOPE: &str = "ensaio.pleme.io/scope";
pub const SPEC: &str = "ensaio.pleme.io/spec";
pub const FIXTURES: &str = "ensaio.pleme.io/fixtures";
pub const RUN: &str = "ensaio.pleme.io/run";
pub const PLAN: &str = "plan.json";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRequest {
    pub context: String,
    pub namespace: String,
    pub name: String,
    pub holder: String,
    pub run: String,
    pub ttl_secs: u64,
    pub scope: BTreeSet<String>,
    pub spec: String,
    pub fixtures: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRef {
    pub context: String,
    pub namespace: String,
    pub name: String,
    pub uid: Option<String>,
    pub holder: String,
}

#[must_use]
pub fn slug(s: &str) -> String {
    let mut out: String = s
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

#[must_use]
pub fn run_id(spec_name: &str, at: DateTime<Utc>) -> String {
    let stamp = at.format("%Y%m%dt%H%M%Sz").to_string();
    let mut base = slug(spec_name);
    base.truncate(63 - 7 - stamp.len() - 1);
    format!("{}-{stamp}", base.trim_end_matches('-'))
}

#[must_use]
pub fn lease_name(run: &str) -> String {
    let mut n = format!("visita-{run}");
    n.truncate(63);
    n.trim_end_matches('-').to_string()
}

#[must_use]
pub fn micro(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

impl LeaseRequest {
    #[must_use]
    pub fn new(
        spec: &VisitaSpec,
        context: &str,
        namespace: &str,
        operator: &str,
        run: &str,
        ttl: Duration,
    ) -> Self {
        Self {
            context: context.into(),
            namespace: namespace.into(),
            name: lease_name(run),
            holder: format!("{operator}/{run}"),
            run: run.into(),
            ttl_secs: ttl.as_secs().max(1),
            scope: spec.scope(),
            spec: spec.name.clone(),
            fixtures: spec.fixtures.iter().map(|f| f.name.clone()).collect(),
        }
    }

    #[must_use]
    pub fn expires(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + chrono::Duration::seconds(i64::try_from(self.ttl_secs).unwrap_or(i64::MAX / 2))
    }

    #[must_use]
    pub fn object(&self, now: DateTime<Utc>) -> Value {
        json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": self.name,
                "namespace": self.namespace,
                "labels": {LEASE_LABEL: "visita", RUN: self.run},
                "annotations": {
                    EXPIRES: micro(self.expires(now)),
                    SCOPE: serde_json::to_string(&self.scope).unwrap_or_default(),
                    SPEC: self.spec,
                    FIXTURES: self.fixtures.join(","),
                },
            },
            "spec": {
                "holderIdentity": self.holder,
                "leaseDurationSeconds": self.ttl_secs,
                "acquireTime": micro(now),
                "renewTime": micro(now),
            },
        })
    }

    #[must_use]
    pub fn renew_patch(&self, now: DateTime<Utc>) -> Value {
        json!({
            "metadata": {"annotations": {EXPIRES: micro(self.expires(now))}},
            "spec": {"renewTime": micro(now)},
        })
    }
}

#[must_use]
pub fn expires_at(lease: &Value) -> Option<DateTime<Utc>> {
    lease
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(EXPIRES))
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
}

#[must_use]
pub fn holder(lease: &Value) -> Option<&str> {
    lease
        .pointer("/spec/holderIdentity")
        .and_then(Value::as_str)
}

#[must_use]
pub fn scope_of(lease: &Value) -> BTreeSet<String> {
    lease
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(SCOPE))
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

#[must_use]
pub fn name_of(lease: &Value) -> &str {
    lease
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[must_use]
pub fn run_of(lease: &Value) -> Option<&str> {
    lease
        .pointer("/metadata/labels")
        .and_then(|l| l.get(RUN))
        .and_then(Value::as_str)
}

#[must_use]
pub fn expired(lease: &Value, now: DateTime<Utc>) -> bool {
    expires_at(lease).is_none_or(|t| t <= now)
}

#[must_use]
pub fn conflicts(leases: &[Value], ours: &LeaseRequest, now: DateTime<Utc>) -> Vec<String> {
    leases
        .iter()
        .filter(|l| name_of(l) != ours.name && !expired(l, now))
        .filter_map(|l| {
            let overlap: Vec<String> = scope_of(l).intersection(&ours.scope).cloned().collect();
            (!overlap.is_empty()).then(|| {
                format!(
                    "lease `{}` held by `{}` until {} covers {}",
                    name_of(l),
                    holder(l).unwrap_or("?"),
                    expires_at(l).map_or_else(|| "?".into(), micro),
                    overlap.join(", ")
                )
            })
        })
        .collect()
}

#[must_use]
pub fn held_by(lease: &Value, operator: &str) -> bool {
    holder(lease).is_some_and(|h| h.split_once('/').is_some_and(|(o, _)| o == operator))
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RunPlan {
    pub run: String,
    pub operator: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub lease: Option<LeaseRef>,
    pub spec: VisitaSpec,
    #[serde(default)]
    pub placed: Vec<String>,
    #[serde(default)]
    pub ledger: Value,
    #[serde(default)]
    pub finished: bool,
}

#[must_use]
pub fn run_dir(state_home: &Path, run: &str) -> PathBuf {
    state_home.join("ensaio/visita").join(run)
}

pub fn write_plan(dir: &Path, plan: &RunPlan) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{PLAN}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(plan).unwrap_or_default())
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(PLAN)).map_err(|e| format!("{}: {e}", dir.display()))
}

pub fn read_plan(dir: &Path) -> Result<RunPlan, String> {
    let p = dir.join(PLAN);
    let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", p.display()))
}
