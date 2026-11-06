(defvisita
  :name "qual-carve-node"
  :target "lab"
  :claim "A carved node carries exactly one NoSchedule taint and one label while the trial runs, and nothing else about it changes."
  :natural-state ((:namespace "default" :kinds ("nodes") :selector "kubernetes.io/hostname=lab"))
  :fixtures ((:name "node" :effect mutate :cleanup own-inverse
              :action (:carve-node (:selector "kubernetes.io/hostname=lab"
                                    :taint-key "ensaio.pleme.io/carved" :taint-value "qual"
                                    :label-key "ensaio.pleme.io/carved" :label-value "qual"))))
  :steps ((:place "node") (:wait "5s"))
  :settle "60s")
