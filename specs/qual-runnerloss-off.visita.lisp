(defvisita
  :name "qual-runnerloss-off"
  :target "ci"
  :claim "With the retry switch off, a lost runner is classified runner-lost, nothing is retried, and the verdict is red with the reason named, exactly as the check behaves without the retry."
  :natural-state ((:namespace "argocd" :kinds ("applications.argoproj.io") :names ("*runner-scale-set*")
                   :keep-status ("sync.status" "health.status"))
                  (:namespace "ci-spot" :kinds ("autoscalingrunnersets.actions.github.com") :names ("*"))
                  (:namespace "default" :kinds ("namespaces") :selector "ensaio.pleme.io/run"))
  :branch-contains ((:repo "example-org/example-repo" :branch "feature-branch" :trunk "main"))
  :fixtures ((:name "ci" :effect mutate :cleanup stop
              :action (:dispatch-workflow (:repo "example-org/example-repo" :workflow "pipeline.yml"
                                           :git-ref "feature-branch"
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
                       (:job "suite (retry)" :conclusion "skipped")
                       (:job "suite (on-demand escalation)" :conclusion "skipped")
                       (:job "suite" :conclusion "failure"))))
  :steps ((:phase "dispatch") (:place "ci")
          (:phase "lose the spot runner") (:place "lose-spot")
          (:phase "observe the chain") (:observe "chain"))
  :settle "120s")
