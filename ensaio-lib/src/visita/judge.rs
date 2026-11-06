//! Pure judges for the operator-side assertion kinds.
//!
//! Each takes what was read and returns a [`Verdict`] through
//! [`Verdict::judge`], so the derived-verdict law holds here as it does for
//! every other kind: an empty subject set is `Vacuous`, never a pass.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::field::{FieldCheck, FieldPredicate};
use crate::types::{HpaPolicy, JobExpect, Observation, Observations, Presence, Verdict};

// ── rollout ────────────────────────────────────────────────────────────

/// One sample of one Deployment's rollout state.
///
/// Also reads the hand-run recorder's JSONL (`ts`, `gen`, null counts, no
/// `observed_generation`), so an earlier manual recording is judged by the
/// same rule as a visita's own.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RolloutSample {
    #[serde(alias = "ts")]
    pub at: String,
    pub name: String,
    #[serde(alias = "gen", deserialize_with = "null_zero")]
    pub generation: i64,
    /// Absent from a manual recording: taken as caught up, so completion is
    /// judged by the counts alone.
    #[serde(default = "caught_up", deserialize_with = "null_zero")]
    pub observed_generation: i64,
    #[serde(default, deserialize_with = "null_zero")]
    pub desired: i64,
    #[serde(default, deserialize_with = "null_zero")]
    pub replicas: i64,
    #[serde(default, deserialize_with = "null_zero")]
    pub updated: i64,
    #[serde(default, deserialize_with = "null_zero")]
    pub ready: i64,
    #[serde(default, deserialize_with = "null_zero")]
    pub available: i64,
    #[serde(default, deserialize_with = "null_zero")]
    pub unavailable: i64,
    #[serde(default)]
    pub image: String,
    /// The Deployment's `spec.strategy.rollingUpdate` at this sample — the
    /// hand recorder's `strategy` key. Decides whether the generation is on
    /// the strategy under test.
    #[serde(default)]
    pub strategy: Option<Value>,
}

fn caught_up() -> i64 {
    i64::MAX
}

fn null_zero<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(Option::<i64>::deserialize(d)?.unwrap_or(0))
}

fn int(v: &Value, ptr: &str) -> i64 {
    v.pointer(ptr).and_then(Value::as_i64).unwrap_or(0)
}

impl RolloutSample {
    /// Read a sample off a Deployment object.
    #[must_use]
    pub fn from_deployment(d: &Value, at: &str) -> Self {
        Self {
            at: at.to_string(),
            name: d
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            generation: int(d, "/metadata/generation"),
            observed_generation: int(d, "/status/observedGeneration"),
            desired: int(d, "/spec/replicas"),
            replicas: int(d, "/status/replicas"),
            updated: int(d, "/status/updatedReplicas"),
            ready: int(d, "/status/readyReplicas"),
            available: int(d, "/status/availableReplicas"),
            unavailable: int(d, "/status/unavailableReplicas"),
            image: d
                .pointer("/spec/template/spec/containers/0/image")
                .and_then(Value::as_str)
                .and_then(|i| i.rsplit(':').next())
                .unwrap_or_default()
                .to_string(),
            strategy: d.pointer("/spec/strategy/rollingUpdate").cloned(),
        }
    }

    /// The rollout of this sample's generation has finished.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.observed_generation >= self.generation
            && self.updated == self.desired
            && self.replicas == self.desired
            && self.available == self.desired
    }
}

/// What a sequence of rollout samples says, typed.
///
/// A [`RolloutEvent::Scale`] is its own event: an HPA changing `desired` adds or
/// removes pods under any strategy, so it is neither surge nor unavailability,
/// and folding it into either reports a breach nobody caused.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum RolloutEvent {
    /// `desired` changed between two samples.
    Scale {
        name: String,
        at: String,
        generation: i64,
        from: i64,
        to: i64,
    },
    /// More pods than `max(desired, previous desired) + max_surge`.
    Surge {
        name: String,
        at: String,
        generation: i64,
        replicas: i64,
        desired: i64,
        max_surge: i64,
    },
    /// `available` fell below `min(previous available, desired) - max_unavailable`:
    /// a pod that was serving was taken away before its replacement was Ready.
    Unavailable {
        name: String,
        at: String,
        generation: i64,
        available: i64,
        previous_available: i64,
        desired: i64,
        max_unavailable: i64,
    },
    /// The last sample of a judged generation is not a finished rollout.
    Incomplete {
        name: String,
        generation: i64,
        updated: i64,
        available: i64,
        desired: i64,
    },
    /// A generation that rolled under a different strategy than the one under
    /// test (a reloader restart, the restoring sync). Reported, not judged.
    OtherStrategy {
        name: String,
        generation: i64,
        strategy: Value,
    },
    /// Every judged generation of this Deployment was seen only at rest — no
    /// sample caught it mid-rollout. The bound was never tested, so this is
    /// not a pass (and not a breach): the verdict routes to Inconclusive.
    Unobserved {
        name: String,
        generations: Vec<i64>,
        samples: usize,
        /// The first sample already showed the expected build, complete: the
        /// rollout finished before recording began.
        already_on_build: bool,
    },
    /// A generation whose image is not the one the trial deploys (a restart
    /// of the old build while the pipeline is still building). Reported, not
    /// judged.
    OtherImage {
        name: String,
        generation: i64,
        image: String,
    },
}

