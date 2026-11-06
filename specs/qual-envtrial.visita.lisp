;; qual-envtrial — the local qualification of `ensaio visita`.
;;
;; A stand-in for a borrowed staging namespace (seeded by
;; specs/fixtures/qual-envtrial.yaml): pin a cluster Secret, change a rollout
;; strategy + image, an HPA's scale-up behaviour and a ConfigMap, observe, undo,
;; and prove the namespace is back. Every undo is DERIVED from the snapshot.
;;
;;   ensaio visita run specs/qual-envtrial.visita.lisp --allow-context <ctx>
;;
;; The context is named here as `lab-dev`, the local dev cluster.
;; Its negative control (one undo missing ⇒ a non-empty diff) is
;; specs/qual-envtrial-missing-inverse.visita.lisp.

(defvisita
  :name "qual-envtrial"
  :claim "Every change the trial makes to qual-envtrial is undone, and a diff against the snapshot taken before the first write comes back empty."
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
             (:name "strategy"
              :effect mutate
              :cleanup restore-from-snapshot
              :action (:patch (:context "lab-dev" :namespace "qual-envtrial"
                               :resource "deployment" :name "trial-app"
                               :patch "{\"spec\":{\"strategy\":{\"rollingUpdate\":{\"maxSurge\":1,\"maxUnavailable\":0}},\"template\":{\"spec\":{\"containers\":[{\"name\":\"app\",\"image\":\"registry.invalid/trial-app:branch\",\"env\":[{\"name\":\"POOL_SIZE\",\"value\":\"8\"}]}]}}}}")))
             (:name "hpa-behaviour"
              :effect mutate
              :cleanup restore-from-snapshot
              :action (:patch (:context "lab-dev" :namespace "qual-envtrial"
                               :resource "hpa" :name "trial-app-hpa"
                               :patch "{\"spec\":{\"behavior\":{\"scaleUp\":{\"policies\":[{\"type\":\"Pods\",\"value\":5,\"periodSeconds\":60}]}}}}")))
             (:name "conf"
              :effect mutate
              :cleanup restore-from-snapshot
              :action (:patch (:context "lab-dev" :namespace "qual-envtrial"
                               :resource "configmap" :name "trial-app-conf"
                               :patch "{\"data\":{\"pool_size\":\"8\",\"feature_flag\":\"on\"}}"))))
  :assertions ((:id "hpa-scale-up-pods-5-per-60s"
                :kind HpaBehavior
                :target "trial-app-hpa"
                :context "lab-dev"
                :target-namespace "qual-envtrial"
                :names ("trial-app-hpa")
                :scale-up (:kind "Pods" :value 5 :period-seconds 60)))
  :steps ((:place "pin")
          (:place "strategy")
          (:place "hpa-behaviour")
          (:place "conf")
          (:observe "hpa-scale-up-pods-5-per-60s"))
  :settle "30s")
