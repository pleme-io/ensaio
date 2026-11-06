# ensaio

> Ensaio (Portuguese, trial/rehearsal/assay) — the typed architectural-trial primitive. Declares N ephemeral systems, wires them, runs a typed catalog of assertions via a `(defensaio ...)` tatara-lisp authoring surface over a Rust border, records a durable verdict, tears down.

## Crates

| crate | what it is |
|---|---|
| `ensaio` (`ensaio-lib/`) | the engine: typed spec, interpreter, `Ensaio` CRD and controller, certification receipts, visita, and the `ensaio` CLI |
| `ensaio-mcp` | the MCP surface for driving a running instance |
| `utsuroi-lifecycle` | the shared lifecycle vocabulary: `Ttl`, `Lease`, `Boundedness`, `ReapMechanism`, phases and stages |

## Guest trials: `ensaio visita`

A trial on borrowed ground: snapshot the natural state, change it only through
declared reversible steps, observe through its own API, put it back, and prove
it is back with a diff — creating nothing in the target it does not delete
again. See [`docs/VISITA.md`](./docs/VISITA.md).

## Deployments

A deployment depends on this repo by git, flattens `ensaio::cli::CoreCommand`
into its own binary, and sets its scope gate:

| variable | meaning |
|---|---|
| `ENSAIO_SCOPE` | the only `spec.scope` the controller reconciles; the controller refuses to start without it |
| `ENSAIO_CERTIFY_NAMESPACE` | default namespace for `ensaio certify` probe Jobs |
| `ENSAIO_PROBE_SERVICE_ACCOUNT` | ServiceAccount the probe pods run as |

## Building

```bash
nix run .#ensaio -- --help
nix build .#ensaio-mcp
cargo test --workspace
```

## License

MIT.