impl RolloutEvent {
    /// Whether this event is evidence against the bound.
    #[must_use]
    pub fn is_breach(&self) -> bool {
        match self {
            Self::Surge { .. } | Self::Unavailable { .. } | Self::Incomplete { .. } => true,
            Self::Scale { .. }
            | Self::OtherStrategy { .. }
            | Self::OtherImage { .. }
            | Self::Unobserved { .. } => false,
        }
    }

    fn observation(&self) -> Observation {
        Observation::new(match self {
            Self::Scale {
                name,
                at,
                generation,
                from,
                to,
            } => {
                format!("scale {name} gen {generation} @{at}: desired {from} -> {to}")
            }
            Self::Surge {
                name,
                at,
                generation,
                replicas,
                desired,
                max_surge,
            } => format!(
                "surge {name} gen {generation} @{at}: {replicas} replicas for {desired} desired exceeds surge {max_surge}"
            ),
            Self::Unavailable {
                name,
                at,
                generation,
                available,
                previous_available,
                desired,
                max_unavailable,
            } => format!(
                "unavailable {name} gen {generation} @{at}: {available} available after {previous_available} (desired {desired}) exceeds unavailable {max_unavailable}"
            ),
            Self::Incomplete {
                name,
                generation,
                updated,
                available,
                desired,
            } => format!(
                "incomplete {name} gen {generation}: {updated} updated, {available} available of {desired} when the window closed"
            ),
            Self::OtherStrategy {
                name,
                generation,
                strategy,
            } => {
                format!("other-strategy {name} gen {generation}: {strategy} (not judged)")
            }
            Self::Unobserved {
                name,
                generations,
                samples,
                already_on_build,
            } => format!(
                "unobserved {name} gen {generations:?}: {samples} samples, none mid-rollout{} — the bound was not tested",
                if *already_on_build {
                    ", already on the expected build when recording began"
                } else {
                    ""
                }
            ),
            Self::OtherImage {
                name,
                generation,
                image,
            } => {
                format!("other-image {name} gen {generation}: {image} (not judged)")
            }
        })
    }
}

fn int_or_zero(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64)
}

/// Whether a sample's declared `rollingUpdate` is the strategy under test.
/// A sample that does not carry its strategy is judged (it cannot be excused).
fn on_trial_strategy(s: &RolloutSample, max_surge: i64, max_unavailable: i64) -> bool {
    s.strategy.as_ref().is_none_or(|st| {
        int_or_zero(st.get("maxSurge")) == Some(max_surge)
            && int_or_zero(st.get("maxUnavailable")) == Some(max_unavailable)
    })
}

/// A sample taken mid-rollout: pods not yet updated, more pods than desired,
/// or fewer available than desired. A record with none of these watched only
/// the finished state.
#[must_use]
pub fn in_transition(s: &RolloutSample) -> bool {
    s.updated < s.replicas || s.replicas > s.desired || s.available < s.desired
}

/// Whether a sample runs the image under test: its tag starts with `expect`.
/// No expectation matches everything.
#[must_use]
pub fn image_matches(s: &RolloutSample, expect: Option<&str>) -> bool {
    expect.is_none_or(|e| s.image.starts_with(e))
}

/// The Deployment's FIRST recorded sample is already the expected build,
/// complete: its rollout finished before recording began, so nothing that
/// follows can show it in motion. Only meaningful with an expected image.
fn already_on_build(series: &[&RolloutSample], expect: Option<&str>) -> bool {
    expect.is_some()
        && series
            .first()
            .is_some_and(|f| image_matches(f, expect) && f.complete())
}

/// Whether the watch may stop: every named Deployment's latest sample is a
/// COMPLETE generation newer than its baseline, on the expected image.
///
/// A new generation on some other image (the old build restarted while the
/// pipeline is still building) does not settle the watch — that is what made
/// an awaited-too-early observation return vacuous after three minutes of a
/// forty-minute window.
#[must_use]
pub fn rollout_settled(
    samples: &[RolloutSample],
    names: &[String],
    baseline: &std::collections::BTreeMap<String, i64>,
    expect: Option<&str>,
) -> bool {
    !names.is_empty()
        && names.iter().all(|n| {
            let series: Vec<&RolloutSample> = samples.iter().filter(|s| &s.name == n).collect();
            // Already done before recording began: waiting cannot help, stop
            // now and let the judge say Unobserved (not after the window).
            already_on_build(&series, expect)
                || series.last().is_some_and(|s| {
                    s.generation > baseline.get(n).copied().unwrap_or(i64::MAX)
                        && image_matches(s, expect)
                        && s.complete()
                })
        })
}

