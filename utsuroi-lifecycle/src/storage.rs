//! The storage-compatibility **relation** — `Durability × Locus` — and the
//! medium it DERIVES.
//!
//! # Why the relation lives in the spine and the ramdisk does not
//!
//! RAMDISK is a property of a **permutation** (a *variedade*), never of the
//! framework. `theory/RAMDISK.md` prescribes memory-backed storage for every
//! ephemeral environment, and the framework's job is not to hold that choice —
//! it is to hold the *rule that decides whether a given permutation's choice is
//! legal for a given component*. So there is no storage field on any lifecycle
//! resource here: a variedade declares a [`StoragePosture`], a component
//! declares a [`StorageNeed`], and [`compatible`] answers.
//!
//! # Why compatibility is not a boolean
//!
//! "Does this component tolerate a ramdisk?" has at least four different
//! answers that a `bool` renders identically, and two of them are *yes for
//! opposite reasons*:
//!
//! | scenario | verdict | because |
//! |---|---|---|
//! | branch/PR test | admitted | nothing outlives the PR — [`Because::DurabilitySuffices`] |
//! | PITR drill | admitted | the authoritative bytes are a row inside RDS — [`Because::StateIsExternal`] |
//! | DR-restore rehearsal | refused | the restore target IS the in-cluster database — [`Refusal::DurabilityShortfall`] |
//! | perf/soak | refused | **fidelity, not durability** — [`Refusal::FidelityShortfall`] |
//!
//! Collapsing the last two is the expensive one: a soak refused for "durability"
//! sends an operator to add a PVC, which does not fix the measurement, because
//! nothing about the soak needed the bytes to survive — it needed them to behave
//! like production. The verdict therefore names WHICH clause failed and renders
//! through [`fmt::Display`] (★★ TYPED EMISSION — a `Display`-family `write!`,
//! never a `format!` of `Debug`).
//!
//! # The `External` escape is load-bearing
//!
//! `provided.durability >= required.durability || required.locus == External`.
//! The second clause is what makes a PITR permutation legal on ANY pod medium:
//! its authoritative state is a row inside RDS and its drill Jobs declare zero
//! volumes, so the pod-local medium is simply not where the durability question
//! is asked. Without that arm, "restores a database" reads as "needs durable
//! pod storage" and the ramdisk default gets waived for a workload that stores
//! nothing.
//!
//! # Honest tiers, stated per invariant
//!
//! - A memory-backed posture claiming above [`Durability::PodLifetime`] is
//!   **parse-time-rejected** — [`StoragePosture::new`] refuses it and the serde
//!   boundary routes through that constructor, so the lie has no representation
//!   past deserialize. It is *not* truly-unrepresentable: the fields are private
//!   precisely because a public pair of enums could still be paired wrongly.
//! - A need that must outlive the pod while located inside it is likewise
//!   **parse-time-rejected** ([`StorageNeed::new`]).
//! - Whether the cluster's CSI driver actually honours a snapshot class is a
//!   **world-fact** no type here reaches — `INVARIANT-BY-CONSISTENCY-AND-CONTROLLER`.
//!   `pending-storage-relation: no controller re-derives the posture from the
//!   live StorageClass set`
//!
//! # Relation to `viveiro::env::EphemeralStorage`
//!
//! That type is the *emitter* — it renders a pod-spec volume and already carries
//! the truly-unrepresentable rung this module must not duplicate:
//! `EphemeralStorage::Disk` has no constructor without an `Outlives`
//! justification. This module is the *judge* one level up, in the spine, where
//! neither viveiro nor a PITR component owns it. A caller decides with
//! [`compatible`], then emits with `EphemeralStorage` — the medium travels as a
//! derivation ([`derived_medium`]), never as a declared field.

use core::fmt;

use serde::{Deserialize, Serialize};

/// How long the bytes must survive. **ORDERED** — the declaration order IS the
/// relation, so `provided >= required` is the literal comparison [`compatible`]
/// performs.
///
/// # Why a tmpfs denies the top two STRUCTURALLY, not by policy
///
/// It is tempting to read "a ramdisk is not durable" as a soft statement about
/// power loss. It is harder than that: an `emptyDir` — memory-backed or not —
/// is **not a CSI volume**, so it has no `PersistentVolumeClaim`, and therefore
/// no `VolumeSnapshot` can name it. There is nothing for a snapshot controller
/// or a PITR engine to *attach to*. The denial is not "the data might be lost";
/// it is "the API that would preserve or rewind the data has no object here".
/// A component asking for [`Self::ClusterLifetime`] or [`Self::Recoverable`]
/// from a tmpfs is not accepting a risk — it is naming a mechanism that does
/// not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Durability {
    /// Born with the pod, dies with it. The lifetime an ephemeral environment
    /// already claims to have — the fleet-prescribed default.
    PodLifetime,
    /// Survives pod restarts and rescheduling; dies with the namespace, so the
    /// environment's own reap still collects it. A namespace-scoped PVC.
    ClusterLifetime,
    /// Survives *and* can be rewound — a snapshot or point-in-time restore can
    /// reconstruct an earlier state. Requires a medium a snapshot API can name.
    Recoverable,
}

impl Durability {
    /// Every rung, ascending — the CATALOG-REFLECTION surface, co-located so a
    /// drift test can pin both the count and the order.
    pub const ALL: [Durability; 3] = [
        Durability::PodLifetime,
        Durability::ClusterLifetime,
        Durability::Recoverable,
    ];

