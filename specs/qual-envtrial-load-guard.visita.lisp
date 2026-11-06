;; qual-envtrial-load-guard — negative control for the guard.
;;
;; The guard watches `load-target`, which is never Ready (its image never
;; pulls). Within `:max-unready` the guard must trip, the run must stop and
;; undo, and the receipt must be Inconclusive with the guard's reason — even
;; though the ConfigMap change it made is put back cleanly.
;;
;; (The local cluster does not serve the `resize` subresource, so the load here
;; is a ConfigMap patch; "resizes are undone first on a trip" is proven by the
;; hermetic suite.)

(defvisita
  :name "qual-envtrial-load-guard"
  :claim "A watched pod that stays unready stops the trial, the change is undone, and the run is not certified."
  :natural-state ((:context "lab-dev" :namespace "qual-envtrial"
                   :kinds ("configmaps") :names ("trial-app-conf")))
  :fixtures ((:name "conf"
              :effect mutate
              :cleanup restore-from-snapshot
              :action (:patch (:context "lab-dev" :namespace "qual-envtrial"
                               :resource "configmap" :name "trial-app-conf"
                               :patch "{\"data\":{\"pool_size\":\"8\"}}"))))
  :guard (:context "lab-dev" :namespace "qual-envtrial" :pods ("load-target")
          :max-unready "5s" :interval "1s")
  :steps ((:place "conf") (:wait "30s"))
  :settle "30s")
