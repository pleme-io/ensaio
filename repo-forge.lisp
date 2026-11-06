(defrepo ensaio
  :description "Ensaio (Portuguese, trial/rehearsal/assay) — the typed architectural-trial primitive. Declares N ephemeral systems, wires them, runs a typed catalog of assertions via a (defensaio ...) tatara-lisp authoring surface over a Rust border, records a durable verdict, tears down. Engine, MCP surface and the utsuroi-lifecycle library."
  :kind :rust-tool
  :visibility :public
  :binary "ensaio"
  :external-crates
    ((:name "tokio"              :version "1"     :features ("full"))
     (:name "serde"              :version "1"     :features ("derive"))
     (:name "serde_json"         :version "1")
     (:name "serde_yaml_ng"      :version "0.10")
     (:name "thiserror"          :version "2")
     (:name "anyhow"             :version "1")
     (:name "async-trait"        :version "0.1")
     (:name "clap"               :version "4"     :features ("derive" "env"))
     (:name "tracing"            :version "0.1")
     (:name "tracing-subscriber" :version "0.3"   :features ("env-filter" "fmt" "json"))
     (:name "tatara-lisp"        :version "0.3.3")
     (:name "kube"               :version "0.99"  :features ("client" "rustls-tls" "runtime"))
     (:name "k8s-openapi"        :version "0.24"  :features ("latest"))
     (:name "reqwest"            :version "0.12"  :features ("json" "rustls-tls")))
  :ci
    (:systems       ("aarch64-darwin" "x86_64-linux" "aarch64-linux")
     :test-systems  ("x86_64-linux")
     :build-images  #f))
