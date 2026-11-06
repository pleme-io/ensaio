(defvisita
  :name "qual-carve-guard-refused"
  :target "lab-locked"
  :claim "A production target refuses a class it does not allow and any write outside a window."
  :natural-state ((:namespace "default" :kinds ("nodes")))
  :fixtures ((:name "space" :effect create :cleanup own-inverse
              :action (:carve-namespace (:name "never")))
             (:name "node" :effect mutate :cleanup own-inverse
              :action (:carve-node (:selector "x=y" :taint-key "k" :label-key "k" :label-value "v"))))
  :steps ((:place "space") (:place "node")))
