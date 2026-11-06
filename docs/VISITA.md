# visita — guest trials in an environment we do not own

`ensaio visita` runs a trial on borrowed ground (a shared staging tenant):
capture its natural state, change it only through declared reversible steps,
observe it through its own API, always put it back, and prove it is back with a
diff. Every read and write goes through a closed verb set
(`visita/command.rs`), from the operator's machine, against contexts the spec
names and the CLI allows. There is no `apply`; the one thing that creates is an
`:apply-manifest` fixture, which refuses if any of its objects already exists
and deletes exactly the objects it created.

```
ensaio visita validate|snapshot|diff|run <spec.visita.lisp> --allow-context <ctx>…
ensaio visita judge-rollout <samples.jsonl> [--baseline name=gen] [--expect-image p]
ensaio visita plan-delta <trunk-plan.json> <branch-plan.json> --expect 'address#/json/pointer'…
ensaio visita validate|snapshot|diff|run <spec> --target <name>… [--config f.yaml]… [--operator id] [--lease-ttl 2h] [--lease-namespace ns] [--lease-enabled false]
ensaio visita reap --target <name> | --context <ctx> [--run <id>] [--config f.yaml]…
```

## The loop, and which key carries each move

| move | what it means on borrowed ground | carried by |
|---|---|---|
| detect | read what the environment holds now, including other people's pins | `:natural-state` (`snapshot`) |
| absorb | take what is there as the baseline; never write over it | `:absorb #t` on an `annotate`/`patch` fixture; `plan-delta` for a Terraform leaf |
| occupy | add only keys nobody holds | fixtures with `:absorb #t` |
| augment | the change under trial | the remaining fixtures |
| control | stop and undo when the environment objects | `:guard`, `:branch-contains` |
| test | observe through the environment's own API | `:assertions`, `:steps` |
| restore | back to what detect read, proven by a diff | derived inverses, `:restore`, `restore-diff.json` |

## The authored form: `(defvisita …)`

