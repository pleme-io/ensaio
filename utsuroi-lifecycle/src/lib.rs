//! `utsuroi` (移ろい, *the passing / transition through states*) — the
//! shared ephemeral-resource **lifecycle spine**.
//!
//! # What this crate IS (the honest, narrow claim)
//!
//! A `/vocabularify` design established that viveiro (env broker), ensaio
//! (trial rig), kenshi (product-CI test), an ephemeral-tenant CR, and
//! a Go come-up tool do **not** share one `Phase` enum — they
//! share a *meta-shape* and a *stage skeleton* each projects into. The
//! adversarial verify then narrowed even that: the REAL, M0-provable shared
//! spine is exactly three things —
//!
//! 1. a [`LifecycleStage`] **projection** ([`stage`]) every domain phase FSM
//!    maps onto, plus the [`DomainPhase`] adapter trait ([`phase`]) that
//!    declares that projection additively (giving kenshi/tenant a
//!    catalog-reflection `ALL` for free);
//! 2. the **already-shared code** [`StepOutcome`] / [`ControllerError`]
//!    ([`controller`]) — viveiro imports these from ensaio's own
//!    `crate::controller` today, so this crate relocates them to their real
//!    home and ensaio re-exports them (the live consumer keeps compiling);
//! 3. kenshi's [`TeardownPolicy`] ([`teardown`]) — the one genuinely-typed
//!    reap-policy sum in the family, lifted verbatim.
//!
//! On top of that spine it adds two small border pieces the verify's corrected
//! scope demanded: a **parse-rejecting** [`Ttl`] ([`ttl`]) and a **passive**
//! [`Lease`] value + a four-arm [`Boundedness`] sum aligned with
//! [`ReapMechanism`] ([`lease`]).
//!
//! # What this crate is NOT (never rounded up)
//!
//! M0 ships **zero sealing**. There is no phantom-typestate `Lifecycle<Ready>`
//! FSM here (an illegal transition is still a runtime `match` in each domain,
//! not an `E0599`); there is no controller, no pool, no `(deflifecycle)` lisp
//! form, no witness-carrying `Terminal::{Accepted(ReapProof)|Rejected(_)}`
//! sealed constructor. Those are the named-M1 destination.
//! The single M0 guarantee that IS earned at the type level is [`Ttl`]'s
//! **parse-time rejection** of a zero/absent/unparseable ttl (a value the
//! reap-firing itself remains a runtime world-fact — only-mitigated).
//!
//! # What M0.1 adds — NESTING (2026-08-29)
//!
//! [`SubMachine`] ([`substage`]) — a [`LifecycleStage`] may carry an ORDERED
//! sub-machine, and [`Nested`] is a phase plus its position in it. This exists
//! because the flat projection is LOSSY where it matters: eleven viveiro
//! come-up states projected onto the single stage `Spawning`, and wiring the
//! spine in that shape would have made the collapse permanent.
//!
//! It also adds the arm the flat spine had no shape for. [`SubVerdict`] is
//! four-valued: `Holds` / `Pending` / **`AwaitingWindow`** / `Errored`. A wait
//! on an EXTERNAL CLOCK is neither pending nor errored, and — the load-bearing
//! difference — it must NOT consume a failure budget. Branch on
//! [`SubVerdict::is_budget_consuming`], never on a re-`match`.
//!
//! Still no sealing: an out-of-order step is caught by a derived
//! `predecessor()` and a test helper, not by a type. `Nested` carrying a step
//! from another stage's sub-machine is detectable
//! ([`Nested::step_belongs_to_stage`]) and not unrepresentable —
//! **only-mitigated**, said out loud.
//!
//! # What M0.2 adds — the STORAGE RELATION (2026-08-29)
//!
//! [`storage`] — `Durability × Locus`, the relation that decides whether a
//! permutation's storage posture may back a component's need. It is a relation
//! and not a field because RAMDISK is a property of a *variedade*, never of the
//! framework: the spine holds the rule, a permutation holds the choice, and the
//! MEDIUM is derived ([`derived_medium`]) rather than declared. The verdict is
//! typed, not a `bool` — two components are admitted for opposite reasons
//! (nothing outlives them / their state is external) and two are refused for
//! opposite reasons (durability / fidelity), and a boolean renders all four
//! alike.
//!
//! `ishou_tokens::Refined` is deliberately **not** used for [`Ttl`]: its
//! `Deserialize` *clamps* an out-of-range value to a bound; a lease ttl must
//! *reject* a non-positive one at the parse boundary. Clamp and reject are
//! different algebras, so [`Ttl`] is not a fourth copy of the clamp primitive
//! (see [`ttl`]).

pub mod controller;
pub mod lease;
pub mod phase;
pub mod stage;
pub mod storage;
pub mod substage;
pub mod teardown;
pub mod ttl;

pub use controller::{ControllerError, StepOutcome};
pub use lease::{Boundedness, Lease, LifecycleRef, ReapMechanism};
pub use phase::DomainPhase;
pub use stage::LifecycleStage;
pub use storage::{
    Because, Compatibility, Durability, Fidelity, IoProfile, Locus, Medium, Overflow, Refusal,
    StorageError, StorageNeed, StoragePosture, compatible, derived_medium,
};
pub use substage::{Nested, SubMachine, SubVerdict};
pub use teardown::{Outcome, TeardownPolicy};
pub use ttl::{Ttl, TtlError};