/// Every event in every generation newer than `baseline`.
///
/// Within a generation on the trial strategy, per sample `t` after `t-1`:
/// `replicas_t <= max(desired_t, desired_{t-1}) + max_surge` and
/// `available_t >= min(available_{t-1}, desired_t) - max_unavailable`, with
/// `t-1` the actual previous sample of that Deployment whatever its
/// generation. The first sample of a record has no predecessor: it is the
/// reference, never held to `desired`.
#[must_use]
pub fn rollout_events(
    samples: &[RolloutSample],
    baseline: &std::collections::BTreeMap<String, i64>,
    max_surge: i64,
    max_unavailable: i64,
    expect_image: Option<&str>,
) -> Vec<RolloutEvent> {
    let mut events = Vec::new();
    let mut names: Vec<&str> = samples.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let base = baseline.get(name).copied().unwrap_or(i64::MAX);
        let series: Vec<&RolloutSample> = samples.iter().filter(|s| s.name == name).collect();
        let mut gens: Vec<i64> = series
            .iter()
            .map(|s| s.generation)
            .filter(|g| *g > base)
            .collect();
        gens.dedup();
        for (i, s) in series.iter().enumerate() {
            if s.generation <= base {
                continue;
            }
            let prev = i.checked_sub(1).map(|j| series[j]);
            if let Some(p) = prev {
                if p.desired != s.desired {
                    events.push(RolloutEvent::Scale {
                        name: name.into(),
                        at: s.at.clone(),
                        generation: s.generation,
                        from: p.desired,
                        to: s.desired,
                    });
                }
            }
            if !on_trial_strategy(s, max_surge, max_unavailable) || !image_matches(s, expect_image)
            {
                continue;
            }
            let desired_cap = prev.map_or(s.desired, |p| p.desired.max(s.desired));
            if s.replicas - desired_cap > max_surge {
                events.push(RolloutEvent::Surge {
                    name: name.into(),
                    at: s.at.clone(),
                    generation: s.generation,
                    replicas: s.replicas,
                    desired: s.desired,
                    max_surge,
                });
            }
            // The FIRST sample of a record has no predecessor and is the
            // reference, not a judgement: hold it to its own `available`. An
            // earlier fixture may already have moved `desired` (an HPA raised
            // 3 -> 8 before a restart storm was recorded): 3 available of 8
            // desired is then five pods STARTING, and holding the first sample
            // to `desired` read it as five removed (load A/B, 2026-10-01).
            let previous_available = prev.map_or(s.available, |p| p.available);
            if previous_available.min(s.desired) - s.available > max_unavailable {
                events.push(RolloutEvent::Unavailable {
                    name: name.into(),
                    at: s.at.clone(),
                    generation: s.generation,
                    available: s.available,
                    previous_available,
                    desired: s.desired,
                    max_unavailable,
                });
            }
        }
        let judged_gens: Vec<i64> = gens
            .iter()
            .copied()
            .filter(|g| {
                series
                    .iter()
                    .filter(|s| s.generation == *g)
                    .last()
                    .is_some_and(|l| {
                        on_trial_strategy(l, max_surge, max_unavailable)
                            && image_matches(l, expect_image)
                    })
            })
            .collect();
        let judged_samples: Vec<&&RolloutSample> = series
            .iter()
            .filter(|s| judged_gens.contains(&s.generation))
            .collect();
        let already = already_on_build(&series, expect_image);
        if (!judged_gens.is_empty() || already) && !judged_samples.iter().any(|s| in_transition(s))
        {
            events.push(RolloutEvent::Unobserved {
                name: name.into(),
                generations: judged_gens.clone(),
                samples: if judged_samples.is_empty() {
                    series.len()
                } else {
                    judged_samples.len()
                },
                already_on_build: already,
            });
        }
        for g in gens {
            let in_gen: Vec<&&RolloutSample> =
                series.iter().filter(|s| s.generation == g).collect();
            let Some(last) = in_gen.last() else { continue };
            if !on_trial_strategy(last, max_surge, max_unavailable) {
                events.push(RolloutEvent::OtherStrategy {
                    name: name.into(),
                    generation: g,
                    strategy: last.strategy.clone().unwrap_or(Value::Null),
                });
                continue;
            }
            if !image_matches(last, expect_image) {
                events.push(RolloutEvent::OtherImage {
                    name: name.into(),
                    generation: g,
                    image: last.image.clone(),
                });
                continue;
            }
            // Only the name's final generation must have finished: an earlier
            // one was superseded, which is not a failure to finish.
            let is_final = series.last().is_some_and(|l| l.generation == g);
            if is_final && !last.complete() {
                events.push(RolloutEvent::Incomplete {
                    name: name.into(),
                    generation: g,
                    updated: last.updated,
                    available: last.available,
                    desired: last.desired,
                });
            }
        }
    }
    events
}

