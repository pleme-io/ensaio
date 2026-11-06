//! `ensaio visita …` — the operator's handle on a guest trial.
//!
//! Every verb loads and validates the spec first, including that every kube
//! context it names was passed with `--allow-context`. A context the operator
//! did not name on the command line is never acted on.

use std::path::{Path, PathBuf};

use clap::Subcommand;

use super::command::{Evidence, Operator, ProcessRunner};
use super::config::{Target, VisitaConfig, VisitaFlags};
use super::env::OperatorEnvironment;
use super::snapshot::{self, NaturalState};
use super::spec::VisitaSpec;

#[derive(Subcommand, Debug)]
pub enum VisitaCommand {
    /// Compile and validate a `(defvisita …)` file; touch nothing.
    Validate {
        path: PathBuf,
        /// Kube context the spec may act on (repeatable). Every context the
        /// spec names must be listed.
        #[arg(long = "allow-context")]
        allow_context: Vec<String>,
        #[arg(long = "target")]
        target: Vec<String>,
        #[command(flatten)]
        config: shikumi::cli::ConfigArgs,
        #[command(flatten)]
        flags: VisitaFlags,
    },
    /// Capture the natural state only, into `<evidence>/natural-state.json`.
    Snapshot {
        path: PathBuf,
        #[arg(long = "allow-context")]
        allow_context: Vec<String>,
        #[arg(long = "target")]
        target: Vec<String>,
        #[command(flatten)]
        config: shikumi::cli::ConfigArgs,
        #[command(flatten)]
        flags: VisitaFlags,
        #[arg(long)]
        evidence: Option<PathBuf>,
        /// Read clusters through this kubeconfig instead of the ambient chain.
        #[arg(long)]
        kubeconfig: Option<PathBuf>,
    },
    /// Diff live state against a captured natural state. Exit 0 = restored.
    Diff {
        path: PathBuf,
        /// A `natural-state.json` written by `snapshot` or `run`.
        #[arg(long)]
        against: PathBuf,
        #[arg(long = "allow-context")]
        allow_context: Vec<String>,
        #[arg(long = "target")]
        target: Vec<String>,
        #[command(flatten)]
        config: shikumi::cli::ConfigArgs,
        #[command(flatten)]
        flags: VisitaFlags,
        #[arg(long)]
        evidence: Option<PathBuf>,
        #[arg(long)]
        kubeconfig: Option<PathBuf>,
    },
    /// Judge a recorded rollout (a visita's `rollout-*.jsonl` or a hand-run
    /// recorder's JSONL) against a surge bound. Touches no cluster. Exit 0 =
    /// held.
    JudgeRollout {
        /// JSONL, one sample per line.
        samples: PathBuf,
        /// `name=generation` baseline per Deployment (repeatable); a
        /// Deployment without one is judged from its first recorded
        /// generation onward.
        #[arg(long = "baseline")]
        baseline: Vec<String>,
        #[arg(long, default_value_t = 1)]
        max_surge: i64,
        #[arg(long, default_value_t = 0)]
        max_unavailable: i64,
        /// Judge only generations whose image tag starts with this.
        #[arg(long)]
        expect_image: Option<String>,
    },
    #[command(
        about = "Judge a branch plan against its trunk's plan (both `terraform show -json`, same live state): drift pending on trunk is absorbed, the rest must be declared occupancy. Touches nothing. Exit 0 = held"
    )]
    PlanDelta {
        baseline: PathBuf,
        candidate: PathBuf,
        #[arg(
            long = "expect",
            help = "`address#/json/pointer` (or `*#/pointer`) the branch may change; repeatable"
        )]
        expect: Vec<String>,
    },
    /// Snapshot, run the steps, undo, prove. Exit 0 = Certified (restored and
    /// every observation held), 1 = NotCertified, 2 = Inconclusive.
    Run {
        path: PathBuf,
        #[arg(long = "allow-context")]
        allow_context: Vec<String>,
        #[arg(long = "target")]
        target: Vec<String>,
        #[command(flatten)]
        config: shikumi::cli::ConfigArgs,
        #[command(flatten)]
        flags: VisitaFlags,
        #[arg(long)]
        evidence: Option<PathBuf>,
        #[arg(long)]
        kubeconfig: Option<PathBuf>,
    },
    #[command(
        about = "Finish runs whose process died: for each expired visita lease this operator holds (or --run), replay the persisted undo plan, release the lease, and diff against the run's natural state. Exit 0 = every reaped run restored"
    )]
    Reap {
        #[arg(long = "target", conflicts_with = "context")]
        target: Option<String>,
        #[arg(long = "context")]
        context: Option<String>,
        #[arg(long = "run")]
        run: Option<String>,
        #[command(flatten)]
        config: shikumi::cli::ConfigArgs,
        #[command(flatten)]
        flags: VisitaFlags,
        #[arg(long)]
        kubeconfig: Option<PathBuf>,
    },
}

