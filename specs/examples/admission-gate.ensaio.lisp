(defensaio
  :name "admission-gate"
  :scope "trials"
  :ttl "30m"
  :claim "Does an Authservice-shaped gate admit a machine call with no human SSO session, before the backend evaluates its own access-token proof?"
  :systems ((:id "authservice-mock" :template "envoy-authservice-minimal" :breathe #t)
            (:id "backend-stub" :template "backend-minimal" :breathe #t))
  :wiring ((:from "authservice-mock" :to "backend-stub" :via "cluster-dns"))
  :assertions ((:id "m2m-admitted-without-sso"
                :kind AuthserviceAdmission
                :target "backend-stub"
                :proof-channel "access-token-field"
                :session "none"))
  :record #t)
