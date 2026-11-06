//! The bounded-demand + reap border: [`Lease`], [`Boundedness`],
//! [`ReapMechanism`], [`LifecycleRef`].
//!
//! The design's biggest verify correction lives here. Two facts the source
//! forced:
//!
//! - **`Lease` is a *passive* bounded-demand value, not a phased CR.** viveiro's
//!   `EnvironmentClaim` is a *Lifecycle that carries a `Lease`* (it has its own
//!   6-phase FSM); it is not itself a `Lease`. So `Lease` here has no phases —
//!   just `granted_at` + `ttl` + `target`.
//! - **`Boundedness` needs four arms, aligned with [`ReapMechanism`].** The
//!   design's two-arm `Lease | SelfTtl` could not express the tenant's actual
//!   shipped boundedness (Kubernetes owner-reference GC, not a lease or a
//!   self-ttl) nor viveiro's warm-pool member (bound by pool membership, which
//!   a 6h self-ttl would wrongly reap). `OwnerRefBound` pairs with
//!   [`ReapMechanism::OwnerRefGc`]; `PoolBound` covers the pool member.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ttl::Ttl;

/// A referential link to another lifecycle resource (kind + name) — e.g. the
/// `WarmEnvironment` a `Lease` holds, or the parent an `OwnerRefBound`
/// resource is garbage-collected with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRef {
    pub kind: String,
    pub name: String,
}

/// How an ephemeral resource's footprint is actually reaped. Kept a small
/// closed sum so a domain's `:reap` declaration is exhaustive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReapMechanism {
    /// Scale every provisioned workload to zero (ensaio).
    ScaleToZero,
    /// Delete the isolated namespace (viveiro / kenshi).
    DeleteNamespace,
    /// Kubernetes garbage-collects the resource when a parent is deleted
    /// (the tenant) — pairs with [`Boundedness::OwnerRefBound`].
    OwnerRefGc,
    /// An external come-up/tear-down binary driven over a Job and a typed
    /// receipt — never linked, only driven.
    ExternalDriver,
}

impl ReapMechanism {
    /// Every mechanism, once — CATALOG-REFLECTION surface.
    pub const ALL: [ReapMechanism; 4] = [
        ReapMechanism::ScaleToZero,
        ReapMechanism::DeleteNamespace,
        ReapMechanism::OwnerRefGc,
        ReapMechanism::ExternalDriver,
    ];

    /// Stable lower-kebab wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScaleToZero => "scale-to-zero",
            Self::DeleteNamespace => "delete-namespace",
            Self::OwnerRefGc => "owner-ref-gc",
            Self::ExternalDriver => "external-driver",
        }
    }
}

/// How an ephemeral resource's lifetime is bounded. A resource **must** carry
/// exactly one — there is no "unbounded" arm, so a footprint that could leak
/// forever has no representation here (the seal is at the type when a resource
/// declares its `Boundedness`; a resource CR that omits it entirely is still
/// only-mitigated until the CRD makes the field required — an M1 wire concern).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Boundedness {
    /// Bounded by an external [`Lease`] held on it (a claim). The resource
    /// lives until its claim releases/expires — viveiro's `WarmEnvironment`.
    Lease,
    /// Carries its own expiry — ensaio / kenshi (their CR *is* the claim, so
    /// the ttl lives on the resource itself).
    SelfTtl(Ttl),
    /// Reaped by Kubernetes garbage collection when `parent` is deleted — the
    /// an ephemeral-tenant CR's actual shipped boundedness. Pairs with
    /// [`ReapMechanism::OwnerRefGc`].
    OwnerRefBound { parent: LifecycleRef },
    /// Bound by pool membership — a warm-pool member is intentionally
    /// long-lived-until-claimed and must NOT carry a self-ttl (a timer would
    /// reap the warm capacity the pool exists to keep hot). viveiro M2.
    PoolBound,
}

/// A bounded demand on a lifecycle — viveiro's `EnvironmentClaim`, generalized
/// to a **passive value**. It carries no phases of its own (the claim's FSM
/// lives on the claim resource, which *carries* a `Lease`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// When the lease was granted.
    pub granted_at: DateTime<Utc>,
    /// How long the lease lasts — a [`Ttl`], so it cannot be non-expiring.
    pub ttl: Ttl,
    /// The lifecycle this lease holds.
    pub target: LifecycleRef,
}

impl Lease {
    /// Whether the lease has expired as of `now`. A runtime compare — the type
    /// guarantees the ttl is a positive duration, but the reap *firing* on
    /// expiry is a reconciler's job (only-mitigated), not this method's.
    #[must_use]
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        // `chrono::Duration::from_std` only fails on an absurd (>~292e9-year)
        // ttl; treat that as not-yet-expired rather than panicking.
        match chrono::Duration::from_std(self.ttl.get()) {
            Ok(d) => now > self.granted_at + d,
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ref_to(name: &str) -> LifecycleRef {
        LifecycleRef {
            kind: "WarmEnvironment".to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn all_four_boundedness_arms_construct() {
        let _lease = Boundedness::Lease;
        let _self_ttl = Boundedness::SelfTtl(Ttl::parse("30m").unwrap());
        let _owner = Boundedness::OwnerRefBound {
            parent: ref_to("parent-validation"),
        };
        let _pool = Boundedness::PoolBound;
    }

    #[test]
    fn owner_ref_bound_aligns_with_owner_ref_gc() {
        // The alignment the design's two-enum disagreement demanded: an
        // owner-ref-bound resource is reaped by owner-ref GC.
        let b = Boundedness::OwnerRefBound {
            parent: ref_to("p"),
        };
        let mech = match b {
            Boundedness::OwnerRefBound { .. } => ReapMechanism::OwnerRefGc,
            _ => unreachable!(),
        };
        assert_eq!(mech.as_str(), "owner-ref-gc");
    }

    #[test]
    fn lease_expiry_compares_against_now() {
        let granted = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let lease = Lease {
            granted_at: granted,
            ttl: Ttl::parse("1h").unwrap(),
            target: ref_to("c1-env"),
        };
        // 30m in — not expired.
        assert!(!lease.is_expired(granted + chrono::Duration::minutes(30)));
        // 90m in — expired.
        assert!(lease.is_expired(granted + chrono::Duration::minutes(90)));
    }

    #[test]
    fn lease_deserialize_rejects_a_non_expiring_ttl() {
        // The Lease parse-rejection the corrected scope requires: a Lease whose
        // ttl is "0s" has no representation past deserialize.
        let bad = r#"{"granted_at":"2026-01-01T00:00:00Z","ttl":"0s","target":{"kind":"WarmEnvironment","name":"c1-env"}}"#;
        assert!(serde_json::from_str::<Lease>(bad).is_err());

        let good = r#"{"granted_at":"2026-01-01T00:00:00Z","ttl":"1h","target":{"kind":"WarmEnvironment","name":"c1-env"}}"#;
        assert!(serde_json::from_str::<Lease>(good).is_ok());
    }

    #[test]
    fn boundedness_self_ttl_round_trips_through_serde() {
        let b = Boundedness::SelfTtl(Ttl::parse("30m").unwrap());
        let json = serde_json::to_string(&b).unwrap();
        let back: Boundedness = serde_json::from_str(&json).unwrap();
        assert_eq!(back, b);
    }
}
