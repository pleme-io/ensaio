//! The engine's command surface: the subcommands every deployment binary
//! carries, and the guarded kube client they act through.

use clap::Subcommand;
use kube::CustomResourceExt;

use crate::{EnsaioSpec, load_all};

pub const CERTIFY_NAMESPACE_ENV: &str = "ENSAIO_CERTIFY_NAMESPACE";

/// Cluster-endpoint fragments that mark a target this tool is FOR.
///
/// **This list only ever matches LOCAL clusters, and that is not a defect —
/// it is the measured reality.** The check runs against the API server's
/// HOST, and every managed-cloud endpoint is an opaque hash
/// (`B1C39B571BCEAD78B45E313F39671058.gr7.us-east-2.eks.amazonaws.com`), so no
/// substring distinguishes a cluster you own from one you are a guest in.
/// Verified 2026-08-11 against two such clusters.
///
/// The consequence, stated plainly so nobody "fixes" the list by adding a
/// cluster name to it and believing that helps: **for any managed-cloud
/// target, explicitness is REQUIRED.** The list buys a convenience exemption
/// for kind/minikube/local loops and nothing more. The real guarantee is the
/// ambient-vs-explicit split below, not name matching.
const INTENDED: &[&str] = &["kind", "minikube", "localhost", "127.0.0.1"];

