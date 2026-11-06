(defvisita
  :name "qual-carve-namespace"
  :target "lab"
  :claim "A carved namespace, its quota and its limit range exist only while the trial runs; afterwards no namespace carries a run label."
  :natural-state ((:namespace "default" :kinds ("namespaces") :names ("default"))
                  (:namespace "default" :kinds ("namespaces") :selector "ensaio.pleme.io/run"))
  :fixtures ((:name "space" :effect create :cleanup own-inverse
              :action (:carve-namespace (:name "qual-carve"
                                         :quota ((:name "pods" :value "4") (:name "requests.cpu" :value "1"))
                                         :limit-default ((:name "cpu" :value "200m") (:name "memory" :value "128Mi"))
                                         :limit-default-request ((:name "cpu" :value "50m") (:name "memory" :value "64Mi"))))))
  :steps ((:place "space") (:wait "5s"))
  :settle "60s")
