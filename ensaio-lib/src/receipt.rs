//! `CertificationReceipt` — the PROVE-axis attestation object for an
//! Ensaio run.
//!
//! A typed, content-addressed **projection** over an
//! [`EnsaioOutcome`](crate::EnsaioOutcome)'s assertion results (never a
//! second side-effecting pass — `build` is pure and mock-testable). It
//! answers one question honestly: *did the environment carry the
//! declared topology?* — and reserves the word "certified" for what
//! earns it.
//!
//! Three structural honesty guarantees:
//!
//! 1. **Preconditions gate; known gaps do not.** `env_preconditions`
//!    (e.g. "the broker's mgmt API is reachable") gate the verdict —
//!    if one is unmet, the substrate wasn't fit to test the topology,
//!    so every artifact assertion routes to [`Verdict::Skipped`] and
//!    the verdict is [`CertVerdict::Inconclusive`], never `Certified`
//!    and never `NotCertified`. `env_known_gaps` (a pre-existing
//!    application service gap — a worker crash-loop, an incomplete
//!    service conf) are a *distinct field*: surfaced for context, **never
//!    gating**. So a rabbit-topology pass is reported *separately* from
//!    any application service gap — the whole point of the partition.
//!
//! 2. **`Skipped` is not `Falsified`.** An assertion the environment
//!    couldn't answer (an unmet precondition) is never rounded into a
//!    falsification. `Skipped → Inconclusive` is the honest verdict for
//!    "neither proven nor disproven."
//!
//! 3. **The receipt is content-addressed.** `receipt_sha` is an ordered
//!    digest of `(assertion_id, verdict_class)` — mirroring
//!    a bootstrap tool's `ApplyReceipt.ReceiptSHA` (an ordered digest
//!    of `(name, status, artifactSHA)`). It is a stable, diffable
//!    fingerprint of the *certified verdict pattern* — it changes iff
//!    some assertion's verdict class changes, so two runs of the same
//!    catalog against the same env produce the same sha. **Tier-honest:
//!    this is attestation-shaped PROVENANCE, not proof-of-correctness**,
//!    and it is BLAKE3 (the fleet content-addressing spine + this
//!    crate's existing dep), not sha256 — the field keeps the
//!    `receipt_sha` name for parity with `ApplyReceipt.ReceiptSHA`; the
//!    hash function is a one-line swap (per that README's own note).

use serde::{Deserialize, Serialize};

use crate::types::{AssertionKind, AssertionResult, Verdict};

/// One assertion's line in a certification receipt.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AssertionRecord {
    pub assertion_id: String,
    pub kind: AssertionKind,
    pub verdict: Verdict,
}

/// A pre-existing environment gap surfaced for context but which NEVER
/// gates the certification verdict — e.g. an application service conf
/// gap (a worker crash-loop, an incomplete service conf) that is
/// independent of the RabbitMQ broker's own health. A rabbit
/// certification can be [`CertVerdict::Certified`] *while* these gaps
/// exist, precisely because the broker is a working message bus
/// independent of those services' health.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct KnownGap {
    pub id: String,
    pub description: String,
}

/// The overall certification verdict.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertVerdict {
    /// Every artifact assertion `Held` AND every gating precondition
    /// `Held` (and there was at least one artifact assertion to test).
    Certified,
    /// At least one artifact assertion was `Falsified` — real, witnessed
    /// evidence that the declared topology is not fully present.
    NotCertified,
    /// The substrate wasn't fit to answer the question: a gating
    /// precondition was unmet (so artifacts were `Skipped`), or the
    /// artifacts produced only `Errored`/`Skipped`/no results — neither
    /// proven nor disproven.
    Inconclusive,
}

/// Typed inputs to [`CertificationReceipt::build`]. The caller declares
/// which assertion ids are gating *preconditions* (everything else is an
/// *artifact assertion* being certified) and any non-gating *known
/// gaps*.
pub struct CertificationInput {
    pub run_id: String,
    pub scope: String,
    /// Assertion ids that are gating preconditions. An unmet one (any
    /// verdict other than `Held`) routes every artifact assertion to
    /// `Skipped` and forces `Inconclusive`.
    pub precondition_ids: Vec<String>,
    /// Non-gating, reported-for-context environment gaps.
    pub known_gaps: Vec<KnownGap>,
    /// RFC3339 timestamp the receipt was built at.
    pub at: String,
}