    /// Stable lower-kebab wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PodLifetime => "pod-lifetime",
            Self::ClusterLifetime => "cluster-lifetime",
            Self::Recoverable => "recoverable",
        }
    }

    /// Whether this rung needs a medium a snapshot/PVC API can name — i.e.
    /// anything above [`Self::PodLifetime`]. The predicate a memory-backed
    /// posture is refused by.
    #[must_use]
    pub fn needs_a_named_volume(self) -> bool {
        match self {
            Self::PodLifetime => false,
            Self::ClusterLifetime | Self::Recoverable => true,
        }
    }
}

impl fmt::Display for Durability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// WHERE the authoritative bytes live. Deliberately not ordered: `External` is
/// not "more" than `InCluster`, it is *elsewhere*, and the relation treats it as
/// an escape rather than a rung.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Locus {
    /// Inside the pod's own filesystem.
    InPod,
    /// Inside the cluster but outside the pod — a PVC, another workload's
    /// volume, the in-cluster database being restored into.
    InCluster,
    /// Outside the cluster entirely — an RDS row, an S3 object, a managed
    /// service's own store. The pod holds a client, not the data.
    External,
}

impl Locus {
    /// Every locus, once — CATALOG REFLECTION.
    pub const ALL: [Locus; 3] = [Locus::InPod, Locus::InCluster, Locus::External];

    /// Stable lower-kebab wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InPod => "in-pod",
            Self::InCluster => "in-cluster",
            Self::External => "external",
        }
    }
}

impl fmt::Display for Locus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether the medium's I/O *behaviour* is load-bearing, independently of
/// whether its bytes survive.
///
/// This axis exists because durability cannot express the soak case. A soak run
/// stores nothing worth keeping — [`Durability::PodLifetime`] is genuinely all
/// it needs — and a tmpfs is still the wrong medium, because the numbers it
/// produces are a measurement of RAM. Refusing it under the durability clause
/// would send an operator to fix the wrong thing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fidelity {
    /// The component does not measure I/O; any medium that holds the bytes will
    /// do. The default — an ABSENT fidelity is not a request, so it must never
    /// wedge a path that worked before this axis existed.
    #[default]
    Indifferent,
    /// The component's output is only meaningful if the medium behaves like the
    /// production one — latency, throughput, and real `fsync` semantics.
    MatchesProduction,
}

impl Fidelity {
    /// Every arm, once — CATALOG REFLECTION.
    pub const ALL: [Fidelity; 2] = [Fidelity::Indifferent, Fidelity::MatchesProduction];

    /// Stable lower-kebab wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indifferent => "indifferent",
            Self::MatchesProduction => "matches-production",
        }
    }
}

impl fmt::Display for Fidelity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a medium's I/O actually behaves like. Genuinely independent of
/// durability and locus: a *disk-backed* `emptyDir` is
/// [`Durability::PodLifetime`] + [`Locus::InPod`] and still
/// [`Self::ProductionBlock`], so this cannot be derived from the other two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IoProfile {
    /// Memory speed, and `fsync` is a no-op because there is nothing to sync
    /// to. A tmpfs.
    MemorySpeed,
    /// A real block device with the production medium's latency and durability
    /// syscall semantics.
    ProductionBlock,
}

impl IoProfile {
    /// Every profile, once — CATALOG REFLECTION.
    pub const ALL: [IoProfile; 2] = [IoProfile::MemorySpeed, IoProfile::ProductionBlock];

    /// Stable lower-kebab wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MemorySpeed => "memory-speed",
            Self::ProductionBlock => "production-block",
        }
    }

    /// Whether the medium's capacity is billed against the pod's **memory**
    /// limit rather than a disk quota. True for a tmpfs, and the reason a
    /// posture's size bound is never optional.
    #[must_use]
    pub fn counts_against_pod_memory(self) -> bool {
        match self {
            Self::MemorySpeed => true,
            Self::ProductionBlock => false,
        }
    }

    /// What happens when the bound is reached. **DERIVED, never declared** — an
    /// author cannot claim a graceful `ENOSPC` on a ramdisk, because a full
    /// tmpfs consumes the pod's memory allowance and the kernel kills the pod.
    #[must_use]
    pub fn overflow(self) -> Overflow {
        match self {
            Self::MemorySpeed => Overflow::OomKill,
            Self::ProductionBlock => Overflow::NoSpaceLeft,
        }
    }
}

impl fmt::Display for IoProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a medium fails when it fills up. Derived from [`IoProfile`]; carried as
/// its own type because the two failures are diagnosed at opposite ends — one
/// is a pod-level kill with no cause named, the other is an application error
/// naming the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Overflow {
    /// The kernel kills the pod. `theory/RAMDISK.md` §VI: it presents as an
    /// unattributable OOM, not a full disk, which is why the bound matters.
    OomKill,
    /// A write fails with `ENOSPC` and the application sees it.
    NoSpaceLeft,
}

impl Overflow {
    /// Every arm, once — CATALOG REFLECTION.
    pub const ALL: [Overflow; 2] = [Overflow::OomKill, Overflow::NoSpaceLeft];

    /// Stable lower-kebab wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OomKill => "oom-kill",
            Self::NoSpaceLeft => "no-space-left",
        }
    }
}