| key | what it is |
|---|---|
| `:natural-state` | selections (context, namespace, kinds, name globs, selector, kept status paths) captured before the first write and diffed after the last undo |
| `:fixtures` | changes, each with an `:action` and a `:cleanup` |
| `:assertions` | operator-side observations |
| `:steps` | `(:place f)` `(:observe a)` `(:wait d)` `(:undo f)` `(:phase name)`, in order |
| `:restore` | fixtures placed after every undo, before the diff (a sync back to the natural revision) |
| fixture `:undo-phase after-restore` | undo this fixture only after the `:restore` fixtures ran and the restore check settled — for what the restore still needs (a proxy the pinned clients point at until the sync points them back). That check leaves the fixture's created objects out (`restore-diff-held.json`); the final diff, after its undo, must find them gone. Last on a guard trip too. Refused where the cleanup has nothing to defer (`stop`, `transient`, `retained`) |
| `:guard` | pods that must stay Ready, not restart, and stay `:liveness-margin` (default 2) short of their liveness probe's `failureThreshold` — consecutive failures counted from `Unhealthy` events since the guard began, threshold and period read from the pod's own probe; a trip undoes at once — every fixture on the pod that tripped it first, then every other resize, then the rest in reverse placement order |
| `:preconditions` | assertion ids that gate the receipt (an A/B's "side A reproduced") |
| `:branch-contains` | `(:repo owner/name :branch b :trunk t)` — every branch the trial pins or dispatches must contain its trunk (GitHub compare, `behind_by == 0`); read at start (refusing before anything is written) and again before each placement naming the branch |
| `:allow`, `:settle` | diff allow-list additions; how long the restore may take to converge |

## Actions and their only valid cleanups

| action | cleanup | inverse |
|---|---|---|
| `annotate` | `restore-from-snapshot` | the captured value, or removal |
| `patch` (JSON merge) | `restore-from-snapshot` | every touched key back to its captured value |
| `resize-pod` (in place, `pods/{name}/resize`) | `restore-from-snapshot` (required) | the pod's captured resources; refused if the cpu `resizePolicy` would restart the container; not placed until `status.containerStatuses` converges |
| `rollout-restart` | `transient` (required) | none; `restartedAt` is allow-listed and the restore sync covers it |
| `dispatch-workflow` | `stop` (required) | cancel the run if still in flight — every run is stopped before any other undo |
| `delete-runner-pod` (`:effect mutate`) | `transient` (required) | none; the pod's controller replaces it. Deletes only the pod that runs one job of a run this trial dispatched |
| `argo-sync` | `transient` | none (it is usually the restore) |
| `rabbitmq-close-connections` | `transient` | none; zero closed is refused |
| `carve-namespace` (`:effect create`) | `own-inverse` (required) | delete the namespace (and its quota and limit range) uid-matched, then wait (default `300s`) until it has terminated; refused before any write if the namespace exists |
| `fence` (`:effect create`) | `own-inverse` (required) | delete the NetworkPolicies it created, uid-matched |
| `carve-node` (`:effect mutate`) | `own-inverse` (required) | remove exactly the taint and label it added, from the node it chose (uid-matched); never cordons or drains |
| `argocd-pause` (`:effect mutate`) | `own-inverse` (required) | put back the snapshotted `spec.syncPolicy` exactly (keys added since are removed), or the snapshotted `skip-reconcile` annotation |
| `apply-manifest` (`kubectl create`, `:effect create`) | `own-inverse` (required) | delete exactly the objects it created, each only while it carries the uid recorded at creation, then wait (bounded by `:timeout`, default `120s`) until they are gone; refused before any write if ANY object in the manifest already exists |
| `exec` (a mutating `kubectl exec`) | `own-inverse` with `:undo-argv`, else `transient` (required — an exec with neither is refused at load) | `:undo-argv` run in every pod the argv was attempted in; a non-zero exit fails the placement, which aborts the trial and undoes it |

### `delete-runner-pod`

```lisp
(:name "lose-runner" :effect mutate
 :cleanup (:transient (:reason "an ephemeral runner pod; its controller replaces it"))
 :action (:delete-runner-pod (:namespace "ci-spot" :run-of "ci" :job "suite (spot)" :wait "10m")))
```

Loses a runner on purpose, the way a spot interruption does, to see what a
workflow does with a lost runner. `:run-of` names a `dispatch-workflow` fixture
placed earlier. The pod is found from that run, never from a selector: the
run's job `:job` must be `in_progress` on a `runner_name`, and that runner must
be a pod in `:namespace`. Anything else is refused with nothing written, and the
refusal names the job, its state and its runner. `:wait` polls (every 10s,
bounded) until the job is running, for a job that starts only after another
ends, such as a retry. The pod's name, uid, node and the run and job ids are a
`steps.jsonl` line. The cleanup is `transient`: a runner pod is ephemeral and
its controller replaces it, so there is nothing to put back.

### `apply-manifest`

```lisp
(:name "proxy" :effect create :cleanup own-inverse
 :action (:apply-manifest (:context "stage-ctx" :namespace "stage"
                           :file "manifests/proxy.yaml"      ; relative to the spec file
                           :await-available ("proxy")       ; Deployments that must be Available
                           :timeout "2m")))                 ; bounds that wait and the delete
```

`:file` or `:yaml "…"` (inline, multi-document), never both. Every object needs
`apiVersion`, `kind` and `metadata.name` (`generateName` could not be found again
to delete), and a `metadata.namespace`, if set, must be the fixture's. The
created identities (kind, name, uid) are a `created` line in `steps.jsonl`; the
manifest as created, Secret values hashed, is `manifest-place-<name>.json`.
Add `:undo-phase after-restore` to the fixture (beside `:cleanup`) to keep the
objects until the restore has converged.

### `exec`

```lisp
(:name "squeeze" :effect mutate :cleanup own-inverse
 :action (:exec (:context "stage-ctx" :namespace "rabbitmq"
                 :selector "app=rabbitmq" :all #t        ; or :pod "rabbitmq-0"
                 :container "rabbitmq"
                 :argv ("rabbitmqctl" "set_vm_memory_high_watermark" "0.2")
                 :undo-argv ("rabbitmqctl" "set_vm_memory_high_watermark" "0.4"))))
```

`:pod` or `:selector`, never both; a selector takes the first Ready,
not-terminating pod by name, or every one with `:all #t`, and selecting none
fails the placement. `:container` is required (kubectl's default-container
pick is not a declaration). Without `:undo-argv` the fixture must say
`:cleanup (:transient (:reason "…"))`. A guard trip on a pod the exec ran in
undoes it first.

### `:absorb`

```lisp
(:name "occupy" :effect mutate :cleanup restore-from-snapshot
 :action (:annotate (:context "cicd" :namespace "argocd" :resource "secret" :name "stage-cluster"
                     :key "my-chart" :value "my-branch" :absorb #t)))
```

An absorbing `annotate` or `patch` reads the live object just before placing
and writes only keys it does not hold, or holds at the same value. A key held at
another value (another team's pin, even one set after the snapshot) refuses the
placement before anything is written, naming the key as a JSON pointer and the
live value; the fixture is not counted as placed, so nothing of it is undone,
and the steps stop and the run undoes what came before. A live read that fails
also refuses (the check cannot see what is held); an object that does not exist
holds nothing. Without `:absorb` a fixture may change a held value, which is
what a fault trial does.

### `plan-delta`

Terraform cannot be told to leave a live label alone, so a leaf's plan carries
every pin someone set by hand. `plan-delta` compares the branch's plan with the
trunk's plan for the same leaf, both from `terraform show -json` against the
same live state. Each changed leaf is keyed by address and JSON pointer
(`/metadata/0/labels/app.kubernetes.io~1name`), and each resource's action list
is one more entry (`@actions`), so a replace never reads as an update. A change
in both plans, identically, is absorbed (pending on trunk, not the branch's); a
change only in the branch must match an `--expect` (`address#/pointer`, matched
whole segment by segment, `*` for any address; an `update` the branch alone
introduces is covered by its expected paths); a trunk change the branch drops is
`kept` when an `--expect` covers it (the branch keeps a live value trunk would revert,
which is absorbing) and `vanished` otherwise. Held = nothing unexpected and nothing vanished. Refused: input that
is not a plan (no `format_version` or `resource_changes`, i.e. a state dump), an
errored plan, and two plans whose `prior_state` resources differ (outputs are left out: they echo each branch's own config). Sensitive values print
as `(sensitive)`. Arrays of different lengths compare whole.

## Carving an isolated space in any cluster

### The target registry (`visita` config, shikumi)

One `TieredConfig` root, `VisitaConfig`, folded by shikumi (app name `visita`):
`prescribed_default` → the discovered file (`$VISITA_CONFIG`, else
`$XDG_CONFIG_HOME/visita/visita.yaml` or `~/.config/visita/visita.yaml`) →
each `--config FILE` in order → `VISITA_*` env (`__` nests) → the typed flags
(`--operator`, `--lease-ttl`, `--lease-namespace`, `--lease-enabled`) →
`--set path=value`. Maps merge per key (targets from two files add up), lists
replace whole, an unknown key is refused naming the layer that wrote it.
`ensaio` ships only the mechanism; the facts (which clusters, which tiers)
come from the private layer that renders the file.

```yaml
targets:
  stage-us:                      # the name a spec's :target and --target use
    context: stage-us-ctx        # kube context
    tier: staging                # production | staging | development
    namespace: visita            # optional: where the run's lease lives
    carve:
      allow: [carve-namespace, fence, apply-manifest, annotate, patch]
      requiresWindow: false
      windowTag: stage-us        # optional; default: the target name
lease:
  enabled: true                  # prescribed default
  ttl: 2h                        # prescribed default
  namespace: visita              # optional; overrides every target's
windows:
  dir: ~/.local/state/guardrail/windows.d   # default $XDG_STATE_HOME/guardrail/windows.d
kubectl:
  clientValidation: true         # false adds --validate=false to creates (an apiserver without OpenAPI, e.g. engenho)
operator: alice                  # default $USER
```

A spec names `:target "stage-us"` and may then leave every `:context` out; the
target's context fills them, and the operator must pass `--target stage-us`
(a context the operator did not name is still never acted on). A raw-context
spec keeps working with `--allow-context`; if the raw context is a registry
target's, it inherits that target's guard and lease.

### Tier guard (load-rejected)

Every fixture (and its declared inverse) whose context is a registry target's is
checked when the spec loads, before anything is read:

- its class (the action's kebab name: `annotate`, `patch`, `resize-pod`,
  `rollout-restart`, `dispatch-workflow`, `argo-sync`,
  `rabbitmq-close-connections`, `apply-manifest`, `exec`, `carve-namespace`,
  `fence`, `carve-node`, `argocd-pause`) must be in `carve.allow`;
- on a `requiresWindow` target, a guardrail change window tagged `windowTag`
  must be open now. Windows are read from guardrail's own files
  (`windows.d/*.json`, `{"changeWindows":[{"name","tag","start","end"}]}`, UTC
  `Z` times) through the `guardrail` crate, so the format has one owner.

A refusal names the fixture, target, tier and class. A `production` target that
does not require a window is refused as a config error.

### Lease (shared, visible, TTL)

For a run with a registry target and `lease.enabled`, after the natural state
is captured and before the first write, visita lists the
`coordination.k8s.io/v1` Leases labelled `ensaio.pleme.io/lease=visita` in the
lease namespace and refuses if an unexpired one (other than its own) covers any
of the same scope (`ns/<namespace>` for every selection and fixture,
`node/<selector>` for a carve-node). It then CREATES (never applies)
`visita-<run>`: `holderIdentity: <operator>/<run>`, `leaseDurationSeconds`,
annotations `ensaio.pleme.io/expires`, `/scope`, `/spec`, `/fixtures`. It is
renewed while the run waits and polls (every ttl/3) and deleted, uid- and
holder-matched, after the last undo. `kubectl get leases -A -l
ensaio.pleme.io/lease=visita` shows who holds what, until when.

The run's plan (spec, placed fixtures, and the ledger of what each one created,
carved or snapshotted) is written to `$XDG_STATE_HOME/ensaio/visita/<run>/plan.json`
after every step, beside `natural-state.json`; that directory is also the
evidence directory unless `--evidence` is given.

### Reaper

`ensaio visita reap --target <name>` (or `--context <ctx>`) lists the visita
leases in the lease namespace, picks the expired ones held by this operator (or
exactly `--run <id>`, expired or not), and for each: replays the persisted undo
plan (in-order undos in reverse, then after-restore ones), places the spec's
`:restore` fixtures, re-captures and diffs against the run's natural state,
releases the lease, and writes `reap-<ts>/restore-diff.json` and `reap.json`
(the `natural-state-restored` verdict). A lease held by another operator is
never touched (`--run` on one is refused). A run with no plan or natural state
on this machine is reported and its lease left in place. Exit 0 = every reaped
run restored.

### The carve fixtures

```lisp
(:name "space" :effect create :cleanup own-inverse
 :action (:carve-namespace (:name "trial-1"
          :quota ((:name "pods" :value "10") (:name "requests.cpu" :value "2"))
          :limit-default ((:name "cpu" :value "500m"))
          :limit-default-request ((:name "cpu" :value "100m"))
          :timeout "5m")))
(:name "fence" :effect create :cleanup own-inverse :after ("space")
 :action (:fence (:namespace "trial-1"
          :egress (dns api-server (:service (:namespace "db" :name "pg")) (:namespace "monitoring"))
          :ingress ((:namespace "trial-1")))))
(:name "node" :effect mutate :cleanup own-inverse
 :action (:carve-node (:selector "pool=trial" :taint-key "ensaio.pleme.io/carved"
          :taint-value "trial-1" :label-key "ensaio.pleme.io/carved" :label-value "trial-1")))
(:name "pause" :effect mutate :cleanup own-inverse
 :action (:argocd-pause (:namespace "argocd" :application "my-app")))
```

- **carve-namespace** creates the Namespace (labelled `ensaio.pleme.io/run=<run>`),
  a `ResourceQuota` and a `LimitRange` named `ensaio-carve`, through the
  apply-manifest machinery: refused if the namespace exists, identities and
  uids recorded, deleted uid-matched and awaited until terminated. Prove it with
  a selection `(:kinds ("namespaces") :selector "ensaio.pleme.io/run")`: a
  namespace left behind is an `extra`.
- **fence** creates `ensaio-fence-deny` (all pods, Ingress+Egress, no rules)
  plus `ensaio-fence-egress`/`-ingress` admitting exactly the peers: `dns`
  (kube-system `k8s-app=kube-dns`, 53 UDP/TCP), `api-server` (egress only: the
  `default/kubernetes` Endpoints addresses as /32 ipBlocks with their ports;
  refused before any write when the cluster publishes none, as engenho does),
  `(:service (:namespace … :name …))` (its pod selector and target ports;
  refused if it has no selector), `(:namespace "n")`.
- **carve-node** picks the first schedulable node by name the selector returns,
  refuses if it already carries the taint key or the label key, and adds one
  `NoSchedule` taint and one label (there is no effect field, so `NoExecute`
  cannot be written); the undo removes exactly those and keeps every other
  taint. No cordon, no drain.
- **argocd-pause** reads the Application. Not owned by an ApplicationSet: it
  snapshots `spec.syncPolicy` and removes `automated`; the undo writes the
  snapshot back exactly. Owned by an ApplicationSet: removing `automated` would
  not hold, because the ApplicationSet controller regenerates the
  Application's spec from its template on its next reconcile and the pause
  silently lapses; so it sets `argocd.argoproj.io/skip-reconcile: "true"`,
  which stops the application controller reconciling the app at all. The
  ApplicationSet controller also rewrites annotations, keeping only the ones it
  is told to preserve, so the fixture refuses before any write unless the set
  lists the annotation in `spec.preservedFields.annotations` (or ignores
  `/metadata/annotations` through `ignoreApplicationDifferences`). The undo puts
  back the annotation's snapshotted value (removing it when it had none).

### Qualifications on the local lab cluster (2026-10-14)

| spec | verdict | what it proved |
|---|---|---|
| `qual-carve-namespace` | Certified | namespace + quota + limit range created, deleted, no run-labelled namespace left; lease acquired and released |
| `qual-carve-fence` | Certified | deny + egress (dns, namespace) + ingress policies on a carved namespace, removed in reverse; `api-server` refused before any write (engenho publishes no `kubernetes` Endpoints) |
| `qual-carve-node` | Certified | node `lab` tainted and labelled, both removed, node diff empty |
| `qual-carve-argocd-pause` | Certified | `automated` removed and the exact `syncPolicy` restored (Application CRD + one app seeded by `specs/fixtures/qual-argocd*.yaml`, no controller) |
| `qual-carve-guard-refused` | refused at validate (negative control) | production target: `carve-node` not allowed; `carve-namespace` outside a window |
| `qual-carve-reap` | reap: held | run with `--lease-ttl 30s` killed (`kill -9`) mid-wait with the namespace carved; `reap` before expiry found nothing, `--run` as another operator was refused, after expiry it deleted the namespace and the lease and judged the diff held |

## Observations (operator-side kinds)

`RolloutSurgeBound` (recorded from `:record-from`, judged per generation on the
expected image; a rollout never seen mid-transition is Skipped, not held) ·
`LogWindow` (per pod; `:min-count` for `present`; vanished pods skipped) ·
`MetricInRange` (pod proxy `/metrics`) · `HpaBehavior` · `ExecMetric` (a
declared read-only command's number, recorded from a fixture, judged by
`:stat all|max|min`) · `WorkflowJobs` (the named jobs of a dispatched run: conclusion, log and
annotation texts) · `ObjectFields` (one object of any group, version and
kind, namespaced with `:target-namespace` or cluster-scoped without it, polled
every `:interval` until its `:wait-until` predicates hold or `:timeout` passes,
then judged by its `:expect` predicates; see below). Computed, never authored: `NaturalStateRestored`,
`StepsCompleted` and `GuardHeld` (the last two are gating preconditions, so an
aborted or guard-stopped run is Inconclusive even with a clean restore). A trip
is `GuardHeld` **Falsified**, never Skipped: its finding names the pod, the
signal (not Ready for n s, restarted, or n/threshold liveness failures) and the
time, and `report.json` carries it typed as `guard_trip`.

### WorkflowJobs

```lisp
(:id "chain" :kind WorkflowJobs :target "pipeline" :run-of "ci" :timeout "150m" :interval "60s"
 :jobs ((:job "suite (spot)" :conclusion "failure" :log-contains "shutdown signal")
        (:job "suite (retry)" :conclusion "skipped")
        (:job "suite" :conclusion "failure")))
```

Judges the jobs of a run a `dispatch-workflow` fixture started (`:run-of`),
read through the GitHub API. It needs no kube context. Each `:jobs` entry names
one job (exact name) and at least one of `:conclusion`, `:log-contains` and
`:annotation-contains` (a text or a list of texts). The run is polled every
`:interval` until every named job is `completed`, bounded by `:timeout`
(default `60m`, interval `30s`). Then each entry is judged: the conclusion, and
the texts in the job's log or its check annotations.

| how the watch ended | verdict |
|---|---|
| every named job completed | Held, or Falsified naming each job whose conclusion or texts differ |
| timeout with a named job still running or absent | Falsified: `still in_progress after 150m` or `never appeared` |
| Unauthorized, Forbidden or NotFound at once, or a log that cannot be read | Skipped, `unobserved (blind)` |

A job that never ran is absent, not skipped: a job skipped by its `if:` has the
conclusion `skipped`, and is judged as one. The observation of every read is
`jobs-<id>.jsonl`; the final state is `jobs-<id>.json`.

### ObjectFields

```lisp
(:id "drill" :kind ObjectFields :target "drill" :context "stage-ctx"
 :api-version "drills.example.io/v1" :resource-kind "DrillSession" :name "drill-a"
 :wait-until ((:path "status.terminal" :equals true) (:path "status.runId" :equals "run-2"))
 :timeout "25m" :interval "10s"
 :expect ((:path "status.pass" :equals true)
          (:path "status.rtoSeconds" :max 60)
          (:path "status.verdict" :one-of ("confirmed"))
          (:path "status.reads[0].failures" :equals 0)))
```

A path is dotted, with `[n]` indices (a numeric dot segment also indexes an
array). Each predicate is exactly one test: `:equals` (string, number or
boolean, compared with its JSON type), `:one-of`, `:min`/`:max` (inclusive,
either or both), `:present true`, `:absent true`. A bare `true`/`false` is a
boolean, because the lisp reader hands a symbol over as a string; `"60"` stays
a string. Paths and predicates are refused when the spec compiles;
`:api-version`, `:resource-kind`, `:name`, `:expect` and `:context` are
required at validation. `:wait-until` takes one predicate or a list; without
it the first answer is judged. Defaults: `:timeout 5m`, `:interval 10s`.

| how the watch ended | verdict |
|---|---|
| wait-until held | Held, or Falsified naming each failing predicate as `path: observed X, wanted Y` |
| timeout before wait-until held | Falsified: `timed out after …` plus every unmet wait-until predicate with its last observed value (or the object never found) |
| Forbidden / Unauthorized (at once), or the last read unreachable at the timeout | Skipped, `unobserved (blind)`, with the last observed values when there were any |

Every read is appended to `object-<id>.jsonl`; the final selected fields and
how the watch ended are in `object-<id>.json`, and the per-field values are the
verdict's subjects in `report.json` and `summary.md`.

## Seals and their tiers

| guarantee | tier |
|---|---|
| nothing created in the target except an `:apply-manifest` fixture's objects | only-mitigated: `Create`/`Delete` verbs exist and the environment confines them (a create only after every object reads NotFound; a delete only of a recorded identity whose uid still matches) |
| an `:apply-manifest` deleting an object it did not create | run-caught: refused before the create, and uid-checked before every delete |
| what an exec / merge patch can do once allowed | only-mitigated |
| Secret values never reach the evidence (redacted at the recording boundary) | truly-unrepresentable for JSON reads |
| a branch behind its trunk deployed (silently reverting the trunk) | run-caught, before the write |
| unallowed contexts, Job-creating kinds, missing kind parameters, uncovered or non-invertible restores, wrong cleanup for an action, `:record-from` after its observe, undo of an unplaced fixture | parse/load-rejected |
| restored = empty diff over a non-empty set | run-caught |
| a fault that did nothing, a resize that never converged, a run that will not stop, a guard trip | run-caught |
| over-broad allow-list | only-mitigated (every absorbed change is counted) |
| an `:absorb` fixture overwriting a key the environment holds | run-caught, before the write |
| a branch plan changing more than its declared occupancy | run-caught by `plan-delta` (exit 1) |
| a fixture class a target does not allow, a write on a window-bound target outside a window, a production target without a window | load-rejected |
| two runs over the same selection at once | run-caught before the first write (the lease list); only-mitigated, since list-then-create is not atomic and two creates in the same instant can both pass |
| a carve fixture without its own inverse; a NoExecute taint | load-rejected; unrepresentable (carve-node has no effect field) |
| a dead run left behind | run-caught by `reap` once its lease expires; only-mitigated until the operator runs it |

## Shared primitives and merges (recorded 2026-09-30)

- **In-place resize → `breathe-resize`.** The resize's decisions (`resizePolicy`
  restart check, QoS-preserving limit block, request block, subresource body,
  convergence) live in one pure crate in the breathe workspace, lifted out of
  `breathe-kube`. breathe's actuators (kube-rs) and visita's `resize-pod`
  (kubectl) both depend on it; neither holds a copy.
- **Owed in pleme-io/actions: `cargo-publish-each-member` cannot publish a new
  member that depends on an unchanged sibling.** `cargo set-version --bump`,
  run in one member's directory, bumps the WHOLE workspace (cargo-edit 0.13,
  reproduced), while the publisher skips unchanged members — so the new
  member's dependency requirement names a version never published, and every
  pass defers it ("dep not on registry yet"). Seen on breathe's release of
  `breathe-resize` (2026-09-30). Until the bump is `--package`-scoped, ensaio
  pins `breathe-resize` to breathe's commit by git.
- **burst-forge: not merged, not sunset — yet.** Compared 2026-09-30.
  burst-forge is an owned-cluster scale-test harness: it creates pods and Jobs,
  labels every node, scales AWS node groups and pauses the cluster-autoscaler,
  and restores to hardcoded values. visita subsumes its *lifecycle*
  (snapshot, derived inverses, restore verdict) and must never gain its writes.
  What would move here is its measurement core (`count_pod_states`, the
  milestone fold in `burst.rs`) as a generic population-timing observe. Sunset
  is owed and blocked on its consumers: the `experiment` skill (runs it), the
  Vector `burst_forge_http` source + `burst_forge_enrich`, shinryu and
  shinryu-mcp's schema and UDFs, the pangea/lava dashboards, and one private
  runbook. No flake input and no workflow depends on it. See burst-forge's
  README.
- **Owed inside ensaio: one budgeted poll.** viveiro's `cycle.rs` (come-up,
  drain, teardown budgets over an injectable `Clock`) and visita (await run,
  stop run, resize convergence, rollout watch, exec record, restore settle — six
  loops over `Instant` and real sleeps) implement the same "poll until the
  condition or the budget" shape independently. The merge is one primitive over
  viveiro's `Clock`, which would also make visita's tests stop sleeping. Not
  done tonight: it touches viveiro's tested FSM.

## Specs

`specs/qual-carve-*.visita.lisp` qualify the carve fixtures, the lease and the reaper on the local lab cluster through `specs/fixtures/qual-visita.yaml` (table above); `qual-carve-guard-refused` is the tier guard's negative control and must refuse at validate.

`specs/qual-runnerloss-*.visita.lisp` (neutral names) qualify a workflow that
retries a job whose runner was lost: the retry recovers, the escalation runs on a
scratch scale set, the switch off keeps the check red, and a real test failure is
never retried. The tests load each through the tier guard and rehearse it end to
end in the in-memory cluster; a target that does not allow `delete-runner-pod`
refuses the two that lose a runner. Their real-named twins are in the private nix
repo.

Public, neutral: `specs/qual-envtrial*.visita.lisp` (the local qualification,
its missing-inverse negative control, and the resize/guard negative controls,
seeded by `specs/fixtures/qual-envtrial.yaml`). Specs that name real tenants
live in the private nix repo (`ensaio-specs/`), never here.