/// A typed, content-addressed certification receipt — the projection of
/// one Ensaio run's assertion results into a gate-honest attestation.
///
/// # The derived-verdict law at the run level
///
/// The derived-verdict law applies here exactly as it does to
/// a single [`Verdict`]: this receipt's `verdict` is **derived** from the
/// records it carries ([`classify`], the sole classifier, used by both
/// [`CertificationReceipt::build`] and the deserialization border) and it
/// **carries the witness** — `env_preconditions` and `artifact_assertions`
/// are the examined set, and [`CertificationReceipt::subject_count`]
/// publishes how many artifacts the certification is about.
///
/// Because a receipt is an *exported attestation artifact* — JSON that
/// leaves this process — the `#[serde(try_from)]` border re-derives the
/// verdict from the carried records **and** re-computes `receipt_sha`,
/// rejecting any record whose claim or content-address disagrees with its
/// own witness. A hand-edited "Certified" does not deserialize.
///
/// **Tier-honest:** the struct's fields are `pub`, so *in-memory* a caller
/// can still build a receipt whose `verdict` disagrees with its records —
/// that is **only-mitigated**, and it is why the check is enforced at the
/// wire, where the forgery would actually travel. Sealing the fields is
/// tracked as `pending-derived-verdict: cert-receipt-sealed-fields`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(try_from = "CertificationReceiptWire")]
pub struct CertificationReceipt {
    pub run_id: String,
    pub scope: String,
    pub verdict: CertVerdict,
    /// The gating preconditions and their (unmodified) verdicts.
    pub env_preconditions: Vec<AssertionRecord>,
    /// The assertions the certification is *about* — with any
    /// artifact routed to `Skipped` when a precondition was unmet.
    pub artifact_assertions: Vec<AssertionRecord>,
    /// Non-gating, reported-for-context gaps.
    pub env_known_gaps: Vec<KnownGap>,
    /// BLAKE3 content-address over the ordered `(assertion_id,
    /// verdict_class)` tuples (preconditions then artifacts) plus the
    /// known-gap ids — mirrors `ApplyReceipt.ReceiptSHA`.
    pub receipt_sha: String,
    pub at: String,
}

impl CertificationReceipt {
    /// Build a receipt from one run's assertion results. Pure — no IO —
    /// so the whole projection is unit-testable without a live cluster.
    #[must_use]
    pub fn build(input: CertificationInput, results: &[AssertionResult]) -> Self {
        let is_precondition = |id: &str| input.precondition_ids.iter().any(|p| p == id);

        let mut env_preconditions = Vec::new();
        let mut artifact_raw = Vec::new();
        for r in results {
            let rec = AssertionRecord {
                assertion_id: r.assertion_id.clone(),
                kind: r.kind,
                verdict: r.verdict.clone(),
            };
            if is_precondition(&r.assertion_id) {
                env_preconditions.push(rec);
            } else {
                artifact_raw.push(rec);
            }
        }

        // A precondition is "met" iff it Held. If ANY precondition is not
        // Held, the substrate wasn't fit to test the artifact claims, so
        // every artifact assertion is routed to `Skipped` (neither proven
        // nor disproven) and the verdict is `Inconclusive`.
        let unmet: Vec<&str> = env_preconditions
            .iter()
            .filter(|r| !r.verdict.held())
            .map(|r| r.assertion_id.as_str())
            .collect();
        let preconditions_all_met = unmet.is_empty();

        let artifact_assertions: Vec<AssertionRecord> = if preconditions_all_met {
            artifact_raw
        } else {
            let reason = format!("gated: unmet precondition(s): {}", unmet.join(", "));
            artifact_raw
                .into_iter()
                .map(|mut r| {
                    r.verdict = Verdict::Skipped {
                        reason: reason.clone(),
                    };
                    r
                })
                .collect()
        };

        let verdict = classify(&env_preconditions, &artifact_assertions);

        let receipt_sha =
            content_address(&env_preconditions, &artifact_assertions, &input.known_gaps);

        Self {
            run_id: input.run_id,
            scope: input.scope,
            verdict,
            env_preconditions,
            artifact_assertions,
            env_known_gaps: input.known_gaps,
            receipt_sha,
            at: input.at,
        }
    }

    /// The count of `Held` artifact assertions — a convenience for
    /// reporting.
    #[must_use]
    pub fn held_count(&self) -> usize {
        self.artifact_assertions
            .iter()
            .filter(|r| r.verdict.held())
            .count()
    }

    /// The count of `Falsified` artifact assertions.
    #[must_use]
    pub fn falsified_count(&self) -> usize {
        self.artifact_assertions
            .iter()
            .filter(|r| matches!(r.verdict, Verdict::Falsified { .. }))
            .count()
    }

    /// **The subject-set witness at the run level**: how many artifact
    /// assertions this certification is *about*.
    ///
    /// The subclass-E discipline — *a gate that cannot state what it
    /// examined cannot be audited for what it missed.* A `Certified`
    /// receipt reporting `subject_count() == 0` is not constructible
    /// ([`classify`] routes an empty artifact set to `Inconclusive`), so
    /// this number is always the real scope of the claim.
    #[must_use]
    pub fn subject_count(&self) -> usize {
        self.artifact_assertions.len()
    }
}

