//! [`Ttl`] — a **parse-rejecting** positive, non-zero lease/TTL duration.
//!
//! # Why this is not `ishou_tokens::Refined`
//!
//! The design named this `Refined<Duration, PositiveNonZero>` and cited the
//! fleet Refined-unification epic ("never a 4th copy"). Grounding that against
//! the canonical source (`ishou/crates/ishou-tokens/src/refined.rs`) refutes
//! the naming: `Refined`'s `Deserialize` **clamps** an out-of-range value to a
//! bound — "Operator YAML `font_size: 9999.0` deserializes to `B::max()`, not a
//! runtime panic." That is exactly the wrong behavior for a lease ttl: a
//! `ttl: 0` must be **rejected at the parse boundary** (parse-don't-validate,
//! `Err`), never silently clamped to some minimum and reaped-at-min. Clamp and
//! reject are different algebras; `Ttl` is a purpose-built parse-rejecting
//! newtype, not a fourth copy of the clamp primitive. (Recorded as the seventh
//! design correction in `UTSUROI.md`.)
//!
//! # Honest tier
//!
//! `Ttl` earns **parse-time-rejected** for the *value*: a zero / absent /
//! unparseable ttl has no representation past deserialize. The reap actually
//! *firing* on expiry remains a runtime world-fact (a reconciler tick) —
//! **only-mitigated**, and unbuilt at M0. Never round the value guarantee up
//! into a reap guarantee.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A ttl that is provably a positive, non-zero duration. Constructed only
/// through a checking path ([`Ttl::from_std`] / [`Ttl::parse`] / serde
/// `try_from`), so a zero/absent/unparseable value cannot flow downstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Ttl(Duration);

/// Why a candidate ttl was rejected at the parse boundary.
#[derive(Debug, thiserror::Error)]
pub enum TtlError {
    /// The ttl string was empty/whitespace.
    #[error("ttl is empty/absent")]
    Absent,
    /// The ttl parsed to a zero duration.
    #[error("ttl must be a positive, non-zero duration (got zero)")]
    NonPositive,
    /// The ttl string is not a humantime duration (e.g. `"soon"`).
    #[error("ttl {raw:?} is not a parseable humantime duration: {source}")]
    Unparseable {
        raw: String,
        source: humantime::DurationError,
    },
}

impl Ttl {
    /// Construct from a [`Duration`], rejecting a zero duration.
    ///
    /// # Errors
    /// [`TtlError::NonPositive`] when `d` is zero.
    pub fn from_std(d: Duration) -> Result<Self, TtlError> {
        if d.is_zero() {
            return Err(TtlError::NonPositive);
        }
        Ok(Self(d))
    }

    /// Parse a humantime string (`"30m"`, `"1h30m"`), rejecting empty,
    /// zero, and unparseable input.
    ///
    /// # Errors
    /// [`TtlError::Absent`] / [`TtlError::NonPositive`] / [`TtlError::Unparseable`].
    pub fn parse(raw: &str) -> Result<Self, TtlError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(TtlError::Absent);
        }
        let d = humantime::parse_duration(trimmed).map_err(|source| TtlError::Unparseable {
            raw: raw.to_string(),
            source,
        })?;
        Self::from_std(d)
    }

    /// The wrapped duration — always positive and non-zero.
    #[must_use]
    pub fn get(self) -> Duration {
        self.0
    }
}

impl TryFrom<String> for Ttl {
    type Error = TtlError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<Ttl> for String {
    fn from(t: Ttl) -> Self {
        humantime::format_duration(t.0).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_std_rejects_zero() {
        assert!(matches!(
            Ttl::from_std(Duration::ZERO),
            Err(TtlError::NonPositive)
        ));
    }

    #[test]
    fn from_std_accepts_positive() {
        assert_eq!(
            Ttl::from_std(Duration::from_secs(1800)).unwrap().get(),
            Duration::from_secs(1800)
        );
    }

    #[test]
    fn parse_rejects_empty_and_zero_and_garbage() {
        assert!(matches!(Ttl::parse(""), Err(TtlError::Absent)));
        assert!(matches!(Ttl::parse("   "), Err(TtlError::Absent)));
        assert!(matches!(Ttl::parse("0s"), Err(TtlError::NonPositive)));
        assert!(matches!(
            Ttl::parse("soon"),
            Err(TtlError::Unparseable { .. })
        ));
    }

    #[test]
    fn parse_accepts_humantime() {
        assert_eq!(Ttl::parse("30m").unwrap().get(), Duration::from_secs(1800));
        assert_eq!(
            Ttl::parse("1h30m").unwrap().get(),
            Duration::from_secs(5400)
        );
    }

    #[test]
    fn serde_deserialize_rejects_non_expiring_value() {
        // The parse-time-rejected tier, proved at the serde boundary:
        // a zero/absent/garbage ttl has no representation past deserialize.
        assert!(serde_json::from_str::<Ttl>("\"0s\"").is_err());
        assert!(serde_json::from_str::<Ttl>("\"\"").is_err());
        assert!(serde_json::from_str::<Ttl>("\"whenever\"").is_err());
    }

    #[test]
    fn serde_round_trips_a_valid_ttl() {
        let t = Ttl::parse("45m").unwrap();
        let json = serde_json::to_string(&t).unwrap();
        let back: Ttl = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get(), t.get());
    }
}
