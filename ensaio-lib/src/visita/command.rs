//! The closed set of things a visita may ask a borrowed environment to do,
//! and the evidence trail every one of them leaves.
//!
//! **There is no `apply` verb, and `create`/`delete` exist for one fixture
//! only.** A visita reads through the target's own API and writes only what a
//! declared step names: an annotation, a merge patch, an exec, a workflow
//! dispatch, or the objects of an `:apply-manifest` fixture. [`Verb::Create`]
//! is `kubectl create` (never `apply`), so an object that already exists is
//! refused by the server as well as by the fixture's own check before it; and
//! the environment builds [`Verb::Delete`] only from the identities a create
//! recorded, uid-checked, so the trial deletes nothing it did not create.
//! Tier: **only-mitigated** — the verbs are constructible, and the environment
//! is what confines them — and **only-mitigated** for what an exec or a merge
//! patch can do once allowed, which is why each is a named step with an
//! inverse or a stated reason.
//!
//! Every invocation names its `--context`. There is no ambient-context path:
//! the context is a field of every cluster verb, not a default.

use std::cell::{Cell, RefCell};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;

/// One request to the target, typed. The argv is derived, never authored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verb {
    /// `kubectl get <resources> -o json`, optionally by label selector.
    Get {
        context: String,
        namespace: String,
        resources: String,
        selector: Option<String>,
    },
    /// `kubectl get <resource> <name>… -o json` — a read of named objects,
    /// for a sampler that must not pull a whole namespace every few seconds.
    GetNamed {
        context: String,
        namespace: String,
        resource: String,
        names: Vec<String>,
    },
    /// `kubectl create -f <file> -o json` — the objects of an
    /// `:apply-manifest` fixture, already checked absent.
    Create {
        context: String,
        namespace: String,
        file: String,
    },
    /// `kubectl delete <resource> <name> --wait=false` — only ever an object
    /// this run created, by its recorded identity.
    Delete {
        context: String,
        namespace: String,
        resource: String,
        name: String,
    },
    DeletePod {
        context: String,
        namespace: String,
        pod: String,
    },
    /// `kubectl get events --field-selector reason=<reason> -o json`.
    Events {
        context: String,
        namespace: String,
        reason: String,
    },
    /// `kubectl get --raw <path>` — the pod proxy `/metrics` read.
    GetRaw { context: String, path: String },
    /// `kubectl logs <pod> --tail=-1`, bounded by a window. One pod per read:
    /// `kubectl logs -l` exits non-zero on the first pod that vanishes and
    /// throws away every line it already read from the others.
    Logs {
        context: String,
        namespace: String,
        pod: String,
        since: Since,
    },
    /// `kubectl annotate --overwrite`; `value: None` removes the key.
    Annotate {
        context: String,
        namespace: String,
        resource: String,
        name: String,
        key: String,
        value: Option<String>,
    },
    /// `kubectl patch --type merge`, optionally against a subresource.
    MergePatch {
        context: String,
        namespace: String,
        resource: String,
        name: String,
        patch: String,
        subresource: Option<String>,
    },
    /// `kubectl patch pod <pod> --subresource resize --type strategic -p <body>`
    /// — the in-place resize. Strategic, because a JSON merge patch would
    /// replace the whole containers list.
    ResizePatch {
        context: String,
        namespace: String,
        pod: String,
        body: String,
    },
    /// `kubectl rollout restart deployment <names>…`.
    RolloutRestart {
        context: String,
        namespace: String,
        deployments: Vec<String>,
    },
    /// `kubectl exec` of a command the spec DECLARES read-only (a broker's
    /// run-queue statistic). Recorded as a read; that it really is one is the
    /// author's claim, not something this type can check.
    ExecRead {
        context: String,
        namespace: String,
        pod: String,
        container: String,
        argv: Vec<String>,
    },
    /// `kubectl exec <pod> -c <container> -- <argv>`.
    Exec {
        context: String,
        namespace: String,
        pod: String,
        container: String,
        argv: Vec<String>,
    },
    /// `gh api <path>` — a GitHub REST read (a branch-vs-trunk compare).
    GhApi { path: String },
    /// `gh run view <id> -R <repo> --json status,conclusion,jobs` — a read.
    RunView { repo: String, run_id: String },
    /// `gh run cancel <id> -R <repo>`.
    RunCancel { repo: String, run_id: String },
    /// `gh workflow run <workflow> -R <repo> --ref <ref> -f k=v…`.
    WorkflowRun {
        repo: String,
        workflow: String,
        git_ref: String,
        inputs: Vec<(String, String)>,
    },
}

