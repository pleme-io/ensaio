//! Mock [`EnsaioEnvironment`] for unit tests — no live cluster needed.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::fixture::Fixture;
use crate::interp::EnsaioEnvironment;
use crate::nervous::PhaseEvent;
use crate::types::{AssertionSpec, SystemSpec, Verdict, WiringSpec};

/// Records every call it receives, in order, so tests can assert on
/// the sequence (provision-before-wire, teardown-after-assert, …) —
/// same shape as `sui-spec::gc::MockEnv`.
pub struct MockEnv {
    pub verdicts: HashMap<String, Verdict>,
    pub fail_provision: Option<String>,
    pub fail_teardown: Option<String>,
    pub log: RefCell<Vec<String>>,
    /// Every [`PhaseEvent`] `publish_phase_event` received, in order —
    /// the direct assertion surface for the reactive-nervous-system
    /// extension point, alongside the generic
    /// `log`.
    pub phase_events: RefCell<Vec<PhaseEvent>>,
    pub fail_publish: bool,
    /// If set, `place_fixture` fails for this fixture name — the half-placed
    /// environment the assertion skip is proven against.
    pub fail_place: Option<String>,
    /// If set, `clean_fixture` fails for this fixture name.
    pub fail_clean: Option<String>,
}

impl MockEnv {
    #[must_use]
    pub fn new() -> Self {
        Self {
            verdicts: HashMap::new(),
            fail_provision: None,
            fail_teardown: None,
            log: RefCell::new(Vec::new()),
            phase_events: RefCell::new(Vec::new()),
            fail_publish: false,
            fail_place: None,
            fail_clean: None,
        }
    }

    /// Script `place_fixture` to fail for `fixture_name`.
    #[must_use]
    pub fn failing_place(mut self, fixture_name: &str) -> Self {
        self.fail_place = Some(fixture_name.into());
        self
    }

    /// Script `clean_fixture` to fail for `fixture_name`.
    #[must_use]
    pub fn failing_clean(mut self, fixture_name: &str) -> Self {
        self.fail_clean = Some(fixture_name.into());
        self
    }

    /// Make `publish_phase_event` return an `Err` — proves a nervous-
    /// system publish failure is non-fatal to the caller (see
    /// `controller::publish_phase_event`'s warn-and-continue wrapper).
    #[must_use]
    pub fn failing_publish(mut self) -> Self {
        self.fail_publish = true;
        self
    }

    #[must_use]
    pub fn with_verdict(mut self, assertion_id: &str, verdict: Verdict) -> Self {
        self.verdicts.insert(assertion_id.into(), verdict);
        self
    }

    #[must_use]
    pub fn failing_provision(mut self, system_id: &str) -> Self {
        self.fail_provision = Some(system_id.into());
        self
    }

    #[must_use]
    pub fn failing_teardown(mut self, system_id: &str) -> Self {
        self.fail_teardown = Some(system_id.into());
        self
    }
}

impl Default for MockEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl EnsaioEnvironment for MockEnv {
    fn provision_system(&self, sys: &SystemSpec) -> Result<String, String> {
        self.log.borrow_mut().push(format!("PROVISION {}", sys.id));
        if self.fail_provision.as_deref() == Some(sys.id.as_str()) {
            return Err(format!("{}: provision failed (mock)", sys.id));
        }
        Ok(format!("{}.mock.svc.cluster.local", sys.id))
    }

    fn wait_ready(&self, endpoint: &str) -> Result<(), String> {
        self.log.borrow_mut().push(format!("WAIT_READY {endpoint}"));
        Ok(())
    }

    fn wire(&self, w: &WiringSpec) -> Result<(), String> {
        self.log
            .borrow_mut()
            .push(format!("WIRE {}->{}", w.from, w.to));
        Ok(())
    }

    fn place_fixture(&self, f: &Fixture) -> Result<(), String> {
        // DISTINCT "PLACE"/"CLEAN" prefixes so the ordering assertions can tell
        // a placement from a probe — which is the whole reason a fixture is a
        // type rather than a `CustomJob` command that happens to mutate.
        self.log.borrow_mut().push(format!("PLACE {}", f.name));
        if self.fail_place.as_deref() == Some(f.name.as_str()) {
            return Err(format!("{}: place failed (mock)", f.name));
        }
        Ok(())
    }

    fn clean_fixture(&self, f: &Fixture) -> Result<(), String> {
        self.log.borrow_mut().push(format!("CLEAN {}", f.name));
        if self.fail_clean.as_deref() == Some(f.name.as_str()) {
            return Err(format!("{}: clean failed (mock)", f.name));
        }
        Ok(())
    }

    fn run_assertion(&self, a: &AssertionSpec) -> Result<Verdict, String> {
        self.log.borrow_mut().push(format!("ASSERT {}", a.id));
        // The default is a *scripted* pass, not a vacuous one: it goes
        // through `Verdict::judge` over a real, non-empty observation, so
        // even the mock cannot mint a hold that witnessed nothing. Tests
        // that want a different answer script it with `with_verdict`.
        Ok(self.verdicts.get(&a.id).cloned().unwrap_or_else(|| {
            Verdict::from_probe(
                vec![crate::types::Observation::new(
                    "no verdict configured in mock — defaulted to Held",
                )],
                true,
            )
        }))
    }

    fn teardown_system(&self, endpoint: &str) -> Result<(), String> {
        self.log.borrow_mut().push(format!("TEARDOWN {endpoint}"));
        if self
            .fail_teardown
            .as_deref()
            .is_some_and(|id| endpoint.starts_with(id))
        {
            return Err(format!("{endpoint}: teardown failed (mock)"));
        }
        Ok(())
    }

    fn publish_phase_event(&self, event: &PhaseEvent) -> Result<(), String> {
        self.log
            .borrow_mut()
            .push(format!("PUBLISH {}", event.subject()));
        self.phase_events.borrow_mut().push(event.clone());
        if self.fail_publish {
            return Err(format!("{}: publish failed (mock)", event.subject()));
        }
        Ok(())
    }
}