/// Judge the rollout: subjects are the generations judged on the trial
/// strategy (plus every scale event, as context), findings the breaches. No
/// generation on the trial strategy in the window is `Vacuous`.
#[must_use]
pub fn rollout(
    samples: &[RolloutSample],
    baseline: &std::collections::BTreeMap<String, i64>,
    max_surge: i64,
    max_unavailable: i64,
    expect_image: Option<&str>,
) -> Verdict {
    let events = rollout_events(samples, baseline, max_surge, max_unavailable, expect_image);
    let findings: Vec<Observation> = events
        .iter()
        .filter(|e| e.is_breach())
        .map(RolloutEvent::observation)
        .collect();
    // Unobserved is decided BEFORE emptiness: a Deployment already on the
    // expected build has no judged generation, and must read as "not tested",
    // never as "nothing in scope".
    let unobserved: Vec<(&str, bool)> = events
        .iter()
        .filter_map(|e| match e {
            RolloutEvent::Unobserved {
                name,
                already_on_build,
                ..
            } => Some((name.as_str(), *already_on_build)),
            _ => None,
        })
        .collect();
    if findings.is_empty() && !unobserved.is_empty() {
        let list = |already: bool| {
            unobserved
                .iter()
                .filter(|(_, a)| *a == already)
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut why = Vec::new();
        if unobserved.iter().any(|(_, a)| *a) {
            why.push(format!(
                "{}: already on the expected build when recording began",
                list(true)
            ));
        }
        if unobserved.iter().any(|(_, a)| !*a) {
            why.push(format!(
                "{}: every sample of the judged generation(s) is at rest",
                list(false)
            ));
        }
        return Verdict::Skipped {
            reason: format!(
                "rollout unobserved — {}; the bound was never tested; start the record earlier (:record-from)",
                why.join("; ")
            ),
        };
    }
    // One subject per DEPLOYMENT (the subject count is how many were
    // watched), carrying each judged generation once and its context events.
    let mut names: Vec<&str> = samples.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let mut subjects = Vec::new();
    for name in names {
        let mut gens: Vec<i64> = samples
            .iter()
            .filter(|s| s.name == name)
            .filter(|s| s.generation > baseline.get(name).copied().unwrap_or(i64::MAX))
            .filter(|s| {
                on_trial_strategy(s, max_surge, max_unavailable) && image_matches(s, expect_image)
            })
            .map(|s| s.generation)
            .collect();
        gens.sort_unstable();
        gens.dedup();
        if gens.is_empty() {
            continue;
        }
        let per_gen: Vec<String> = gens
            .iter()
            .map(|g| {
                let in_gen: Vec<&RolloutSample> = samples
                    .iter()
                    .filter(|s| s.name == name && s.generation == *g)
                    .collect();
                let moving: Vec<&&RolloutSample> =
                    in_gen.iter().filter(|s| in_transition(s)).collect();
                let span = match (moving.first(), moving.last()) {
                    (Some(f), Some(l)) => format!(" {}..{}", f.at, l.at),
                    _ => String::new(),
                };
                format!(
                    "gen {g}: {} samples, {} mid-rollout{span}",
                    in_gen.len(),
                    moving.len()
                )
            })
            .collect();
        let context: Vec<String> = events
            .iter()
            .filter(|e| !e.is_breach())
            .filter(|e| match e {
                RolloutEvent::Scale { name: n, .. }
                | RolloutEvent::OtherStrategy { name: n, .. }
                | RolloutEvent::OtherImage { name: n, .. } => n == name,
                _ => false,
            })
            .map(|e| e.observation().as_str().to_string())
            .collect();
        subjects.push(Observation::new(format!(
            "{name}: {}{}",
            per_gen.join("; "),
            if context.is_empty() {
                String::new()
            } else {
                format!(" | {}", context.join("; "))
            }
        )));
    }
    Verdict::judge(Observations::scope(subjects), findings)
}

// ── pod-scoped reads ───────────────────────────────────────────────────

/// One pod's read, typed. A pod that vanished between being listed and being
/// read (an HPA scale-down, a rollout replacing it) is its own arm: it was
/// never examined, so it is neither a subject nor a failure — it is skipped
/// and counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodRead {
    Text(String),
    Vanished,
    Failed(String),
}

fn skipped_note(reads: &[(String, PodRead)]) -> Option<Observation> {
    let gone: Vec<&str> = reads
        .iter()
        .filter(|(_, r)| *r == PodRead::Vanished)
        .map(|(p, _)| p.as_str())
        .collect();
    (!gone.is_empty()).then(|| {
        Observation::new(format!(
            "skipped {} pod(s) that vanished mid-read: {}",
            gone.len(),
            gone.join(", ")
        ))
    })
}