/// How far back a log read reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Since {
    /// A relative window, `--since=10m`.
    Window(String),
    /// An absolute start, `--since-time=<rfc3339>`.
    Time(String),
}

impl Verb {
    /// The program and its arguments. `kubeconfig` is threaded into every
    /// `kubectl` call when set, so a scratch cluster never reads the ambient
    /// chain.
    #[must_use]
    pub fn argv(&self, kubeconfig: Option<&Path>) -> Vec<String> {
        let kube = |context: &str, rest: Vec<String>| {
            let mut v = vec!["kubectl".to_string()];
            if let Some(k) = kubeconfig {
                v.push("--kubeconfig".into());
                v.push(k.display().to_string());
            }
            v.push("--context".into());
            v.push(context.to_string());
            v.extend(rest);
            v
        };
        match self {
            Self::Get {
                context,
                namespace,
                resources,
                selector,
            } => {
                let mut rest = vec![
                    "-n".into(),
                    namespace.clone(),
                    "get".into(),
                    resources.clone(),
                    "-o".into(),
                    "json".into(),
                ];
                if let Some(s) = selector {
                    rest.push("-l".into());
                    rest.push(s.clone());
                }
                kube(context, rest)
            }
            Self::GetNamed {
                context,
                namespace,
                resource,
                names,
            } => {
                let mut rest = vec![
                    "-n".into(),
                    namespace.clone(),
                    "get".into(),
                    resource.clone(),
                ];
                rest.extend(names.iter().cloned());
                rest.push("-o".into());
                rest.push("json".into());
                kube(context, rest)
            }
            Self::GetRaw { context, path } => {
                kube(context, vec!["get".into(), "--raw".into(), path.clone()])
            }
            Self::Create {
                context,
                namespace,
                file,
            } => kube(
                context,
                vec![
                    "-n".into(),
                    namespace.clone(),
                    "create".into(),
                    "-f".into(),
                    file.clone(),
                    "-o".into(),
                    "json".into(),
                ],
            ),
            Self::Delete {
                context,
                namespace,
                resource,
                name,
            } => kube(
                context,
                vec![
                    "-n".into(),
                    namespace.clone(),
                    "delete".into(),
                    resource.clone(),
                    name.clone(),
                    "--wait=false".into(),
                ],
            ),
            Self::DeletePod {
                context,
                namespace,
                pod,
            } => kube(
                context,
                vec![
                    "-n".into(),
                    namespace.clone(),
                    "delete".into(),
                    "pod".into(),
                    pod.clone(),
                    "--wait=false".into(),
                ],
            ),
            Self::Events {
                context,
                namespace,
                reason,
            } => kube(
                context,
                vec![
                    "-n".into(),
                    namespace.clone(),
                    "get".into(),
                    "events".into(),
                    "--field-selector".into(),
                    format!("reason={reason}"),
                    "-o".into(),
                    "json".into(),
                ],
            ),
            Self::Logs {
                context,
                namespace,
                pod,
                since,
            } => {
                let since_arg = match since {
                    Since::Window(w) => format!("--since={w}"),
                    Since::Time(t) => format!("--since-time={t}"),
                };
                kube(
                    context,
                    vec![
                        "-n".into(),
                        namespace.clone(),
                        "logs".into(),
                        pod.clone(),
                        since_arg,
                        "--tail=-1".into(),
                    ],
                )
            }
            Self::GhApi { path } => vec!["gh".into(), "api".into(), path.clone()],
            Self::RunView { repo, run_id } => vec![
                "gh".into(),
                "run".into(),
                "view".into(),
                run_id.clone(),
                "-R".into(),
                repo.clone(),
                "--json".into(),
                "status,conclusion,jobs".into(),
            ],
            Self::RunCancel { repo, run_id } => vec![
                "gh".into(),
                "run".into(),
                "cancel".into(),
                run_id.clone(),
                "-R".into(),
                repo.clone(),
            ],
            Self::Annotate {
                context,
                namespace,
                resource,
                name,
                key,
                value,
            } => {
                let kv = value
                    .as_ref()
                    .map_or_else(|| format!("{key}-"), |v| format!("{key}={v}"));
                kube(
                    context,
                    vec![
                        "-n".into(),
                        namespace.clone(),
                        "annotate".into(),
                        "--overwrite".into(),
                        resource.clone(),
                        name.clone(),
                        kv,
                    ],
                )
            }
            Self::MergePatch {
                context,
                namespace,
                resource,
                name,
                patch,
                subresource,
            } => {
                let mut rest = vec![
                    "-n".into(),
                    namespace.clone(),
                    "patch".into(),
                    resource.clone(),
                    name.clone(),
                    "--type".into(),
                    "merge".into(),
                    "-p".into(),
                    patch.clone(),
                ];
                if let Some(s) = subresource {
                    rest.push(format!("--subresource={s}"));
                }
                kube(context, rest)
            }
            Self::ResizePatch {
                context,
                namespace,
                pod,
                body,
            } => kube(
                context,
                vec![
                    "-n".into(),
                    namespace.clone(),
                    "patch".into(),
                    "pod".into(),
                    pod.clone(),
                    "--subresource".into(),
                    "resize".into(),
                    "--type".into(),
                    "strategic".into(),
                    "-p".into(),
                    body.clone(),
                ],
            ),
            Self::RolloutRestart {
                context,
                namespace,
                deployments,
            } => {
                let mut rest = vec![
                    "-n".into(),
                    namespace.clone(),
                    "rollout".into(),
                    "restart".into(),
                    "deployment".into(),
                ];
                rest.extend(deployments.iter().cloned());
                kube(context, rest)
            }
            Self::ExecRead {
                context,
                namespace,
                pod,
                container,
                argv,
            }
            | Self::Exec {
                context,
                namespace,
                pod,
                container,
                argv,
            } => {
                let mut rest = vec![
                    "-n".into(),
                    namespace.clone(),
                    "exec".into(),
                    pod.clone(),
                    "-c".into(),
                    container.clone(),
                    "--".into(),
                ];
                rest.extend(argv.iter().cloned());
                kube(context, rest)
            }
            Self::WorkflowRun {
                repo,
                workflow,
                git_ref,
                inputs,
            } => {
                let mut v = vec![
                    "gh".into(),
                    "workflow".into(),
                    "run".into(),
                    workflow.clone(),
                    "-R".into(),
                    repo.clone(),
                    "--ref".into(),
                    git_ref.clone(),
                ];
                for (k, val) in inputs {
                    v.push("-f".into());
                    v.push(format!("{k}={val}"));
                }
                v
            }
        }
    }

