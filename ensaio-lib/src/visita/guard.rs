//! The guard's pure decisions: what a trip is, and how a pod's liveness probe
//! failures are counted from its events.
//!
//! The kubelet kills a container after `failureThreshold` consecutive liveness
//! failures, while Ready can stay True for most of that run. So the guard
//! counts those failures itself and trips `margin` short of the kill. The
//! threshold and period come from the pod's own probe; a container with no
//! liveness probe has no entry, so it contributes no signal rather than a
//! count of zero.

use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset};
use serde::Serialize;
use serde_json::Value;

/// What tripped the guard.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "signal")]
pub enum GuardSignal {
    Unready {
        for_secs: u64,
        bound_secs: u64,
    },
    Restarted {
        from: i64,
        to: i64,
    },
    Liveness {
        container: String,
        failures: u64,
        threshold: u32,
        margin: u32,
    },
}

/// One trip: the pod, the signal, and when it was seen.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct GuardTrip {
    pub pod: String,
    #[serde(flatten)]
    pub signal: GuardSignal,
    pub at: String,
}

impl std::fmt::Display for GuardTrip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pod = &self.pod;
        match &self.signal {
            GuardSignal::Unready {
                for_secs,
                bound_secs,
            } => {
                write!(
                    f,
                    "guard: pod `{pod}` not Ready for {for_secs}s (bound {bound_secs}s)"
                )?;
            }
            GuardSignal::Restarted { from, to } => {
                write!(f, "guard: pod `{pod}` restarted ({from} -> {to} restarts)")?;
            }
            GuardSignal::Liveness {
                container,
                failures,
                threshold,
                margin,
            } => {
                write!(
                    f,
                    "guard: pod `{pod}` container `{container}` failed its liveness probe {failures}/{threshold} consecutive times (margin {margin}: the kubelet kills it at {threshold})"
                )?;
            }
        }
        write!(f, " at {}", self.at)
    }
}

/// One container's liveness probe, with the API defaults the server applies
/// when a field is unset (`failureThreshold` 3, `periodSeconds` 10).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct LivenessProbe {
    pub container: String,
    pub failure_threshold: u32,
    pub period_seconds: u32,
}

impl LivenessProbe {
    /// How many consecutive failures trip the guard: `margin` short of the
    /// threshold, and never fewer than one.
    #[must_use]
    pub fn trips_at(&self, margin: u32) -> u64 {
        u64::from(self.failure_threshold.saturating_sub(margin).max(1))
    }
}

/// Every container of `pod` that declares a liveness probe.
#[must_use]
pub fn liveness_probes(pod: &Value) -> Vec<LivenessProbe> {
    let field = |p: &Value, k: &str, default: u32| {
        p.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(default)
    };
    pod.pointer("/spec/containers")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .filter_map(|c| {
                    let p = c.get("livenessProbe")?;
                    Some(LivenessProbe {
                        container: c.get("name").and_then(Value::as_str)?.to_string(),
                        failure_threshold: field(p, "failureThreshold", 3),
                        period_seconds: field(p, "periodSeconds", 10),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One `Unhealthy` event for a failed liveness probe. The kubelet aggregates
/// repeats into one event, so `count` is how many failures it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivenessFailure {
    pub id: String,
    pub pod: String,
    pub container: Option<String>,
    pub count: u64,
    pub last: Option<DateTime<FixedOffset>>,
}

/// The liveness failures in a `kubectl get events -o json` answer: reason
/// `Unhealthy`, message starting `Liveness probe failed`, on a Pod. `count`
/// is `count`, else `series.count`, else 1; the time is `lastTimestamp`, else
/// `series.lastObservedTime`, else `eventTime`.
#[must_use]
pub fn liveness_failures(events: &Value) -> Vec<LivenessFailure> {
    let text = |e: &Value, p: &str| e.pointer(p).and_then(Value::as_str).map(str::to_string);
    let time = |e: &Value| {
        [
            "/lastTimestamp",
            "/series/lastObservedTime",
            "/eventTime",
            "/firstTimestamp",
        ]
        .iter()
        .find_map(|p| e.pointer(p).and_then(Value::as_str))
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
    };
    super::env::list_items(events)
        .iter()
        .filter(|e| e.get("reason").and_then(Value::as_str) == Some("Unhealthy"))
        .filter(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .is_some_and(|m| m.starts_with("Liveness probe failed"))
        })
        .filter(|e| e.pointer("/involvedObject/kind").and_then(Value::as_str) == Some("Pod"))
        .filter_map(|e| {
            Some(LivenessFailure {
                id: text(e, "/metadata/uid").or_else(|| text(e, "/metadata/name"))?,
                pod: text(e, "/involvedObject/name")?,
                container: text(e, "/involvedObject/fieldPath").and_then(|f| {
                    f.strip_prefix("spec.containers{")
                        .and_then(|r| r.strip_suffix('}'))
                        .map(str::to_string)
                }),
                count: e
                    .get("count")
                    .and_then(Value::as_u64)
                    .or_else(|| e.pointer("/series/count").and_then(Value::as_u64))
                    .unwrap_or(1),
                last: time(e),
            })
        })
        .collect()
}

/// Consecutive liveness failures of one container since the guard started.
///
/// Failures already on record when the guard started are the baseline and
/// never count. A run of new failures continues the streak only when the
/// time since the previous one fits them back to back — at most one period
/// each, plus half a period of jitter. A longer gap means a probe in between
/// succeeded, which resets the kubelet's own count, so the streak restarts
/// from the new failures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LivenessStreak {
    seen: BTreeMap<String, u64>,
    last: Option<DateTime<FixedOffset>>,
    pub failures: u64,
}

impl LivenessStreak {
    #[must_use]
    pub fn baseline<'f>(on_record: impl IntoIterator<Item = &'f LivenessFailure>) -> Self {
        Self {
            seen: on_record
                .into_iter()
                .map(|f| (f.id.clone(), f.count))
                .collect(),
            last: None,
            failures: 0,
        }
    }

    /// Fold one read of this container's failures in; returns the streak.
    pub fn observe<'f>(
        &mut self,
        read: impl IntoIterator<Item = &'f LivenessFailure>,
        period_seconds: u32,
    ) -> u64 {
        let mut new = 0;
        let mut newest: Option<DateTime<FixedOffset>> = None;
        for f in read {
            let before = self.seen.get(&f.id).copied().unwrap_or(0);
            if f.count > before {
                new += f.count - before;
                newest = newest.max(f.last);
            }
            self.seen.insert(f.id.clone(), f.count.max(before));
        }
        if new == 0 {
            return self.failures;
        }
        let back_to_back = match (self.last, newest) {
            (Some(prev), Some(now)) => {
                let allowed = i64::try_from(new)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(i64::from(period_seconds))
                    + i64::from(period_seconds / 2);
                (now - prev).num_seconds() <= allowed
            }
            _ => true,
        };
        self.failures = if back_to_back {
            self.failures + new
        } else {
            new
        };
        if newest.is_some() {
            self.last = newest;
        }
        self.failures
    }
}