impl fmt::Display for Overflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The medium a [`StorageNeed`] implies. **Always derived** ([`derived_medium`]),
/// never a declared field — that is the whole point of expressing storage as a
/// relation: an author states what the component needs, and what to mount is a
/// consequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Medium {
    /// A memory-backed `emptyDir` — the fleet-prescribed default.
    Tmpfs,
    /// A disk-backed `emptyDir`: pod-lifetime bytes with real block I/O. The
    /// medium a fidelity requirement buys without buying durability.
    EphemeralBlock,
    /// A namespace-scoped `PersistentVolumeClaim`.
    ClusterVolume,
    /// A PVC on a snapshot-capable CSI class — the only in-cluster medium a
    /// snapshot or PITR engine can attach to.
    SnapshottableVolume,
    /// No pod-local medium is implied: the authoritative bytes live outside the
    /// cluster, so the pod's own storage is unconstrained and the variedade's
    /// prescribed ramdisk stands.
    ExternalStore,
}

impl Medium {
    /// Every medium, once — CATALOG REFLECTION.
    pub const ALL: [Medium; 5] = [
        Medium::Tmpfs,
        Medium::EphemeralBlock,
        Medium::ClusterVolume,
        Medium::SnapshottableVolume,
        Medium::ExternalStore,
    ];

    /// Stable lower-kebab wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tmpfs => "tmpfs",
            Self::EphemeralBlock => "ephemeral-block",
            Self::ClusterVolume => "cluster-volume",
            Self::SnapshottableVolume => "snapshottable-volume",
            Self::ExternalStore => "external-store",
        }
    }
}

impl fmt::Display for Medium {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a [`StorageNeed`] or [`StoragePosture`] was refused at construction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// A need whose bytes must outlive the pod cannot be located inside it.
    /// Not a stylistic objection: [`Locus::InPod`] names the pod's own
    /// filesystem, which ceases to exist at the moment the durability rung
    /// promises the bytes are still there.
    #[error(
        "a {durability} need cannot have locus {locus}: bytes that must outlive \
         the pod cannot live inside it — widen the locus to in-cluster or external"
    )]
    LocusTooNarrow {
        durability: Durability,
        locus: Locus,
    },
    /// A memory-backed posture claiming durability it structurally cannot
    /// provide — see [`Durability`]'s header for why the denial is structural.
    #[error(
        "a memory-speed posture cannot provide {durability}: an emptyDir is not \
         a CSI volume, so no PersistentVolumeClaim and no VolumeSnapshot can \
         name it — there is nothing for a snapshot or PITR engine to attach to"
    )]
    MemoryCannotPersist { durability: Durability },
    /// A posture with no capacity bound. A tmpfs bills against the pod's memory
    /// limit, so an unbounded one turns a runaway write into an `OOMKill` that
    /// names no cause (`theory/RAMDISK.md` §VI).
    #[error("a storage posture must declare a non-zero capacity bound")]
    UnboundedCapacity,
}

/// What a COMPONENT requires of its storage. Constructed only through a
/// checking path, so the outlive-the-pod-from-inside-it contradiction has no
/// representation past construction or deserialize (**parse-time-rejected**).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "StorageNeedWire", try_from = "StorageNeedWire")]
pub struct StorageNeed {
    durability: Durability,
    locus: Locus,
    fidelity: Fidelity,
    min_bytes: Option<u64>,
}

/// The wire shape of a [`StorageNeed`]. Exists so `Deserialize` routes through
/// [`StorageNeed::new`] rather than around it — the Ttl pattern.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct StorageNeedWire {
    durability: Durability,
    locus: Locus,
    /// Absent is not a request: an omitted fidelity means the component does
    /// not measure I/O, which is exactly [`Fidelity::Indifferent`].
    #[serde(default)]
    fidelity: Fidelity,
    #[serde(default)]
    min_bytes: Option<u64>,
}

impl StorageNeed {
    /// Declare a need, refusing the durability/locus contradiction.
    ///
    /// # Errors
    /// [`StorageError::LocusTooNarrow`] when the bytes must outlive the pod
    /// while being located inside it.
    pub fn new(
        durability: Durability,
        locus: Locus,
        fidelity: Fidelity,
    ) -> Result<Self, StorageError> {
        if durability.needs_a_named_volume() && locus == Locus::InPod {
            return Err(StorageError::LocusTooNarrow { durability, locus });
        }
        Ok(Self {
            durability,
            locus,
            fidelity,
            min_bytes: None,
        })
    }

    /// The same need with a minimum capacity — the working set the component
    /// cannot run below.
    #[must_use]
    pub fn with_min_bytes(mut self, min_bytes: u64) -> Self {
        self.min_bytes = Some(min_bytes);
        self
    }

    /// How long the bytes must survive.
    #[must_use]
    pub fn durability(&self) -> Durability {
        self.durability
    }

    /// Where the authoritative bytes live.
    #[must_use]
    pub fn locus(&self) -> Locus {
        self.locus
    }

    /// Whether the medium's I/O behaviour is load-bearing.
    #[must_use]
    pub fn fidelity(&self) -> Fidelity {
        self.fidelity
    }

    /// The declared minimum capacity, when the component states one.
    #[must_use]
    pub fn min_bytes(&self) -> Option<u64> {
        self.min_bytes
    }

    /// The medium this need implies — see [`derived_medium`].
    #[must_use]
    pub fn medium(&self) -> Medium {
        derived_medium(self)
    }
}