fn all_failed(reads: &[(String, PodRead)]) -> Option<Verdict> {
    let failed: Vec<String> = reads
        .iter()
        .filter_map(|(p, r)| match r {
            PodRead::Failed(e) => Some(format!("{p}: {e}")),
            PodRead::Text(_) | PodRead::Vanished => None,
        })
        .collect();
    (!reads.is_empty() && failed.len() == reads.len()).then(|| Verdict::Errored {
        message: format!("no pod could be read: {}", failed.join("; ")),
    })
}

// ── logs ───────────────────────────────────────────────────────────────

/// Count literal patterns in each pod's log.
///
/// `Absent`: every matching line is a finding. `Present`: a pattern with zero
/// matches across the pods read is a finding. Zero lines read is `Vacuous` —
/// "the pods said nothing" is not "the pods said nothing bad". A pod that
/// could not be read (other than vanishing) is a finding; every pod failing
/// is `Errored`.
#[must_use]
pub fn logs(
    reads: &[(String, PodRead)],
    patterns: &[String],
    presence: Presence,
    min_count: u64,
    match_any: bool,
) -> Verdict {
    if let Some(v) = all_failed(reads) {
        return v;
    }
    let mut lines: Vec<(&str, &str)> = Vec::new();
    for (pod, r) in reads {
        if let PodRead::Text(t) = r {
            lines.extend(
                t.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| (pod.as_str(), l)),
            );
        }
    }
    if lines.is_empty() {
        return Verdict::judge(Observations::scope(Vec::new()), Vec::new());
    }
    let mut subjects: Vec<Observation> = reads
        .iter()
        .filter_map(|(p, r)| match r {
            PodRead::Text(t) => Some(Observation::new(format!(
                "{p}: {} lines",
                t.lines().filter(|l| !l.trim().is_empty()).count()
            ))),
            PodRead::Vanished | PodRead::Failed(_) => None,
        })
        .collect();
    subjects.extend(skipped_note(reads));
    let mut findings: Vec<Observation> = reads
        .iter()
        .filter_map(|(p, r)| match r {
            PodRead::Failed(e) => Some(Observation::new(format!("{p}: unreadable: {e}"))),
            PodRead::Text(_) | PodRead::Vanished => None,
        })
        .collect();
    let mut short: Vec<Observation> = Vec::new();
    for pat in patterns {
        let hits: Vec<&(&str, &str)> = lines
            .iter()
            .filter(|(_, l)| l.contains(pat.as_str()))
            .collect();
        subjects.push(Observation::new(format!("`{pat}`: {} matches", hits.len())));
        match presence {
            Presence::Absent => {
                for (pod, l) in hits.iter().take(20) {
                    findings.push(Observation::new(format!("`{pat}` present in {pod}: {l}")));
                }
                if hits.len() > 20 {
                    findings.push(Observation::new(format!(
                        "`{pat}`: {} more matches not listed",
                        hits.len() - 20
                    )));
                }
            }
            Presence::Present => {
                if (hits.len() as u64) < min_count.max(1) {
                    short.push(Observation::new(format!(
                        "`{pat}`: {} match(es) in {} lines, fewer than the {} required",
                        hits.len(),
                        lines.len(),
                        min_count.max(1)
                    )));
                }
            }
        }
    }
    // `present`: every pattern must reach the count, or — with `match_any` —
    // at least one must.
    if presence == Presence::Present && !(match_any && short.len() < patterns.len()) {
        findings.extend(short);
    }
    Verdict::judge(Observations::scope(subjects), findings)
}

// ── metrics ────────────────────────────────────────────────────────────

/// Samples of `metric` in one Prometheus text exposition.
#[must_use]
pub fn metric_samples(text: &str, metric: &str) -> Vec<(String, f64)> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let rest = l.strip_prefix(metric)?;
            if !(rest.starts_with('{') || rest.starts_with(' ')) {
                return None;
            }
            let (series, value) = l.rsplit_once(' ')?;
            value.parse::<f64>().ok().map(|v| (series.to_string(), v))
        })
        .collect()
}

/// Judge a metric across pods: present, and inside `[min, max]` when given.
/// An unreadable pod is named as unreadable, never as absent; a vanished pod
/// is skipped and counted; every pod unreadable is `Errored`; nothing read is
/// `Vacuous`.
#[must_use]
pub fn metrics(
    reads: &[(String, PodRead)],
    metric: &str,
    min: Option<f64>,
    max: Option<f64>,
) -> Verdict {
    if let Some(v) = all_failed(reads) {
        return v;
    }
    let mut subjects = Vec::new();
    let mut findings = Vec::new();
    for (pod, r) in reads {
        match r {
            PodRead::Vanished => {}
            PodRead::Failed(e) => {
                findings.push(Observation::new(format!("{pod}: unreadable: {e}")))
            }
            PodRead::Text(text) => {
                let samples = metric_samples(text, metric);
                if samples.is_empty() {
                    subjects.push(Observation::new(format!("{pod}: {metric} absent")));
                    findings.push(Observation::new(format!("{pod}: {metric} absent")));
                }
                for (series, v) in samples {
                    subjects.push(Observation::new(format!("{pod}: {series} = {v}")));
                    if min.is_some_and(|m| v < m) || max.is_some_and(|m| v > m) {
                        findings.push(Observation::new(format!(
                            "{pod}: {series} = {v} outside [{}, {}]",
                            min.map_or("-inf".into(), |m| m.to_string()),
                            max.map_or("+inf".into(), |m| m.to_string())
                        )));
                    }
                }
            }
        }
    }
    if !subjects.is_empty() {
        subjects.extend(skipped_note(reads));
    }
    Verdict::judge(Observations::scope(subjects), findings)
}