/// Resolve a kube client, **announcing the cluster before acting**.
///
/// `Client::try_default()` silently takes whatever the ambient `KUBECONFIG`
/// chain's current-context happens to be. That is the whole defect: on a
/// workstation whose merged chain defaults to a cluster the operator is a
/// guest in, every invocation without an explicit context aims at someone
/// else's estate.
///
/// Measured twice on 2026-08-11. Once `kubectl` fell back to the ambient
/// default after a managed kubeconfig refused a write; once a spawn of an
/// ephemeral environment was aimed there and was stopped only by an expired
/// SSO token. Neither was a decision — both were ambient state deciding for
/// the operator, and the second would have created workloads in someone
/// else's cluster had the token been fresh.
///
/// Two properties, and the split matters:
///
/// - **It always announces.** A wrong target becomes visible rather than
///   silent, which is the half that failed both times.
/// - **Explicitness defeats the guard; ambience does not.** Passing
///   `--context` IS the statement of intent, so it is never blocked. Falling
///   through to the ambient default against an unrecognised cluster is
///   refused, because that is the case where nobody chose anything.
///
/// Tier: **only-mitigated (C1)**. What is actually enforced is "an ambient
/// default may not be acted on unless it is recognisably a local cluster" —
/// NOT "the target is ours". Two managed-cloud endpoints are both opaque
/// hashes. A caller who passes `--context` at the wrong cluster is obeyed,
/// correctly, because that is a decision.
///
/// The honest ceiling: this closes the SILENT-DEFAULT class. It does not close
/// the authorization one, which needs per-cluster credentials scoped to what we
/// may touch.
///
/// # Errors
///
/// A named context absent from the kubeconfig read, `--allow-foreign-cluster`
/// without `--context`, or an unrecognised ambient target.
pub async fn connect(context: Option<&str>, allow_foreign: bool) -> anyhow::Result<kube::Client> {
    use kube::config::{Config, KubeConfigOptions};

    // IN-CLUSTER FIRST, and this ordering is not a preference — it is the whole
    // correctness of running as a controller.
    //
    // A first version of this function called `Config::from_kubeconfig()`
    // unconditionally. That reads a FILE, and a pod has none: every controller
    // in the deployment went CrashLoopBackOff with
    // `failed to read kubeconfig from '/.kube/config'`. `Client::try_default()`
    // — what this replaced — tried in-cluster first and fell back to a
    // kubeconfig, which is exactly the behaviour a CLI-facing guard must not
    // take away.
    //
    // So: a pod is never guarded, because a pod has no ambient default to be
    // wrong about. Its identity IS its ServiceAccount, and the cluster it can
    // reach is the one it runs in.
    if context.is_none() && std::env::var("KUBERNETES_SERVICE_HOST").is_ok() {
        let config = Config::incluster()?;
        eprintln!(
            "ensaio: acting IN-CLUSTER ({})",
            config.cluster_url.host().unwrap_or("<unknown>")
        );
        return Ok(kube::Client::try_from(config)?);
    }

    // ── PROVENANCE BEFORE ACTION ────────────────────────────────────────
    //
    // `Config::from_kubeconfig` delegates file discovery to
    // `Kubeconfig::read()`, which merges the whole KUBECONFIG chain with
    // client-go first-file-wins semantics. So `--context` is NOT authority
    // over WHICH FILE is read — it is only a lookup key applied to whatever
    // merge happened to be assembled, and the caller never learns which files
    // those were.
    //
    // That cost a near-miss on 2026-08-12: a `KUBECONFIG=<file> ensaio ...
    // --context '<an ARN>'` failed `failed to load current context`, because
    // the prefix was lost at a process boundary and the workstation's managed
    // chain won — a chain whose current-context is a cluster we are a guest
    // in. Dropping --context and reaching for --allow-foreign-cluster then
    // aimed a spawn at the host, stopped only by an expired SSO token.
    //
    // Two files described the same cluster under DIFFERENT context names (one
    // used the ARN; the other a short name while declaring a current-context
    // no context in it defines), so a name that resolves against one
    // legitimately does not against the other.
    //
    // The fix is to stop delegating discovery: read the chain ourselves,
    // ANNOUNCE THE PATHS BEFORE LOADING, and refuse a named context that is
    // not in what we read — naming both the files searched and the contexts
    // found. kube-rs's own error does neither: it collapses "no such name"
    // and "context body is null" into one string and never cites a file.
    let sources: Vec<String> = std::env::var_os("KUBECONFIG").map_or_else(
        || vec!["<default ~/.kube/config>".to_string()],
        |v| {
            std::env::split_paths(&v)
                .map(|p| p.display().to_string())
                .collect()
        },
    );
    eprintln!("ensaio: kubeconfig sources: {}", sources.join(", "));

    let kubeconfig = kube::config::Kubeconfig::read()?;

    if let Some(name) = context {
        anyhow::ensure!(
            kubeconfig.contexts.iter().any(|c| c.name == name),
            "context `{name}` is not in the kubeconfig actually read.\n\
             \n\
             searched: {}\n\
             found:    {}\n\
             \n\
             A --context that names nothing is never a cluster choice, so this \
             refuses rather than falling back. If the file you meant is not in \
             `searched` above, your KUBECONFIG did not reach this process — do \
             NOT reach for --allow-foreign-cluster to get past this, because \
             that acts on the AMBIENT chain, which may resolve to a cluster \
             you are a guest in.",
            sources.join(", "),
            kubeconfig
                .contexts
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    let opts = KubeConfigOptions {
        context: context.map(ToString::to_string),
        ..Default::default()
    };
    let config = Config::from_custom_kubeconfig(kubeconfig, &opts).await?;
    let target = config.cluster_url.host().unwrap_or("<unknown>").to_string();

    let named = context.unwrap_or("<ambient current-context>");
    eprintln!("ensaio: acting against {named} ({target})");

    // `--allow-foreign-cluster` WITHOUT `--context` is refused, and this is the
    // half that was missing on 2026-08-12.
    //
    // The flag means "I have chosen this cluster." A choice NAMES ITS TARGET.
    // Used bare it means "act on whatever the ambient chain resolved", which is
    // not a choice at all — it is the absence of one, wearing the word `allow`.
    //
    // That distinction is not pedantic: when `--context` failed to resolve, the
    // obvious next move was to drop it and add this flag, and that aimed a
    // spawn at the host's cluster. A resolution bug should never make the
    // override the path of least resistance. Requiring both means the escape
    // hatch can only ever widen a target the caller already named.
    anyhow::ensure!(
        !(allow_foreign && context.is_none()),
        "--allow-foreign-cluster requires --context.\n\
         \n\
         The flag means \"I have chosen this cluster\", and a choice names its \
         target. Used alone it acts on whatever the ambient KUBECONFIG chain \
         resolved — which may be a cluster you are a guest in.\n\
         \n\
         If --context is failing to resolve, fix that (the sources and the \
         contexts found are printed above); do not route around it with this \
         flag."
    );

    if context.is_none() && !allow_foreign {
        let looks_intended = INTENDED.iter().any(|f| target.contains(f));
        anyhow::ensure!(
            looks_intended,
            "refusing to act against `{target}` resolved from the AMBIENT \
             current-context — nobody chose this cluster.\n\
             The merged KUBECONFIG's default may be a cluster you are a \
             guest in, and an ephemeral spawn there would create workloads \
             in someone else's estate.\n\
             Pass `--context <name>` to state the target, or \
             `--allow-foreign-cluster` if the ambient default is genuinely what \
             you want."
        );
    }

    Ok(kube::Client::try_from(config)?)
}

#[derive(Subcommand)]
pub enum CoreCommand {
    /// Compile a `.ensaio.lisp` file and report every declared scenario.
    Validate {
        /// Path to a `.ensaio.lisp` file.
        path: std::path::PathBuf,
    },
    /// Compile a `.ensaio.lisp` file and print it as JSON.
    Show {
        /// Path to a `.ensaio.lisp` file.
        path: std::path::PathBuf,
    },
    /// Run the M2 controller: watch `Ensaio` CRs cluster-wide, reconcile
    /// each toward a terminal `status.phase`. Requires a kubeconfig.
    RunController,
    /// Run one `.ensaio.lisp` scenario against a live cluster **now** and
    /// emit a typed [`crate::CertificationReceipt`] as JSON — the
    /// interpreter path (no controller redeploy needed). `CustomJob`
    /// probes land in `--namespace` (set it to the target env's
    /// namespace so same-namespace reach + default-allow apply).
    /// Exit: 0 = Certified, 1 = NotCertified, 2 = Inconclusive.
    Certify {
        /// Path to a `.ensaio.lisp` file (its first scenario is run).
        path: std::path::PathBuf,
        /// Namespace the probe Jobs are created in (also the env under
        /// test). Overridden per-assertion by `probe_namespace` when set.
        #[arg(long, env = "ENSAIO_CERTIFY_NAMESPACE")]
        namespace: String,
        /// Assertion id that is a gating precondition (repeatable). An
        /// unmet precondition routes every artifact assertion to
        /// `Skipped` and forces `Inconclusive`.
        #[arg(long = "precondition")]
        preconditions: Vec<String>,
        /// A non-gating known gap, `id=description` (repeatable) —
        /// surfaced in the receipt but never gating the verdict.
        #[arg(long = "known-gap")]
        known_gaps: Vec<String>,
    },
    /// Emit the `Ensaio` `CustomResourceDefinition` as YAML, generated
    /// directly from the typed Rust struct — never hand-authored, so
    /// the CRD YAML cannot drift from `crd.rs`.
    PrintCrd,
    /// A guest trial in an environment we do not own: snapshot its natural
    /// state, run declared reversible steps and operator-side assertions,
    /// undo, and prove the restore with a diff. Nothing lands in the target.
    /// Uses `--allow-context`, not the global `--context`.
    Visita {
        #[command(subcommand)]
        verb: crate::visita::cli::VisitaCommand,
    },
}

/// Run one core subcommand. `Visita` and `Certify` exit the process with
/// their own verdict code.
///
/// # Errors
///
/// Any failure reading a spec, reaching the cluster or running the controller.
pub async fn run_core(
    command: CoreCommand,
    context: Option<&str>,
    allow_foreign_cluster: bool,
) -> anyhow::Result<()> {
    match command {
        CoreCommand::Visita { verb } => {
            let code = crate::visita::cli::dispatch(verb)?;
            std::process::exit(code);
        }
        CoreCommand::Validate { path } => {
            let src = std::fs::read_to_string(&path)?;
            let specs = load_all::<EnsaioSpec>(&src)?;
            for spec in &specs {
                println!(
                    "{}  scope={}  ttl={}  systems={}  wiring={}  assertions={}",
                    spec.name,
                    spec.scope,
                    spec.ttl,
                    spec.systems.len(),
                    spec.wiring.len(),
                    spec.assertions.len()
                );
            }
            println!("{} scenario(s) valid", specs.len());
        }
        CoreCommand::Show { path } => {
            let src = std::fs::read_to_string(&path)?;
            let specs = load_all::<EnsaioSpec>(&src)?;
            println!("{}", serde_json::to_string_pretty(&specs)?);
        }
        CoreCommand::RunController => {
            let client = connect(context, allow_foreign_cluster).await?;
            crate::controller::run(client).await?;
        }
        CoreCommand::Certify {
            path,
            namespace,
            preconditions,
            known_gaps,
        } => {
            let src = std::fs::read_to_string(&path)?;
            let specs = load_all::<EnsaioSpec>(&src)?;
            let spec = specs
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("no scenario in {}", path.display()))?;
            let client = connect(context, allow_foreign_cluster).await?;
            let env = crate::k8s_env::K8sEnvironment::new(client, namespace);
            // `apply` runs provision/wire/assert/teardown; a `:systems ()`
            // cert spec has nothing to provision/wire/tear down, so only
            // the assertions run — each `CustomJob` a real batch/v1 Job.
            let outcome = crate::apply(&spec, &env)?;
            let gaps = known_gaps
                .iter()
                .map(|g| {
                    let (id, desc) = g.split_once('=').unwrap_or((g.as_str(), ""));
                    crate::KnownGap {
                        id: id.to_string(),
                        description: desc.to_string(),
                    }
                })
                .collect();
            let receipt = crate::CertificationReceipt::build(
                crate::CertificationInput {
                    run_id: spec.name.clone(),
                    scope: spec.scope.clone(),
                    precondition_ids: preconditions,
                    known_gaps: gaps,
                    at: chrono::Utc::now().to_rfc3339(),
                },
                &outcome.results,
            );
            println!("{}", serde_json::to_string_pretty(&receipt)?);
            std::process::exit(match receipt.verdict {
                crate::CertVerdict::Certified => 0,
                crate::CertVerdict::NotCertified => 1,
                crate::CertVerdict::Inconclusive => 2,
            });
        }
        CoreCommand::PrintCrd => {
            let crd = crate::crd::Ensaio::crd();
            println!("{}", serde_yaml_ng::to_string(&crd)?);
        }
    }
    Ok(())
}
