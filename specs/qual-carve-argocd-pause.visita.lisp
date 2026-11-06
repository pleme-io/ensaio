(defvisita
  :name "qual-carve-argocd-pause"
  :target "lab"
  :claim "Pausing an Application removes automated sync while the trial runs, and its syncPolicy is back exactly afterwards."
  :natural-state ((:namespace "qual-argocd" :kinds ("applications.argoproj.io") :names ("qual-app")))
  :fixtures ((:name "pause" :effect mutate :cleanup own-inverse
              :action (:argocd-pause (:namespace "qual-argocd" :application "qual-app"))))
  :steps ((:place "pause") (:wait "5s"))
  :settle "60s")