pub struct Loaded {
    pub spec: VisitaSpec,
    pub config: VisitaConfig,
    pub target: Option<(String, Target)>,
}

fn load(
    path: &Path,
    allow: &[String],
    targets: &[String],
    config: &shikumi::cli::ConfigArgs,
    flags: &VisitaFlags,
) -> anyhow::Result<Loaded> {
    let cfg = VisitaConfig::resolve(config, flags).map_err(|e| anyhow::anyhow!(e))?;
    let src = std::fs::read_to_string(path)?;
    let spec = crate::load_one::<VisitaSpec>(&src)?
        .relative_to(path.parent().unwrap_or_else(|| Path::new(".")));
    prepare(
        spec,
        &cfg,
        allow,
        targets,
        &super::tier::open_windows(&cfg.windows_dir()),
        guardrail::windows::now_unix(),
    )
    .map_err(|e| anyhow::anyhow!("{} is not runnable:\n{e}", path.display()))
}

pub fn prepare(
    mut spec: VisitaSpec,
    cfg: &VisitaConfig,
    allow: &[String],
    targets: &[String],
    windows: &[guardrail::model::ChangeWindow],
    now_unix: i64,
) -> Result<Loaded, String> {
    cfg.check()?;
    let mut allowed: Vec<String> = allow.to_vec();
    for t in targets {
        let entry = cfg
            .targets
            .get(t)
            .ok_or_else(|| format!("--target `{t}` is not in the visita target registry"))?;
        allowed.push(entry.context.clone());
    }
    if let Some(t) = spec.target.clone() {
        let entry = cfg
            .targets
            .get(&t)
            .ok_or_else(|| format!(":target `{t}` is not in the visita target registry"))?;
        if !targets.contains(&t) {
            return Err(format!(
                ":target `{t}` is not allowed; pass --target {t} to name it"
            ));
        }
        spec.fill_target_context(&entry.context);
    }
    spec.validate(&allowed)?;
    let refusals = super::tier::check(&spec, cfg, windows, now_unix);
    if !refusals.is_empty() {
        return Err(refusals
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let target = spec
        .target
        .as_ref()
        .and_then(|t| cfg.targets.get(t).map(|e| (t.clone(), e.clone())))
        .or_else(|| {
            spec.contexts().iter().find_map(|c| {
                cfg.target_for_context(c)
                    .map(|(n, e)| (n.to_string(), e.clone()))
            })
        });
    Ok(Loaded {
        spec,
        config: cfg.clone(),
        target,
    })
}

fn evidence_dir(explicit: Option<PathBuf>, name: &str) -> PathBuf {
    explicit.unwrap_or_else(|| {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("ensaio/visita")
            .join(name)
            .join(chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string())
    })
}

/// Run one verb; returns the process exit code.
///
/// # Errors
///
/// A spec that does not load or validate, or an evidence directory that
/// cannot be created.
pub fn dispatch(cmd: VisitaCommand) -> anyhow::Result<i32> {
    let runner = ProcessRunner;
    match cmd {
        VisitaCommand::Validate {
            path,
            allow_context,
            target,
            config,
            flags,
        } => {
            let loaded = load(&path, &allow_context, &target, &config, &flags)?;
            if let Some((name, t)) = &loaded.target {
                println!("target {name}: context={} tier={:?}", t.context, t.tier);
            }
            let spec = loaded.spec;
            println!(
                "{}  natural-state={} fixtures={} assertions={} steps={} contexts={:?}",
                spec.name,
                spec.natural_state.len(),
                spec.fixtures.len(),
                spec.assertions.len(),
                spec.steps.len(),
                spec.contexts()
            );
            Ok(0)
        }
        VisitaCommand::Snapshot {
            path,
            allow_context,
            target,
            config,
            flags,
            evidence,
            kubeconfig,
        } => {
            let spec = load(&path, &allow_context, &target, &config, &flags)?.spec;
            let dir = evidence_dir(evidence, &spec.name);
            let ev = Evidence::open(&dir)?;
            let op = Operator::new(&runner, &ev, kubeconfig);
            let n = snapshot::capture(&op, &spec.natural_state, "natural-state")
                .map_err(|e| anyhow::anyhow!(e))?;
            let p = ev.write("natural-state.json", &serde_json::to_vec_pretty(&n)?)?;
            println!(
                "{} objects, address {}, {}",
                n.objects.len(),
                n.address,
                p.display()
            );
            Ok(0)
        }
        VisitaCommand::Diff {
            path,
            against,
            allow_context,
            target,
            config,
            flags,
            evidence,
            kubeconfig,
        } => {
            let spec = load(&path, &allow_context, &target, &config, &flags)?.spec;
            let natural: NaturalState = serde_json::from_slice(&std::fs::read(&against)?)?;
            let dir = evidence_dir(evidence, &spec.name);
            let ev = Evidence::open(&dir)?;
            let op = Operator::new(&runner, &ev, kubeconfig);
            let live = snapshot::capture(&op, &spec.natural_state, "restore-check")
                .map_err(|e| anyhow::anyhow!(e))?;
            let d = snapshot::diff(&natural, &live, &snapshot::effective_allow(&spec.allow));
            let body = serde_json::to_vec_pretty(&d)?;
            ev.write("restore-diff.json", &body)?;
            println!("{}", String::from_utf8_lossy(&body));
            eprintln!("evidence: {}", dir.display());
            Ok(i32::from(!d.is_empty()))
        }
        VisitaCommand::JudgeRollout {
            samples,
            baseline,
            max_surge,
            max_unavailable,
            expect_image,
        } => {
            let text = std::fs::read_to_string(&samples)?;
            let rows: Vec<super::judge::RolloutSample> = text
                .lines()
                .filter(|l| l.contains("\"name\""))
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()?;
            let mut base: std::collections::BTreeMap<String, i64> =
                std::collections::BTreeMap::new();
            for b in &baseline {
                let (n, g) = b
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--baseline `{b}`: want name=generation"))?;
                base.insert(n.to_string(), g.parse()?);
            }
            for r in &rows {
                base.entry(r.name.clone()).or_insert(r.generation - 1);
            }
            let expect = expect_image.as_deref();
            let events =
                super::judge::rollout_events(&rows, &base, max_surge, max_unavailable, expect);
            let v = super::judge::rollout(&rows, &base, max_surge, max_unavailable, expect);
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "verdict": v, "events": events })
                )?
            );
            Ok(i32::from(!v.held()))
        }
        VisitaCommand::PlanDelta {
            baseline,
            candidate,
            expect,
        } => {
            let b: serde_json::Value = serde_json::from_slice(&std::fs::read(&baseline)?)?;
            let c: serde_json::Value = serde_json::from_slice(&std::fs::read(&candidate)?)?;
            let d = super::absorb::plan_delta(&b, &c, &expect).map_err(|e| anyhow::anyhow!(e))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "held": d.held(), "delta": d }))?
            );
            Ok(i32::from(!d.held()))
        }
        VisitaCommand::Run {
            path,
            allow_context,
            target,
            config,
            flags,
            evidence,
            kubeconfig,
        } => {
            let loaded = load(&path, &allow_context, &target, &config, &flags)?;
            let spec = &loaded.spec;
            let run_id = super::lease::run_id(&spec.name, chrono::Utc::now());
            let run_dir = super::lease::run_dir(&super::config::state_home(), &run_id);
            let dir = match (&evidence, &loaded.target) {
                (Some(e), _) => e.clone(),
                (None, Some(_)) => run_dir.clone(),
                (None, None) => evidence_dir(None, &spec.name),
            };
            let ev = Evidence::open(&dir)?;
            let op = Operator::new(&runner, &ev, kubeconfig)
                .with_client_validation(loaded.config.kubectl.client_validation);
            let mut env = OperatorEnvironment::new(op).with_run(&run_id);
            if let Some((name, t)) = &loaded.target {
                let cfg = &loaded.config;
                let operator = cfg.operator_id();
                if cfg.lease.enabled {
                    let ttl = humantime::parse_duration(&cfg.lease.ttl)?;
                    env = env.with_lease(super::lease::LeaseRequest::new(
                        spec,
                        &t.context,
                        &cfg.lease_namespace(t),
                        &operator,
                        &run_id,
                        ttl,
                    ));
                }
                env = env.with_plan(
                    run_dir.clone(),
                    super::lease::RunPlan {
                        run: run_id.clone(),
                        operator,
                        target: Some(name.clone()),
                        lease: None,
                        spec: spec.clone(),
                        placed: Vec::new(),
                        ledger: serde_json::Value::Null,
                        finished: false,
                    },
                );
                eprintln!(
                    "run {run_id}: target {name} ({:?}), plan {}",
                    t.tier,
                    run_dir.display()
                );
            }

            let report = super::run(spec, &env).map_err(|e| anyhow::anyhow!(e))?;
            if dir != run_dir && loaded.target.is_some() {
                let _ = std::fs::copy(
                    dir.join("natural-state.json"),
                    run_dir.join("natural-state.json"),
                );
            }
            println!(
                "{}",
                std::fs::read_to_string(dir.join("summary.md")).unwrap_or_default()
            );
            eprintln!("evidence: {}", dir.display());
            Ok(match report.receipt.verdict {
                crate::CertVerdict::Certified => 0,
                crate::CertVerdict::NotCertified => 1,
                crate::CertVerdict::Inconclusive => 2,
            })
        }
        VisitaCommand::Reap {
            target,
            context,
            run,
            config,
            flags,
            kubeconfig,
        } => reap(
            &runner,
            target.as_deref(),
            context.as_deref(),
            run.as_deref(),
            &config,
            &flags,
            kubeconfig.as_ref(),
        ),
    }
}

