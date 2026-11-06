(defvisita
  :name "qual-carve-fence"
  :target "lab"
  :claim "A fence is a default-deny policy set plus the declared peers, on a carved namespace, and both are gone afterwards."
  :natural-state ((:namespace "default" :kinds ("namespaces") :names ("default"))
                  (:namespace "default" :kinds ("namespaces") :selector "ensaio.pleme.io/run"))
  :fixtures ((:name "space" :effect create :cleanup own-inverse
              :action (:carve-namespace (:name "qual-fence")))
             (:name "fence" :effect create :cleanup own-inverse :after ("space")
              :action (:fence (:namespace "qual-fence"
                               :egress (dns (:namespace "default"))
                               :ingress ((:namespace "qual-fence"))))))
  :steps ((:place "space") (:place "fence") (:wait "5s"))
  :settle "60s")