// ── exec metric ────────────────────────────────────────────────────────

/// Judge a recorded command's numbers. `All`: every parsed sample in
/// `[min, max]`. `Max` / `Min`: the window's largest / smallest in range — a
/// "saturation was reached" claim is `max >= threshold`. Samples that did not
/// parse are counted and named, never dropped silently; zero parsed samples is
/// `Vacuous`.
#[must_use]
pub fn exec_metric(
    samples: &[super::env::ExecSample],
    stat: crate::types::Stat,
    min: Option<f64>,
    max: Option<f64>,
) -> Verdict {
    use crate::types::Stat;
    let parsed: Vec<(&str, f64)> = samples
        .iter()
        .filter_map(|s| s.value.map(|v| (s.at.as_str(), v)))
        .collect();
    let unparsed = samples.len() - parsed.len();
    if parsed.is_empty() {
        return Verdict::judge(Observations::scope(Vec::new()), Vec::new());
    }
    let in_range = |v: f64| min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m);
    let bounds = format!(
        "[{}, {}]",
        min.map_or("-inf".into(), |m| m.to_string()),
        max.map_or("+inf".into(), |m| m.to_string())
    );
    let (lo, hi) = parsed
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (_, v)| {
            (lo.min(*v), hi.max(*v))
        });
    let mut subjects = vec![Observation::new(format!(
        "{} samples {}..{}, min {lo}, max {hi}{}",
        parsed.len(),
        parsed[0].0,
        parsed[parsed.len() - 1].0,
        if unparsed > 0 {
            format!(", {unparsed} unparseable")
        } else {
            String::new()
        }
    ))];
    let mut findings = Vec::new();
    match stat {
        Stat::All => {
            for (at, v) in &parsed {
                if !in_range(*v) {
                    findings.push(Observation::new(format!("@{at}: {v} outside {bounds}")));
                }
            }
        }
        Stat::Max => {
            subjects.push(Observation::new(format!("judged: window max {hi}")));
            if !in_range(hi) {
                findings.push(Observation::new(format!(
                    "window max {hi} outside {bounds}"
                )));
            }
        }
        Stat::Min => {
            subjects.push(Observation::new(format!("judged: window min {lo}")));
            if !in_range(lo) {
                findings.push(Observation::new(format!(
                    "window min {lo} outside {bounds}"
                )));
            }
        }
    }
    Verdict::judge(Observations::scope(subjects), findings)
}

// ── hpa ────────────────────────────────────────────────────────────────

/// Every named HPA carries `policy` in `spec.behavior.scaleUp.policies`.
#[must_use]
pub fn hpa(items: &[Value], names: &[String], policy: &HpaPolicy) -> Verdict {
    let mut subjects = Vec::new();
    let mut findings = Vec::new();
    for name in names {
        let Some(h) = items
            .iter()
            .find(|h| h.pointer("/metadata/name").and_then(Value::as_str) == Some(name))
        else {
            findings.push(Observation::new(format!("{name}: no such HPA")));
            continue;
        };
        let scale_up = h
            .pointer("/spec/behavior/scaleUp")
            .cloned()
            .unwrap_or(Value::Null);
        subjects.push(Observation::new(format!("{name}: scaleUp {scale_up}")));
        let has = scale_up
            .get("policies")
            .and_then(Value::as_array)
            .is_some_and(|ps| {
                ps.iter().any(|p| {
                    p.get("type").and_then(Value::as_str) == Some(policy.kind.as_str())
                        && p.get("value").and_then(Value::as_i64) == Some(policy.value)
                        && p.get("periodSeconds").and_then(Value::as_i64)
                            == Some(policy.period_seconds)
                })
            });
        if !has {
            findings.push(Observation::new(format!(
                "{name}: no scaleUp policy {} {} / {}s",
                policy.kind, policy.value, policy.period_seconds
            )));
        }
    }
    Verdict::judge(Observations::scope(subjects), findings)
}

// ── rabbitmq ───────────────────────────────────────────────────────────

