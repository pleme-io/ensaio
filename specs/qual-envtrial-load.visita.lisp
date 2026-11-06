;; qual-envtrial-load — negative control for the in-place resize.
;;
;; `load-target` (specs/fixtures/qual-envtrial.yaml) never starts, so the
;; kubelet never reports resources in status. The resize must be refused as
;; "not in effect", the run must end Inconclusive (it aborted), and the pod's
;; declared resources must be put back from the snapshot (restore diff empty).

(defvisita
  :name "qual-envtrial-load"
  :claim "A resize the kubelet never applies is not called done; the run aborts, and the pod's spec is put back."
  :natural-state ((:context "lab-dev" :namespace "qual-envtrial"
                   :kinds ("pods") :names ("load-target")))
  :fixtures ((:name "starve"
              :effect mutate
              :cleanup restore-from-snapshot
              :action (:resize-pod (:context "lab-dev" :namespace "qual-envtrial"
                                    :pod "load-target" :container "broker" :cpu-limit "250m" :timeout "20s"))))
  :steps ((:place "starve"))
  :settle "30s")