/// **The one run-level classifier.** A total function of the examined
/// records — no caller supplies a `CertVerdict`, and both
/// [`CertificationReceipt::build`] and the deserialization border derive
/// through here, so the two can never disagree.
///
/// The ladder:
///
/// 1. any gating precondition not `Held` ⇒ `Inconclusive` — checked
///    **first**, so no artifact result can promote an untestable substrate
///    to a claim;
/// 2. a **non-empty** artifact set, all `Held` ⇒ `Certified`. The
///    non-emptiness is load-bearing: `[].iter().all(_)` is `true`, so
///    without it a run that certified nothing would report `Certified`;
/// 3. any artifact `Falsified` ⇒ `NotCertified`;
/// 4. otherwise ⇒ `Inconclusive` — an artifact `Errored`/`Skipped`/
///    `Vacuous`/`Unreached`, or an empty artifact set. Neither proven nor
///    disproven, which is exactly what a [`Verdict::Vacuous`] assertion
///    means: *we asked, and there was nothing to look at.*
#[must_use]
fn classify(preconditions: &[AssertionRecord], artifacts: &[AssertionRecord]) -> CertVerdict {
    if !preconditions.iter().all(|r| r.verdict.held()) {
        return CertVerdict::Inconclusive;
    }
    if !artifacts.is_empty() && artifacts.iter().all(|r| r.verdict.held()) {
        return CertVerdict::Certified;
    }
    if artifacts
        .iter()
        .any(|r| matches!(r.verdict, Verdict::Falsified { .. }))
    {
        return CertVerdict::NotCertified;
    }
    CertVerdict::Inconclusive
}

// ── The wire border: re-derive the verdict AND the content-address ─────

/// The deserialization shape — structurally identical to
/// [`CertificationReceipt`], but inert: nothing consumes a
/// `CertificationReceiptWire` except the re-derivation below.
#[derive(Deserialize)]
struct CertificationReceiptWire {
    run_id: String,
    scope: String,
    verdict: CertVerdict,
    env_preconditions: Vec<AssertionRecord>,
    artifact_assertions: Vec<AssertionRecord>,
    env_known_gaps: Vec<KnownGap>,
    receipt_sha: String,
    at: String,
}

/// Failure modes of the receipt parse boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReceiptError {
    /// The receipt declared a verdict its own records do not derive.
    #[error(
        "receipt declares `{declared:?}` but its own {artifacts} artifact assertion(s) \
         and {preconditions} precondition(s) derive `{derived:?}`"
    )]
    DeclaredVerdictContradictsRecords {
        declared: CertVerdict,
        derived: CertVerdict,
        preconditions: usize,
        artifacts: usize,
    },
    /// The receipt's content-address does not match its own contents — the
    /// records or the gaps were edited after the digest was taken.
    #[error("receipt_sha {declared} does not match the digest of its own contents ({derived})")]
    ContentAddressMismatch { declared: String, derived: String },
}

impl TryFrom<CertificationReceiptWire> for CertificationReceipt {
    type Error = ReceiptError;

    fn try_from(w: CertificationReceiptWire) -> Result<Self, Self::Error> {
        let derived = classify(&w.env_preconditions, &w.artifact_assertions);
        if derived != w.verdict {
            return Err(ReceiptError::DeclaredVerdictContradictsRecords {
                declared: w.verdict,
                derived,
                preconditions: w.env_preconditions.len(),
                artifacts: w.artifact_assertions.len(),
            });
        }

        let sha = content_address(
            &w.env_preconditions,
            &w.artifact_assertions,
            &w.env_known_gaps,
        );
        if sha != w.receipt_sha {
            return Err(ReceiptError::ContentAddressMismatch {
                declared: w.receipt_sha,
                derived: sha,
            });
        }

        Ok(Self {
            run_id: w.run_id,
            scope: w.scope,
            verdict: w.verdict,
            env_preconditions: w.env_preconditions,
            artifact_assertions: w.artifact_assertions,
            env_known_gaps: w.env_known_gaps,
            receipt_sha: w.receipt_sha,
            at: w.at,
        })
    }
}

/// The verdict's stable class tag — the digest component that mirrors
/// `ApplyReceipt`'s per-step `status` (deliberately NOT the per-run
/// evidence, which is noisy live pod logs, so the sha stays diffable
/// across runs of the same catalog).
fn verdict_tag(v: &Verdict) -> &'static str {
    // Delegates to `Verdict::tag` — one exhaustive `match` with no `_` arm,
    // in one place, so a future seventh variant is a compile error rather
    // than being silently rounded into a neighbouring class here AND
    // colliding with that neighbour's content address.
    v.tag()
}

/// BLAKE3 content-address over the ordered `(assertion_id, verdict_class)`
/// tuples (preconditions then artifacts) plus the known-gap ids —
/// mirrors a bootstrap tool's `ApplyReceipt.ReceiptSHA` ordered digest.
fn content_address(pre: &[AssertionRecord], art: &[AssertionRecord], gaps: &[KnownGap]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"ensaio-certification-receipt-v1");
    for r in pre.iter().chain(art.iter()) {
        h.update(b"\x1e");
        h.update(r.assertion_id.as_bytes());
        h.update(b"\x1f");
        h.update(verdict_tag(&r.verdict).as_bytes());
    }
    for g in gaps {
        h.update(b"\x1e");
        h.update(b"gap:");
        h.update(g.id.as_bytes());
    }
    h.finalize().to_hex().to_string()
}