/// `10.1.2.3` → `{0,0,0,0,0,65535,2561,515}`, the IPv4-mapped IPv6 tuple
/// RabbitMQ reports as `peer_host` for a dual-stack listener.
///
/// # Errors
///
/// When the address is not dotted-quad IPv4.
pub fn ipv4_mapped_tuple(ip: &str) -> Result<String, String> {
    let addr: std::net::Ipv4Addr = ip.parse().map_err(|e| format!("`{ip}`: {e}"))?;
    let [a, b, c, d] = addr.octets();
    Ok(format!(
        "{{0,0,0,0,0,65535,{},{}}}",
        u32::from(a) * 256 + u32::from(b),
        u32::from(c) * 256 + u32::from(d)
    ))
}

/// The one cluster-wide eval that closes the selected peers' connections on
/// one vhost and returns how many it closed.
#[must_use]
pub fn close_connections_expr(tuples: &[String], vhost: &str) -> String {
    format!(
        "Ips=[{}], L=[P || [{{pid,P}},{{peer_host,H}},{{vhost,V}}] <- rabbit_networking:connection_info_all([pid,peer_host,vhost]), V =:= <<\"{vhost}\">>, lists:member(H,Ips)], [rabbit_networking:close_connection(P, \"resilience test\") || P <- L], length(L).",
        tuples.join(",")
    )
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "seen", content = "value", rename_all = "kebab-case")]
pub enum Seen {
    Object(Value),
    Absent(String),
}