impl TryFrom<StorageNeedWire> for StorageNeed {
    type Error = StorageError;
    fn try_from(w: StorageNeedWire) -> Result<Self, Self::Error> {
        let need = Self::new(w.durability, w.locus, w.fidelity)?;
        Ok(match w.min_bytes {
            Some(b) => need.with_min_bytes(b),
            None => need,
        })
    }
}

impl From<StorageNeed> for StorageNeedWire {
    fn from(n: StorageNeed) -> Self {
        Self {
            durability: n.durability,
            locus: n.locus,
            fidelity: n.fidelity,
            min_bytes: n.min_bytes,
        }
    }
}

/// What a VARIEDADE provides. Carries enough to derive the medium — never the
/// medium itself.
///
/// The three axes beyond `durability`/`locus` are the ones that genuinely stay
/// independent: [`IoProfile`] (a disk-backed `emptyDir` is pod-lifetime AND
/// block), the capacity bound (a tmpfs bills against the pod memory limit, so
/// the bound is mandatory), and — derived from the profile, not declared — the
/// overflow semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "StoragePostureWire", try_from = "StoragePostureWire")]
pub struct StoragePosture {
    durability: Durability,
    locus: Locus,
    io_profile: IoProfile,
    capacity_bytes: u64,
}

/// The wire shape of a [`StoragePosture`] — routes `Deserialize` through
/// [`StoragePosture::new`].
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct StoragePostureWire {
    durability: Durability,
    locus: Locus,
    io_profile: IoProfile,
    capacity_bytes: u64,
}

impl StoragePosture {
    /// Declare what a permutation provides, refusing a memory-backed medium
    /// that claims persistence and an unbounded one.
    ///
    /// # Errors
    /// [`StorageError::MemoryCannotPersist`] / [`StorageError::UnboundedCapacity`].
    pub fn new(
        durability: Durability,
        locus: Locus,
        io_profile: IoProfile,
        capacity_bytes: u64,
    ) -> Result<Self, StorageError> {
        if io_profile == IoProfile::MemorySpeed && durability.needs_a_named_volume() {
            return Err(StorageError::MemoryCannotPersist { durability });
        }
        if capacity_bytes == 0 {
            return Err(StorageError::UnboundedCapacity);
        }
        Ok(Self {
            durability,
            locus,
            io_profile,
            capacity_bytes,
        })
    }

    /// The fleet-prescribed posture: memory-backed, pod-lifetime, bounded — the
    /// sibling of `viveiro::env::EphemeralStorage::prescribed`, stated as a
    /// posture rather than as a volume.
    ///
    /// # Errors
    /// [`StorageError::UnboundedCapacity`] when `capacity_bytes` is zero.
    pub fn ramdisk(capacity_bytes: u64) -> Result<Self, StorageError> {
        Self::new(
            Durability::PodLifetime,
            Locus::InPod,
            IoProfile::MemorySpeed,
            capacity_bytes,
        )
    }

    /// How long these bytes survive.
    #[must_use]
    pub fn durability(&self) -> Durability {
        self.durability
    }

    /// Where these bytes live.
    #[must_use]
    pub fn locus(&self) -> Locus {
        self.locus
    }

    /// What the medium's I/O behaves like.
    #[must_use]
    pub fn io_profile(&self) -> IoProfile {
        self.io_profile
    }

    /// The capacity bound — against the pod's memory limit when
    /// [`IoProfile::counts_against_pod_memory`].
    #[must_use]
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    /// What happens when the bound is reached — **derived** from the profile.
    #[must_use]
    pub fn overflow(&self) -> Overflow {
        self.io_profile.overflow()
    }
}

impl TryFrom<StoragePostureWire> for StoragePosture {
    type Error = StorageError;
    fn try_from(w: StoragePostureWire) -> Result<Self, Self::Error> {
        Self::new(w.durability, w.locus, w.io_profile, w.capacity_bytes)
    }
}

impl From<StoragePosture> for StoragePostureWire {
    fn from(p: StoragePosture) -> Self {
        Self {
            durability: p.durability,
            locus: p.locus,
            io_profile: p.io_profile,
            capacity_bytes: p.capacity_bytes,
        }
    }
}

/// Why a posture was ADMITTED. Two arms, because the two admissions happen for
/// opposite reasons and an operator reading "compatible" learns nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Because {
    /// The medium outlasts what the component asked for — the D2 relation
    /// `provided >= required` held on its own.
    DurabilitySuffices {
        provided: Durability,
        required: Durability,
    },
    /// The durability clause was never asked: the authoritative bytes are
    /// outside the cluster, so the pod's medium is not where they live. The
    /// escape that makes a PITR permutation legal on a ramdisk.
    StateIsExternal,
}

impl Because {
    /// Stable lower-kebab wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DurabilitySuffices { .. } => "durability-suffices",
            Self::StateIsExternal => "state-is-external",
        }
    }
}

impl fmt::Display for Because {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DurabilitySuffices { provided, required } => write!(
                f,
                "the medium provides {provided} and the component requires {required}"
            ),
            Self::StateIsExternal => f.write_str(
                "the authoritative state is external to the cluster, so the pod's \
                 medium holds nothing that must survive",
            ),
        }
    }
}

