;; qual-envtrial-missing-inverse — the NEGATIVE CONTROL for qual-envtrial.
;;
;; Identical changes, except `conf` is declared transient: nothing puts the
;; ConfigMap back. The restore diff MUST come back non-empty (trial-app-conf's
;; `data.pool_size` and `data.feature_flag`) and the run MUST NOT certify. If this
;; ever certifies, the diff is blind and qual-envtrial's pass means nothing.
;;
;; It leaves trial-app-conf changed; the test harness deletes the namespace's
;; objects afterwards.

(defvisita
  :name "qual-envtrial-missing-inverse"
  :claim "A trial that omits one inverse is caught: the restore diff names the object left changed."
  :natural-state ((:context "lab-dev" :namespace "qual-envtrial"
                   :kinds ("deployments" "horizontalpodautoscalers" "configmaps")
                   :names ("trial-app" "trial-app-hpa" "trial-app-conf"))
                  (:context "lab-dev" :namespace "qual-envtrial"
                   :kinds ("secrets")
                   :selector "project=trial-staging"))
  :fixtures ((:name "pin"
              :effect mutate
              :cleanup restore-from-snapshot
              :action (:annotate (:context "lab-dev" :namespace "qual-envtrial"
                                  :resource "secret" :name "trial-cluster"
                                  :key "branch-pin" :value "trial-branch")))
             (:name "conf"
              :effect mutate
              :cleanup (:transient (:reason "negative control: deliberately not undone"))
              :action (:patch (:context "lab-dev" :namespace "qual-envtrial"
                               :resource "configmap" :name "trial-app-conf"
                               :patch "{\"data\":{\"pool_size\":\"8\",\"feature_flag\":\"on\"}}"))))
  :steps ((:place "pin")
          (:place "conf")))