fn reap(
    runner: &ProcessRunner,
    target: Option<&str>,
    context: Option<&str>,
    run: Option<&str>,
    config: &shikumi::cli::ConfigArgs,
    flags: &VisitaFlags,
    kubeconfig: Option<&PathBuf>,
) -> anyhow::Result<i32> {
    let cfg = VisitaConfig::resolve(config, flags).map_err(|e| anyhow::anyhow!(e))?;
    let (ctx, ns) = match (target, context) {
        (Some(t), _) => {
            let e = cfg.targets.get(t).ok_or_else(|| {
                anyhow::anyhow!("--target `{t}` is not in the visita target registry")
            })?;
            (e.context.clone(), cfg.lease_namespace(e))
        }
        (None, Some(c)) => match cfg.target_for_context(c) {
            Some((_, e)) => (c.to_string(), cfg.lease_namespace(e)),
            None => (
                c.to_string(),
                cfg.lease
                    .namespace
                    .clone()
                    .unwrap_or_else(|| "default".into()),
            ),
        },
        (None, None) => anyhow::bail!("reap needs --target <name> or --context <ctx>"),
    };
    let operator = cfg.operator_id();
    let state = super::config::state_home();
    let scratch = state
        .join("ensaio/visita/reap")
        .join(chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    let lister_ev = Evidence::open(&scratch)?;
    let lister = OperatorEnvironment::new(Operator::new(runner, &lister_ev, kubeconfig.cloned()));
    let leases = lister.leases(&ctx, &ns).map_err(|e| anyhow::anyhow!(e))?;
    let chosen = super::reap::select(&leases, &operator, run, chrono::Utc::now())
        .map_err(|e| anyhow::anyhow!(e))?;
    if chosen.is_empty() {
        println!("nothing to reap: no expired visita lease held by `{operator}` in {ctx}/{ns}");
        return Ok(0);
    }
    let mut all_restored = true;
    for r in chosen {
        let dir = super::lease::run_dir(&state, &r.run);
        let plan = match super::lease::read_plan(&dir) {
            Ok(p) => p,
            Err(e) => {
                all_restored = false;
                println!(
                    "{}: no undo plan ({e}); lease `{}` left in place",
                    r.run, r.lease
                );
                continue;
            }
        };
        let natural: NaturalState = match std::fs::read(dir.join("natural-state.json"))
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        {
            Ok(n) => n,
            Err(e) => {
                all_restored = false;
                println!(
                    "{}: no natural state ({e}); lease `{}` left in place",
                    r.run, r.lease
                );
                continue;
            }
        };
        let ev_dir = dir.join(format!(
            "reap-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        ));
        let ev = Evidence::open(&ev_dir)?;
        let env = OperatorEnvironment::new(
            Operator::new(runner, &ev, kubeconfig.cloned())
                .with_client_validation(cfg.kubectl.client_validation),
        )
        .with_run(&plan.run)
        .with_plan(dir.clone(), plan.clone());
        let report = super::reap::reap(&env, &plan, &natural).map_err(|e| anyhow::anyhow!(e))?;
        let restored = report.verdict.tag() == "held" && report.undo_errors.is_empty();
        all_restored &= restored;
        println!(
            "{}: {} — undone {:?}, {} differing, {} missing, {} extra; evidence {}",
            r.run,
            report.verdict.tag(),
            report.undone,
            report.restore.differing.len(),
            report.restore.missing.len(),
            report.restore.extra.len(),
            ev_dir.display()
        );
    }
    Ok(i32::from(!all_restored))
}