/// Why a posture was REFUSED, naming the clause. The arms must not render
/// identically: a durability refusal sends an operator to add a volume, a
/// fidelity refusal sends them to change the medium's *kind*, and adding a
/// volume does not fix a measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The medium's bytes do not survive long enough, and the state is not
    /// external.
    DurabilityShortfall {
        provided: Durability,
        required: Durability,
        /// What the need implies instead — the actionable half.
        wants: Medium,
    },
    /// The bytes survive long enough; the medium *behaves* wrongly. Nothing
    /// about this is fixed by making the storage more durable.
    FidelityShortfall {
        provided: IoProfile,
        required: Fidelity,
        wants: Medium,
    },
    /// The medium is too small for the component's working set.
    CapacityShortfall {
        provided_bytes: u64,
        required_bytes: u64,
        /// Whether the bound bills against the pod's memory limit, which
        /// decides whether raising it means raising the memory limit too.
        against_pod_memory: bool,
    },
}

impl Refusal {
    /// Stable lower-kebab wire token — one per clause.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DurabilityShortfall { .. } => "durability-shortfall",
            Self::FidelityShortfall { .. } => "fidelity-shortfall",
            Self::CapacityShortfall { .. } => "capacity-shortfall",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DurabilityShortfall {
                provided,
                required,
                wants,
            } => write!(
                f,
                "durability: the medium provides {provided} but the component \
                 requires {required} and its state is not external — it needs a {wants}"
            ),
            Self::FidelityShortfall {
                provided,
                required,
                wants,
            } => write!(
                f,
                "fidelity: the medium is {provided} but the component requires \
                 {required} — its measurements would describe the medium, not the \
                 workload; it needs a {wants}. Durability is not the problem and a \
                 larger or longer-lived volume does not fix it"
            ),
            Self::CapacityShortfall {
                provided_bytes,
                required_bytes,
                against_pod_memory,
            } => {
                write!(
                    f,
                    "capacity: the medium is bounded at {provided_bytes} bytes but \
                     the component requires {required_bytes}"
                )?;
                if *against_pod_memory {
                    f.write_str(
                        " — and this bound bills against the pod's memory limit, so \
                         raising it means raising that limit too, or the overflow is \
                         an OOMKill that names no cause",
                    )?;
                }
                Ok(())
            }
        }
    }
}

/// The verdict. A typed sum, never a `bool` — see this module's header for the
/// four answers a boolean renders identically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compatibility {
    /// Admitted, with the clause that admitted it.
    Holds(Because),
    /// Refused, with the clause that refused it.
    Refused(Refusal),
}

impl Compatibility {
    /// Whether the posture may back this component.
    #[must_use]
    pub fn holds(self) -> bool {
        matches!(self, Self::Holds(_))
    }

    /// Stable lower-kebab token naming the deciding clause — `"state-is-external"`,
    /// `"fidelity-shortfall"`, … Never the arm alone, because "refused" is the
    /// half an operator already knows.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Holds(b) => b.as_str(),
            Self::Refused(r) => r.as_str(),
        }
    }
}

impl fmt::Display for Compatibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Holds(b) => write!(f, "compatible: {b}"),
            Self::Refused(r) => write!(f, "incompatible: {r}"),
        }
    }
}

/// The medium a need implies. **The derivation** — the medium is never a
/// declared field, on the framework or on a permutation.
///
/// Reading order is locus-first, because [`Locus::External`] settles the
/// question before durability is asked: if the authoritative bytes are outside
/// the cluster there is no pod-local medium to choose, and that is precisely
/// what leaves a PITR drill free to run on the prescribed ramdisk.
#[must_use]
pub fn derived_medium(need: &StorageNeed) -> Medium {
    match need.locus {
        Locus::External => Medium::ExternalStore,
        Locus::InPod | Locus::InCluster => match need.durability {
            // Fidelity is the discriminator only here: above pod-lifetime the
            // medium is already a real block device, so asking for production
            // behaviour changes nothing.
            Durability::PodLifetime => match need.fidelity {
                Fidelity::Indifferent => Medium::Tmpfs,
                Fidelity::MatchesProduction => Medium::EphemeralBlock,
            },
            Durability::ClusterLifetime => Medium::ClusterVolume,
            Durability::Recoverable => Medium::SnapshottableVolume,
        },
    }
}

