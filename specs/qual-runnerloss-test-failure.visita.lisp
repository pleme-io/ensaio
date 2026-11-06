(defvisita
  :name "qual-runnerloss-test-failure"
  :target "ci"
  :claim "An attempt that fails its own tests is classified test-failed, is never retried or escalated, and the verdict stays red; no runner is touched."
  :natural-state ((:namespace "argocd" :kinds ("applications.argoproj.io") :names ("*runner-scale-set*")
                   :keep-status ("sync.status" "health.status"))
                  (:namespace "ci-spot" :kinds ("autoscalingrunnersets.actions.github.com") :names ("*"))
                  (:namespace "default" :kinds ("namespaces") :selector "ensaio.pleme.io/run"))
  :branch-contains ((:repo "example-org/example-repo" :branch "broken-branch" :trunk "feature-branch"))
  :fixtures ((:name "ci" :effect mutate :cleanup stop
              :action (:dispatch-workflow (:repo "example-org/example-repo" :workflow "pipeline.yml"
                                           :git-ref "broken-branch"
                                           :inputs ((:name "runner_loss_retry" :value "on")
                                                    (:name "runner_loss_escalate" :value "on"))
                                           :await (:job "suite (spot)" :step "Check out code")
                                           :accept-failure ("suite")
                                           :poll "30s" :timeout "60m"))))
  :assertions ((:id "chain" :kind WorkflowJobs :target "pipeline" :run-of "ci" :timeout "150m" :interval "60s"
                :jobs ((:job "suite (spot)" :conclusion "failure")
                       (:job "suite (triage)" :conclusion "success")
                       (:job "suite (retry)" :conclusion "skipped")
                       (:job "suite (on-demand escalation)" :conclusion "skipped")
                       (:job "suite" :conclusion "failure"))))
  :steps ((:phase "dispatch the branch with one test broken") (:place "ci")
          (:phase "observe the chain") (:observe "chain"))
  :settle "60s")
