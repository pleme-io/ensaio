(defvisita
  :name "qual-carve-reap"
  :target "lab"
  :claim "A run whose process dies is finished by the reaper from its persisted plan."
  :natural-state ((:namespace "default" :kinds ("namespaces") :names ("default"))
                  (:namespace "default" :kinds ("namespaces") :selector "ensaio.pleme.io/run"))
  :fixtures ((:name "space" :effect create :cleanup own-inverse
              :action (:carve-namespace (:name "qual-reap"))))
  :steps ((:place "space") (:wait "10m")))