    /// Whether this verb changes the target. Reads never do.
    #[must_use]
    pub fn mutates(&self) -> bool {
        match self {
            Self::Get { .. }
            | Self::GetNamed { .. }
            | Self::GetRaw { .. }
            | Self::Events { .. }
            | Self::Logs { .. }
            | Self::RunView { .. }
            | Self::GhApi { .. }
            | Self::ExecRead { .. } => false,
            Self::Annotate { .. }
            | Self::MergePatch { .. }
            | Self::Exec { .. }
            | Self::WorkflowRun { .. }
            | Self::RunCancel { .. }
            | Self::ResizePatch { .. }
            | Self::RolloutRestart { .. }
            | Self::Create { .. }
            | Self::Delete { .. }
            | Self::DeletePod { .. } => true,
        }
    }

    /// The kube context this verb acts on, when it acts on a cluster.
    #[must_use]
    pub fn context(&self) -> Option<&str> {
        match self {
            Self::Get { context, .. }
            | Self::GetNamed { context, .. }
            | Self::GetRaw { context, .. }
            | Self::Events { context, .. }
            | Self::Create { context, .. }
            | Self::Delete { context, .. }
            | Self::DeletePod { context, .. }
            | Self::Logs { context, .. }
            | Self::Annotate { context, .. }
            | Self::MergePatch { context, .. }
            | Self::ResizePatch { context, .. }
            | Self::RolloutRestart { context, .. }
            | Self::ExecRead { context, .. }
            | Self::Exec { context, .. } => Some(context),
            Self::WorkflowRun { .. }
            | Self::RunView { .. }
            | Self::RunCancel { .. }
            | Self::GhApi { .. } => None,
        }
    }
}