impl Seen {
    #[must_use]
    pub fn root(&self) -> Option<&Value> {
        match self {
            Self::Object(v) => Some(v),
            Self::Absent(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ObjectRead {
    Seen(Seen),
    Blind { reason: String, lasting: bool },
}

impl ObjectRead {
    #[must_use]
    pub fn classify(read: Result<Option<Value>, String>) -> Self {
        match read {
            Ok(Some(v)) => Self::Seen(Seen::Object(v)),
            Ok(None) => Self::Seen(Seen::Absent("NotFound".into())),
            Err(e) if e.contains("doesn't have a resource type") => {
                Self::Seen(Seen::Absent("no such resource type".into()))
            }
            Err(e) => Self::Blind {
                lasting: e.contains("(Forbidden)")
                    || e.contains("(Unauthorized)")
                    || e.contains("must be logged in"),
                reason: e,
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum ObjectWatch {
    Settled(Seen),
    TimedOut { after: String, last: Seen },
    Blind { reason: String, last: Option<Seen> },
}

#[must_use]
pub fn holds_all(seen: &Seen, predicates: &[FieldPredicate]) -> bool {
    predicates.iter().all(|p| p.check(seen.root()).held)
}

#[must_use]
pub fn selected_fields(
    seen: &Seen,
    predicates: &[&FieldPredicate],
) -> serde_json::Map<String, Value> {
    predicates
        .iter()
        .map(|p| {
            (
                p.path.to_string(),
                p.check(seen.root()).observed.unwrap_or(Value::Null),
            )
        })
        .collect()
}

fn values_line(seen: &Seen, predicates: &[&FieldPredicate]) -> String {
    match seen {
        Seen::Absent(why) => format!("object absent ({why})"),
        Seen::Object(_) => predicates
            .iter()
            .map(|p| format!("{} = {}", p.path, p.check(seen.root()).observed_text()))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn miss(prefix: &str, c: &FieldCheck) -> Observation {
    Observation::new(format!(
        "{prefix}{}: observed {}, wanted {}",
        c.predicate.path,
        c.observed_text(),
        c.predicate.test
    ))
}

#[must_use]
pub fn object_fields(
    subject: &str,
    watch: &ObjectWatch,
    wait_until: &[FieldPredicate],
    expect: &[FieldPredicate],
) -> Verdict {
    let every: Vec<&FieldPredicate> = wait_until.iter().chain(expect).collect();
    let state = |seen: &Seen| -> Vec<Observation> {
        let head = match seen {
            Seen::Object(_) => Observation::new(format!("{subject}: read")),
            Seen::Absent(why) => Observation::new(format!("{subject}: absent ({why})")),
        };
        std::iter::once(head)
            .chain(wait_until.iter().map(|p| {
                let c = p.check(seen.root());
                Observation::new(format!(
                    "wait-until {}: {} (wanted {})",
                    p.path,
                    c.observed_text(),
                    p.test
                ))
            }))
            .chain(expect.iter().map(|p| {
                let c = p.check(seen.root());
                Observation::new(format!(
                    "{}: {} (wanted {})",
                    p.path,
                    c.observed_text(),
                    p.test
                ))
            }))
            .collect()
    };
    match watch {
        ObjectWatch::Settled(seen) => {
            let findings = expect
                .iter()
                .map(|p| p.check(seen.root()))
                .filter(|c| !c.held)
                .map(|c| miss("", &c))
                .collect();
            Verdict::judge(Observations::scope(state(seen)), findings)
        }
        ObjectWatch::TimedOut { after, last } => {
            let head = match last {
                Seen::Object(_) => format!("timed out after {after} before :wait-until held"),
                Seen::Absent(why) => format!(
                    "timed out after {after} before :wait-until held: {subject} never found ({why})"
                ),
            };
            let findings = std::iter::once(Observation::new(head))
                .chain(
                    wait_until
                        .iter()
                        .map(|p| p.check(last.root()))
                        .filter(|c| !c.held)
                        .map(|c| miss("wait-until ", &c)),
                )
                .collect();
            Verdict::judge(Observations::scope(state(last)), findings)
        }
        ObjectWatch::Blind { reason, last } => Verdict::Skipped {
            reason: format!(
                "unobserved (blind): {subject} could not be read: {}{}",
                reason.trim(),
                last.as_ref().map_or_else(String::new, |s| format!(
                    "; last observed: {}",
                    values_line(s, &every)
                ))
            ),
        },
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct RunJob {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub runner: String,
}

#[must_use]
pub fn run_jobs(answer: &Value) -> Vec<RunJob> {
    let text = |j: &Value, k: &str| {
        j.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    answer
        .get("jobs")
        .and_then(Value::as_array)
        .map(|jobs| {
            jobs.iter()
                .map(|j| RunJob {
                    id: j.get("id").and_then(Value::as_i64).unwrap_or_default(),
                    name: text(j, "name"),
                    status: text(j, "status"),
                    conclusion: text(j, "conclusion"),
                    runner: text(j, "runner_name"),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[must_use]
pub fn job<'a>(jobs: &'a [RunJob], name: &str) -> Option<&'a RunJob> {
    jobs.iter().find(|j| j.name == name)
}

#[must_use]
pub fn expected_completed(jobs: &[RunJob], expect: &[JobExpect]) -> bool {
    expect
        .iter()
        .all(|e| job(jobs, &e.job).is_some_and(|j| j.status == "completed"))
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum JobsWatch {
    Settled { jobs: Vec<RunJob> },
    TimedOut { after: String, jobs: Vec<RunJob> },
    Blind { reason: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobTexts {
    pub logs: std::collections::BTreeMap<String, String>,
    pub annotations: std::collections::BTreeMap<String, Vec<String>>,
}

fn job_line(e: &JobExpect, found: Option<&RunJob>) -> Observation {
    Observation::new(match found {
        None => format!("job `{}`: absent from the run", e.job),
        Some(j) => format!(
            "job `{}`: {} {} on runner {}",
            j.name,
            j.status,
            if j.conclusion.is_empty() {
                "-"
            } else {
                &j.conclusion
            },
            if j.runner.is_empty() { "-" } else { &j.runner }
        ),
    })
}

fn job_findings(e: &JobExpect, j: &RunJob, texts: &JobTexts) -> Vec<Observation> {
    let mut out = Vec::new();
    if let Some(want) = &e.conclusion
        && &j.conclusion != want
    {
        out.push(Observation::new(format!(
            "job `{}`: concluded {}, wanted {want}",
            e.job, j.conclusion
        )));
    }
    let log = texts.logs.get(&e.job);
    for t in e.log_contains.iter().flatten() {
        if !log.is_some_and(|l| l.contains(t.as_str())) {
            out.push(Observation::new(format!(
                "job `{}`: log does not contain `{t}`",
                e.job
            )));
        }
    }
    let notes = texts.annotations.get(&e.job);
    for t in e.annotation_contains.iter().flatten() {
        if !notes.is_some_and(|ns| ns.iter().any(|n| n.contains(t.as_str()))) {
            out.push(Observation::new(format!(
                "job `{}`: no annotation contains `{t}`",
                e.job
            )));
        }
    }
    out
}

#[must_use]
pub fn workflow_jobs(
    subject: &str,
    watch: &JobsWatch,
    expect: &[JobExpect],
    texts: &JobTexts,
) -> Verdict {
    let (jobs, timed_out) = match watch {
        JobsWatch::Blind { reason } => {
            return Verdict::Skipped {
                reason: format!(
                    "unobserved (blind): {subject} could not be read: {}",
                    reason.trim()
                ),
            };
        }
        JobsWatch::Settled { jobs } => (jobs, None),
        JobsWatch::TimedOut { after, jobs } => (jobs, Some(after)),
    };
    let mut subjects = Vec::new();
    let mut findings = Vec::new();
    for e in expect {
        let found = job(jobs, &e.job);
        subjects.push(job_line(e, found));
        match found {
            None => findings.push(Observation::new(match timed_out {
                Some(after) => {
                    format!("job `{}` never appeared in {subject} within {after}", e.job)
                }
                None => format!("job `{}` is not in {subject}", e.job),
            })),
            Some(j) if j.status != "completed" => findings.push(Observation::new(format!(
                "job `{}` still {} after {}",
                e.job,
                j.status,
                timed_out.map_or("the watch", String::as_str)
            ))),
            Some(j) => findings.extend(job_findings(e, j, texts)),
        }
    }
    Verdict::judge(Observations::scope(subjects), findings)
}
