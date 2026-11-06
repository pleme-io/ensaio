(defvisita
  :name "qual-runnerloss-escalates"
  :target "ci"
  :claim "When the retry also loses its runner, the on-demand escalation runs on a scratch on-demand scale set and the run ends green; the scale set, its namespace and its registration are gone afterwards."
  :natural-state ((:namespace "argocd" :kinds ("applications.argoproj.io") :names ("*runner-scale-set*")
                   :keep-status ("sync.status" "health.status"))
                  (:namespace "ci-spot" :kinds ("autoscalingrunnersets.actions.github.com") :names ("*"))
                  (:namespace "argocd" :kinds ("secrets") :selector "argocd.argoproj.io/secret-type=cluster")
                  (:namespace "default" :kinds ("namespaces") :selector "ensaio.pleme.io/run"))
  :branch-contains ((:repo "example-org/example-repo" :branch "feature-branch" :trunk "main")
                    (:repo "example-org/example-config" :branch "pool-branch" :trunk "main"))
  :fixtures ((:name "space" :effect create :cleanup own-inverse
              :action (:carve-namespace (:name "qualify-runners" :timeout "300s")))
             (:name "pool" :effect create :cleanup own-inverse :after ("space")
              :action (:apply-manifest (:namespace "argocd" :file "fixtures/qual-runnerloss-pool.yaml" :timeout "300s")))
             (:name "ci" :effect mutate :cleanup stop :after ("pool")
              :action (:dispatch-workflow (:repo "example-org/example-repo" :workflow "pipeline.yml"
                                           :git-ref "feature-branch"
                                           :inputs ((:name "runner_loss_retry" :value "on")
                                                    (:name "runner_loss_escalate" :value "on")
                                                    (:name "runner_loss_escalation_runner" :value "qualify-od"))
                                           :await (:job "suite (spot)" :step "Check out code")
                                           :accept-failure ("suite")
                                           :poll "30s" :timeout "60m")))
             (:name "lose-spot" :effect mutate :after ("ci")
              :cleanup (:transient (:reason "an ephemeral runner pod; its controller replaces it"))
              :action (:delete-runner-pod (:namespace "ci-spot" :run-of "ci"
                                           :job "suite (spot)" :wait "10m")))
             (:name "lose-retry" :effect mutate :after ("lose-spot")
              :cleanup (:transient (:reason "an ephemeral runner pod; its controller replaces it"))
              :action (:delete-runner-pod (:namespace "ci-spot" :run-of "ci"
                                           :job "suite (retry)" :wait "40m"))))
  :assertions ((:id "chain" :kind WorkflowJobs :target "pipeline" :run-of "ci" :timeout "180m" :interval "60s"
                :jobs ((:job "suite (spot)" :conclusion "failure"
                        :log-contains "The runner has received a shutdown signal")
                       (:job "suite (retry)" :conclusion "failure"
                        :log-contains "The runner has received a shutdown signal")
                       (:job "suite (retry triage)" :conclusion "success")
                       (:job "suite (on-demand escalation)" :conclusion "success")
                       (:job "suite" :conclusion "success"))))
  :steps ((:phase "scratch pool") (:place "space") (:place "pool")
          (:phase "dispatch") (:place "ci")
          (:phase "lose the spot runner, then the retry's") (:place "lose-spot") (:place "lose-retry")
          (:phase "observe the chain") (:observe "chain"))
  :settle "300s")