/// `kubectl`'s answer for an object that no longer exists — a pod the HPA or a
/// rollout removed between listing it and reading it.
#[must_use]
pub fn vanished(stderr: &str) -> bool {
    stderr.contains("(NotFound)")
}

/// What one invocation returned.
#[derive(Debug, Clone, Default)]
pub struct Exec {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Exec {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

/// The seam between a visita and a real process. Tests substitute an
/// in-memory cluster; the CLI uses [`ProcessRunner`].
pub trait Runner {
    fn run(&self, argv: &[String]) -> Exec;
}

/// Runs the argv as a child process. No shell: the argv is passed as-is, so
/// nothing is word-split, globbed or aliased.
pub struct ProcessRunner;

impl Runner for ProcessRunner {
    fn run(&self, argv: &[String]) -> Exec {
        let Some((program, args)) = argv.split_first() else {
            return Exec {
                status: -1,
                stdout: String::new(),
                stderr: "empty argv".into(),
            };
        };
        match std::process::Command::new(program).args(args).output() {
            Ok(out) => Exec {
                status: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => Exec {
                status: -1,
                stdout: String::new(),
                stderr: format!("could not start `{program}`: {e}"),
            },
        }
    }
}

/// One line of `steps.jsonl`: what was asked, when, and where its raw output is.
#[derive(Serialize, Debug, Clone)]
pub struct Invocation {
    pub seq: u32,
    pub at: String,
    pub step: String,
    pub argv: Vec<String>,
    pub mutates: bool,
    pub status: i32,
    pub duration_ms: u128,
    pub stdout_file: String,
    pub stderr_file: String,
}

/// The evidence directory: every invocation's raw output, plus a JSONL index.
///
/// Recorded BEFORE the result is interpreted, so a verdict can always be
/// traced back to the bytes it was derived from.
pub struct Evidence {
    dir: PathBuf,
    seq: Cell<u32>,
    index: RefCell<std::fs::File>,
}

impl Evidence {
    /// Open (creating) an evidence directory.
    ///
    /// # Errors
    ///
    /// When the directory or its index cannot be created.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir.join("raw"))?;
        let index = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("steps.jsonl"))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            seq: Cell::new(0),
            index: RefCell::new(index),
        })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write a named artifact (a snapshot, a diff, the report) into the bundle.
    ///
    /// # Errors
    ///
    /// When the file cannot be written.
    pub fn write(&self, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
        let p = self.dir.join(name);
        std::fs::write(&p, bytes)?;
        Ok(p)
    }

