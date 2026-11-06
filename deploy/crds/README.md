# CRDs — generated, GitOps-applied, and deliberately NOT in a chart

`ensaio-crd.yaml` is the generator's output (`ensaio print-crd`) and the
canonical copy the tests compare against. Apply it from a GitOps tree, never
from a Helm chart's `crds/` directory.

## Why not a chart's `crds/`

Helm installs everything in a chart's `crds/` directory **once, on install, and
never upgrades it**. For an evolving CRD that is a trap with two failure modes:

1. **Silent freeze.** A CRD installed from a chart has no managed fields on the
   cluster. The next field added to `EnsaioCrdSpec` simply does not arrive, and
   nothing goes red.
2. **A silently-lost conflict.** A CRD applied both from a chart and from a
   GitOps tree has two field managers, and the GitOps server-side apply loses
   the conflict every cycle while still reporting Ready. Measured 2026-08-19:
   a live CRD had four spec fields where git had six.

## Generating

    cargo run --bin ensaio -- print-crd > ensaio-crd.yaml

## Changing the CRD

Edit the Rust types, then regenerate this copy and every deployed mirror. The
tests fail until this copy matches the generator; nothing but review catches a
stale deployed mirror, and a stale deployed CRD **prunes unknown fields
silently**.