/// Judge a permutation's posture against a component's need.
///
/// Clause order, stated because only the FIRST failing clause is named (an
/// operator fixes one thing and re-asks):
///
/// 1. **durability**, with the [`Locus::External`] escape — the D2 relation
///    itself, so its escape is evaluated where D2 states it;
/// 2. **fidelity** — checked even when the escape admitted the durability
///    clause, because an external authoritative store says nothing about how
///    the pod's own scratch behaves under a soak;
/// 3. **capacity**.
#[must_use]
pub fn compatible(provided: &StoragePosture, required: &StorageNeed) -> Compatibility {
    let wants = derived_medium(required);

    // (1) Durability, with the External escape. `>=` is the literal relation —
    // Durability's declaration order IS its ordering.
    let because = if required.locus == Locus::External {
        Because::StateIsExternal
    } else if provided.durability >= required.durability {
        Because::DurabilitySuffices {
            provided: provided.durability,
            required: required.durability,
        }
    } else {
        return Compatibility::Refused(Refusal::DurabilityShortfall {
            provided: provided.durability,
            required: required.durability,
            wants,
        });
    };

    // (2) Fidelity — orthogonal to durability, and to the escape above.
    if required.fidelity == Fidelity::MatchesProduction
        && provided.io_profile == IoProfile::MemorySpeed
    {
        return Compatibility::Refused(Refusal::FidelityShortfall {
            provided: provided.io_profile,
            required: required.fidelity,
            wants,
        });
    }

    // (3) Capacity, when the component states a floor.
    if let Some(required_bytes) = required.min_bytes
        && provided.capacity_bytes < required_bytes
    {
        return Compatibility::Refused(Refusal::CapacityShortfall {
            provided_bytes: provided.capacity_bytes,
            required_bytes,
            against_pod_memory: provided.io_profile.counts_against_pod_memory(),
        });
    }

    Compatibility::Holds(because)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// The prescribed variedade posture every row below is judged against: a
    /// 2 GiB ramdisk. `theory/RAMDISK.md`'s default, stated once.
    fn prescribed_ramdisk() -> StoragePosture {
        StoragePosture::ramdisk(2 * GIB).expect("a bounded ramdisk is a legal posture")
    }

    fn need(d: Durability, l: Locus, f: Fidelity) -> StorageNeed {
        StorageNeed::new(d, l, f).expect("legal need")
    }

    /// One row of the scenario matrix: a named component, what it needs, and
    /// the verdict the prescribed ramdisk earns for it.
    struct Row {
        scenario: &'static str,
        need: StorageNeed,
        expect_wire: &'static str,
        holds: bool,
    }

    /// The matrix. Its LENGTH is the denominator every assertion below carries,
    /// so a row silently lost fails the count rather than passing vacuously.
    fn matrix() -> Vec<Row> {
        vec![
            // ── admitted, reason #1: nothing outlives the PR ────────────────
            Row {
                scenario: "branch/PR test environment",
                need: need(Durability::PodLifetime, Locus::InPod, Fidelity::Indifferent),
                expect_wire: "durability-suffices",
                holds: true,
            },
            // ── admitted, reason #2: the state is a row inside RDS ──────────
            // The drill Jobs declare zero volumes; the authoritative bytes are
            // external, so the pod medium is not where durability is asked.
            Row {
                scenario: "PITR drill",
                need: need(
                    Durability::Recoverable,
                    Locus::External,
                    Fidelity::Indifferent,
                ),
                expect_wire: "state-is-external",
                holds: true,
            },
            // ── refused, reason #1: durability ──────────────────────────────
            // The restore target IS the in-cluster database: the restored bytes
            // must outlive the pod that wrote them.
            Row {
                scenario: "DR-restore rehearsal",
                need: need(
                    Durability::ClusterLifetime,
                    Locus::InCluster,
                    Fidelity::Indifferent,
                ),
                expect_wire: "durability-shortfall",
                holds: false,
            },
            // ── refused, reason #2: fidelity ────────────────────────────────
            // Pod-lifetime is genuinely enough; the numbers are the deliverable
            // and a tmpfs makes them a measurement of RAM.
            Row {
                scenario: "perf/soak run",
                need: need(
                    Durability::PodLifetime,
                    Locus::InPod,
                    Fidelity::MatchesProduction,
                ),
                expect_wire: "fidelity-shortfall",
                holds: false,
            },
            // ── admitted: a scan holds an unpacked image, nothing more ──────
            Row {
                scenario: "security scan target",
                need: need(Durability::PodLifetime, Locus::InPod, Fidelity::Indifferent)
                    .with_min_bytes(GIB),
                expect_wire: "durability-suffices",
                holds: true,
            },
            // ── migration rehearsal, arm A: the answer is pass/fail ─────────
            Row {
                scenario: "migration rehearsal (verdict-only)",
                need: need(Durability::PodLifetime, Locus::InPod, Fidelity::Indifferent),
                expect_wire: "durability-suffices",
                holds: true,
            },
            // ── migration rehearsal, arm B: the migrated database IS the ────
            // deliverable, inspected by a later stage.
            Row {
                scenario: "migration rehearsal (upgraded database is the artifact)",
                need: need(
                    Durability::ClusterLifetime,
                    Locus::InCluster,
                    Fidelity::Indifferent,
                ),
                expect_wire: "durability-shortfall",
                holds: false,
            },
        ]
    }

    #[test]
    fn every_matrix_row_earns_its_stated_verdict_and_reason() {
        let posture = prescribed_ramdisk();
        let rows = matrix();
        let mut scanned = 0_usize;
        let mut admitted = 0_usize;
        let mut refused = 0_usize;

        for row in &rows {
            let verdict = compatible(&posture, &row.need);
            assert_eq!(
                verdict.as_wire(),
                row.expect_wire,
                "{}: wrong deciding clause — rendered {verdict}",
                row.scenario
            );
            assert_eq!(verdict.holds(), row.holds, "{}", row.scenario);
            scanned += 1;
            if verdict.holds() {
                admitted += 1;
            } else {
                refused += 1;
            }
        }

        // The denominator lives INSIDE the compared value: a lost row, or a
        // matrix that stopped producing rows at all, fails here rather than
        // passing over an empty iteration.
        assert_eq!(
            (scanned, admitted, refused),
            (7, 4, 3),
            "the scenario matrix must judge 7 rows — 4 admitted, 3 refused"
        );
        assert_eq!(rows.len(), scanned, "a row was skipped rather than judged");
    }

    #[test]
    fn the_two_admissions_hold_for_opposite_reasons() {
        let posture = prescribed_ramdisk();

        let pr = compatible(
            &posture,
            &need(Durability::PodLifetime, Locus::InPod, Fidelity::Indifferent),
        );
        let pitr = compatible(
            &posture,
            &need(
                Durability::Recoverable,
                Locus::External,
                Fidelity::Indifferent,
            ),
        );

        assert!(pr.holds() && pitr.holds());
        assert_eq!(
            pr,
            Compatibility::Holds(Because::DurabilitySuffices {
                provided: Durability::PodLifetime,
                required: Durability::PodLifetime,
            })
        );
        assert_eq!(pitr, Compatibility::Holds(Because::StateIsExternal));
        assert_ne!(pr.as_wire(), pitr.as_wire());
        assert_ne!(pr.to_string(), pitr.to_string());

        // The PITR row is admitted DESPITE requiring the top durability rung —
        // which is exactly what the External arm buys and what a boolean or a
        // durability-only relation would have got wrong.
        assert!(Durability::Recoverable > posture.durability());
    }

    #[test]
    fn the_two_refusals_fail_for_opposite_reasons_and_do_not_render_alike() {
        let posture = prescribed_ramdisk();

        let dr = compatible(
            &posture,
            &need(
                Durability::ClusterLifetime,
                Locus::InCluster,
                Fidelity::Indifferent,
            ),
        );
        let soak = compatible(
            &posture,
            &need(
                Durability::PodLifetime,
                Locus::InPod,
                Fidelity::MatchesProduction,
            ),
        );

        assert_eq!(
            dr,
            Compatibility::Refused(Refusal::DurabilityShortfall {
                provided: Durability::PodLifetime,
                required: Durability::ClusterLifetime,
                wants: Medium::ClusterVolume,
            })
        );
        assert_eq!(
            soak,
            Compatibility::Refused(Refusal::FidelityShortfall {
                provided: IoProfile::MemorySpeed,
                required: Fidelity::MatchesProduction,
                wants: Medium::EphemeralBlock,
            })
        );

        // Same arm of Compatibility, different clause, different rendering.
        assert_ne!(dr.as_wire(), soak.as_wire());
        assert_ne!(dr.to_string(), soak.to_string());
        assert!(dr.to_string().starts_with("incompatible: durability:"));
        assert!(soak.to_string().starts_with("incompatible: fidelity:"));

        // And the soak's durability was never the problem: the same need with
        // an indifferent fidelity is admitted against the same posture.
        assert!(
            compatible(
                &posture,
                &need(Durability::PodLifetime, Locus::InPod, Fidelity::Indifferent)
            )
            .holds()
        );
    }

    #[test]
    fn capacity_is_a_third_clause_with_its_own_rendering() {
        let posture = prescribed_ramdisk(); // 2 GiB
        let too_big = need(Durability::PodLifetime, Locus::InPod, Fidelity::Indifferent)
            .with_min_bytes(8 * GIB);

        let verdict = compatible(&posture, &too_big);
        assert_eq!(verdict.as_wire(), "capacity-shortfall");
        assert_eq!(
            verdict,
            Compatibility::Refused(Refusal::CapacityShortfall {
                provided_bytes: 2 * GIB,
                required_bytes: 8 * GIB,
                against_pod_memory: true,
            })
        );
        // A tmpfs bound bills against the pod memory limit, and the rendering
        // says so — raising it silently is how an unattributable OOM is made.
        assert!(verdict.to_string().contains("memory limit"));

        // The same floor, met, is admitted.
        assert!(
            compatible(
                &posture,
                &need(Durability::PodLifetime, Locus::InPod, Fidelity::Indifferent)
                    .with_min_bytes(GIB)
            )
            .holds()
        );
    }

    #[test]
    fn the_medium_is_derived_over_every_legal_need() {
        let mut derived: Vec<(&'static str, &'static str)> = Vec::new();
        let mut refused_needs = 0_usize;
        let mut attempted = 0_usize;

        for d in Durability::ALL {
            for l in Locus::ALL {
                for f in Fidelity::ALL {
                    attempted += 1;
                    match StorageNeed::new(d, l, f) {
                        Ok(n) => derived.push((n.medium().as_str(), d.as_str())),
                        Err(StorageError::LocusTooNarrow { .. }) => refused_needs += 1,
                        Err(other) => panic!("unexpected need error: {other}"),
                    }
                }
            }
        }

        // Denominator inside the compared value: 3 durabilities x 3 loci x 2
        // fidelities = 18 attempts, of which the 4 outlive-the-pod-from-inside
        // combinations are parse-rejected.
        assert_eq!(
            (attempted, derived.len(), refused_needs),
            (18, 14, 4),
            "the need space must be enumerated, not sampled"
        );

        // Spot the derivations that carry the doctrine.
        assert_eq!(
            need(Durability::PodLifetime, Locus::InPod, Fidelity::Indifferent).medium(),
            Medium::Tmpfs,
            "the prescribed default"
        );
        assert_eq!(
            need(
                Durability::PodLifetime,
                Locus::InPod,
                Fidelity::MatchesProduction
            )
            .medium(),
            Medium::EphemeralBlock,
            "fidelity buys block I/O without buying durability"
        );
        assert_eq!(
            need(
                Durability::Recoverable,
                Locus::InCluster,
                Fidelity::Indifferent
            )
            .medium(),
            Medium::SnapshottableVolume,
            "only a snapshot-capable volume can be rewound"
        );
        assert_eq!(
            need(
                Durability::Recoverable,
                Locus::External,
                Fidelity::Indifferent
            )
            .medium(),
            Medium::ExternalStore,
            "External settles the medium before durability is asked"
        );
    }

    #[test]
    fn a_memory_backed_posture_cannot_claim_persistence() {
        // parse-time-rejected at the constructor …
        assert_eq!(
            StoragePosture::new(
                Durability::Recoverable,
                Locus::InPod,
                IoProfile::MemorySpeed,
                GIB
            ),
            Err(StorageError::MemoryCannotPersist {
                durability: Durability::Recoverable
            })
        );
        assert_eq!(
            StoragePosture::new(
                Durability::ClusterLifetime,
                Locus::InCluster,
                IoProfile::MemorySpeed,
                GIB
            ),
            Err(StorageError::MemoryCannotPersist {
                durability: Durability::ClusterLifetime
            })
        );
        // … and at the serde boundary, because Deserialize routes through it.
        let lie = r#"{"durability":"Recoverable","locus":"InPod","io_profile":"MemorySpeed","capacity_bytes":1073741824}"#;
        assert!(serde_json::from_str::<StoragePosture>(lie).is_err());

        // The honest shapes construct.
        assert!(StoragePosture::ramdisk(GIB).is_ok());
        assert!(
            StoragePosture::new(
                Durability::Recoverable,
                Locus::InCluster,
                IoProfile::ProductionBlock,
                8 * GIB
            )
            .is_ok()
        );
    }

    #[test]
    fn an_unbounded_posture_is_refused() {
        // A tmpfs with no bound turns a runaway write into an OOMKill that
        // names no cause — the bound is not optional.
        assert_eq!(
            StoragePosture::ramdisk(0),
            Err(StorageError::UnboundedCapacity)
        );
    }

    #[test]
    fn a_need_that_outlives_the_pod_cannot_live_inside_it() {
        assert_eq!(
            StorageNeed::new(
                Durability::ClusterLifetime,
                Locus::InPod,
                Fidelity::Indifferent
            ),
            Err(StorageError::LocusTooNarrow {
                durability: Durability::ClusterLifetime,
                locus: Locus::InPod,
            })
        );
        let bad = r#"{"durability":"Recoverable","locus":"InPod"}"#;
        assert!(serde_json::from_str::<StorageNeed>(bad).is_err());

        // An absent fidelity is not a request: the minimal need deserializes
        // and means Indifferent, so nothing that worked before this axis
        // existed is wedged by it.
        let minimal: StorageNeed =
            serde_json::from_str(r#"{"durability":"PodLifetime","locus":"InPod"}"#).unwrap();
        assert_eq!(minimal.fidelity(), Fidelity::Indifferent);
        assert_eq!(minimal.min_bytes(), None);
        assert_eq!(minimal.medium(), Medium::Tmpfs);
    }

    #[test]
    fn overflow_semantics_are_derived_from_the_profile() {
        assert_eq!(prescribed_ramdisk().overflow(), Overflow::OomKill);
        assert!(
            prescribed_ramdisk()
                .io_profile()
                .counts_against_pod_memory()
        );

        let block = StoragePosture::new(
            Durability::ClusterLifetime,
            Locus::InCluster,
            IoProfile::ProductionBlock,
            8 * GIB,
        )
        .unwrap();
        assert_eq!(block.overflow(), Overflow::NoSpaceLeft);
        assert!(!block.io_profile().counts_against_pod_memory());
    }

    #[test]
    fn durability_ordering_is_the_relation() {
        assert!(Durability::PodLifetime < Durability::ClusterLifetime);
        assert!(Durability::ClusterLifetime < Durability::Recoverable);
        let mut sorted = Durability::ALL;
        sorted.sort_unstable();
        assert_eq!(sorted, Durability::ALL, "ALL must be declared ascending");

        // Exactly the top two rungs need a volume a snapshot/PVC API can name.
        let named: Vec<_> = Durability::ALL
            .into_iter()
            .filter(|d| d.needs_a_named_volume())
            .collect();
        assert_eq!(
            named,
            vec![Durability::ClusterLifetime, Durability::Recoverable]
        );
    }

    #[test]
    fn every_catalog_is_total_and_uniquely_tokenized() {
        let mut seen = std::collections::HashSet::new();
        let mut counted = 0_usize;
        for t in Durability::ALL.map(Durability::as_str) {
            assert!(seen.insert(t), "duplicate token {t}");
            counted += 1;
        }
        for t in Locus::ALL.map(Locus::as_str) {
            assert!(seen.insert(t), "duplicate token {t}");
            counted += 1;
        }
        for t in Fidelity::ALL.map(Fidelity::as_str) {
            assert!(seen.insert(t), "duplicate token {t}");
            counted += 1;
        }
        for t in IoProfile::ALL.map(IoProfile::as_str) {
            assert!(seen.insert(t), "duplicate token {t}");
            counted += 1;
        }
        for t in Overflow::ALL.map(Overflow::as_str) {
            assert!(seen.insert(t), "duplicate token {t}");
            counted += 1;
        }
        for t in Medium::ALL.map(Medium::as_str) {
            assert!(seen.insert(t), "duplicate token {t}");
            counted += 1;
        }
        // 3 + 3 + 2 + 2 + 2 + 5 — the denominator, so a catalog that lost an
        // arm fails here instead of proving uniqueness over fewer tokens.
        assert_eq!((counted, seen.len()), (17, 17));
    }

    #[test]
    fn both_borders_round_trip_through_serde() {
        let n = need(
            Durability::Recoverable,
            Locus::External,
            Fidelity::MatchesProduction,
        )
        .with_min_bytes(4 * GIB);
        let back: StorageNeed = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
        assert_eq!(back, n);

        let p = prescribed_ramdisk();
        let back: StoragePosture =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}