    /// Append bytes to a named artifact, creating it — for records written as
    /// they are taken (a rollout sample the moment it is read).
    ///
    /// # Errors
    ///
    /// When the file cannot be opened or written.
    pub fn append(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(name))?;
        f.write_all(bytes)
    }

    /// Append one typed line to the index (a verdict, a step boundary).
    pub fn note<T: Serialize>(&self, line: &T) {
        if let Ok(s) = serde_json::to_string(line) {
            let _ = writeln!(self.index.borrow_mut(), "{s}");
        }
    }

    fn record(&self, step: &str, argv: &[String], mutates: bool, exec: &Exec, ms: u128) {
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        let slug: String = step
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let out = format!("raw/{seq:04}-{slug}.out");
        let err = format!("raw/{seq:04}-{slug}.err");
        let _ = std::fs::write(self.dir.join(&out), &exec.stdout);
        let _ = std::fs::write(self.dir.join(&err), &exec.stderr);
        self.note(&Invocation {
            seq,
            at: now(),
            step: step.to_string(),
            argv: argv.to_vec(),
            mutates,
            status: exec.status,
            duration_ms: ms,
            stdout_file: out,
            stderr_file: err,
        });
    }
}

/// RFC3339 UTC, millisecond precision.
#[must_use]
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A runner bound to an evidence trail and an optional kubeconfig. The only
/// way a visita reaches a process.
pub struct Operator<'a> {
    runner: &'a dyn Runner,
    evidence: &'a Evidence,
    kubeconfig: Option<PathBuf>,
    client_validation: bool,
}

impl<'a> Operator<'a> {
    #[must_use]
    pub fn new(
        runner: &'a dyn Runner,
        evidence: &'a Evidence,
        kubeconfig: Option<PathBuf>,
    ) -> Self {
        Self {
            runner,
            evidence,
            kubeconfig,
            client_validation: true,
        }
    }

    #[must_use]
    pub fn with_client_validation(mut self, on: bool) -> Self {
        self.client_validation = on;
        self
    }

    #[must_use]
    pub fn evidence(&self) -> &Evidence {
        self.evidence
    }

    /// Run one verb, record it, return what it produced.
    ///
    /// A JSON read's (or a create's) Secret values are redacted HERE, before the evidence
    /// file is written and before the caller sees the bytes — so a Secret
    /// value has no path into the bundle. Tier: truly-unrepresentable for any
    /// JSON read; a non-JSON read of a Secret (none exists in [`Verb`]) would
    /// not be covered.
    pub fn run(&self, step: &str, verb: &Verb) -> Exec {
        let mut argv = verb.argv(self.kubeconfig.as_deref());
        if !self.client_validation && matches!(verb, Verb::Create { .. }) {
            argv.push("--validate=false".into());
        }
        let t = Instant::now();
        let mut exec = self.runner.run(&argv);
        if matches!(
            verb,
            Verb::Get { .. } | Verb::GetNamed { .. } | Verb::Create { .. }
        ) {
            if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&exec.stdout) {
                super::snapshot::redact_secrets(&mut v);
                exec.stdout = v.to_string();
            }
        }
        self.evidence
            .record(step, &argv, verb.mutates(), &exec, t.elapsed().as_millis());
        exec
    }

    /// Run a read that must return JSON.
    ///
    /// # Errors
    ///
    /// A non-zero exit or unparseable output, with stderr quoted — never an
    /// empty value, because "could not look" is not "nothing there".
    pub fn json(&self, step: &str, verb: &Verb) -> Result<serde_json::Value, String> {
        let exec = self.run(step, verb);
        if !exec.ok() {
            return Err(format!(
                "`{}` exited {}: {}",
                verb.argv(None).join(" "),
                exec.status,
                exec.stderr.trim()
            ));
        }
        serde_json::from_str(&exec.stdout).map_err(|e| format!("{step}: output is not JSON: {e}"))
    }
}
