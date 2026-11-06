(defvisita
  :name "qual-runnerloss-recovers"
  :target "ci"
  :claim "An attempt whose runner is lost is classified runner-lost, retried once, and the run ends green with the lost runner named; nothing the trial touched outlives it."
  :natural-state ((:namespace "argocd" :kinds ("applications.argoproj.io") :names ("*runner-scale-set*")
                   :keep-status ("sync.status" "health.status"))
                  (:namespace "ci-spot" :kinds ("autoscalingrunnersets.actions.github.com") :names ("*"))
                  (:namespace "argocd" :kinds ("secrets") :selector "argocd.argoproj.io/secret-type=cluster")
                  (:namespace "default" :kinds ("namespaces") :selector "ensaio.pleme.io/run"))
  :branch-contains ((:repo "example-org/example-repo" :branch "feature-branch" :trunk "main"))
  :fixtures ((:name "ci" :effect mutate :cleanup stop
              :action (:dispatch-workflow (:repo "example-org/example-repo" :workflow "pipeline.yml"
                                           :git-ref "feature-branch"
                                           :inputs ((:name "runner_loss_retry" :value "on"))
                                           :await (:job "suite (spot)" :step "Check out code")
                                           :accept-failure ("suite")
                                           :poll "30s" :timeout "60m")))
             (:name "lose-spot" :effect mutate
              :cleanup (:transient (:reason "an ephemeral runner pod; its controller replaces it"))
              :action (:delete-runner-pod (:namespace "ci-spot" :run-of "ci"
                                           :job "suite (spot)" :wait "10m"))))
  :assertions ((:id "chain" :kind WorkflowJobs :target "pipeline" :run-of "ci" :timeout "150m" :interval "60s"
                :jobs ((:job "suite (spot)" :conclusion "failure"
                        :log-contains "The runner has received a shutdown signal")
                       (:job "suite (triage)" :conclusion "success")
                       (:job "suite (retry)" :conclusion "success")
                       (:job "suite (retry triage)" :conclusion "success")
                       (:job "suite (on-demand escalation)" :conclusion "skipped")
                       (:job "suite" :conclusion "success"))))
  :steps ((:phase "dispatch") (:place "ci")
          (:phase "lose the spot runner") (:place "lose-spot")
          (:phase "observe the chain") (:observe "chain"))
  :settle "120s")
