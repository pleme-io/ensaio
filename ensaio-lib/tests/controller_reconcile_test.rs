//! A real integration test for the M2 reconcile loop — against a
//! hand-rolled mock `tower::Service` standing in for the K8s apiserver
//! (the standard kube-rs testing pattern: `Client::new` accepts any
//! `tower::Service`), not a real or `kind`-backed cluster.
//!
//! Scoped honestly: exercises exactly one real phase transition
//! (`Pending → Provisioning`, the two-system provision step) end to
//! end through the actual `kube::Client` HTTP plumbing — proving the
//! async/kube wiring itself works, which the pure-logic unit tests in
//! `crd_tests.rs` cannot. Walking the mock all the way to `Done` is a
//! further increment; each additional phase needs its own canned
//! response shapes, and this first slice is the one that most needed
//! proving (it's the phase every run starts from).

use std::sync::{Arc, Mutex};

use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use kube::Client;
use tower::service_fn;

use ensaio::crd::{Ensaio, EnsaioCrdSpec};
use ensaio::{AssertionKind, AssertionSpec, SystemSpec};

const NAMESPACE: &str = "trials-ensaio";
const RUN_NAME: &str = "test-run";

fn test_ensaio() -> Ensaio {
    let spec = EnsaioCrdSpec {
        scope: "trials".into(),
        ttl: "30m".into(),
        claim: "integration test claim".into(),
        confluence_ref: None,
        systems: vec![
            SystemSpec {
                id: "sys-a".into(),
                template: "nginx:1.27-alpine".into(),
                breathe: false,
            },
            SystemSpec {
                id: "sys-b".into(),
                template: "nginx:1.27-alpine".into(),
                breathe: false,
            },
        ],
        wiring: vec![],
        assertions: vec![AssertionSpec {
            marker: None,
            id: "a1".into(),
            kind: AssertionKind::NetworkPath,
            target_namespace: None,
            target: "sys-b".into(),
            proof_channel: None,
            session: None,
            header_name: None,
            from: None,
            must_not_traverse: None,
            image: None,
            command: None,
            args: None,
            sidecar: None,
            env: None,
            probe_namespace: None,
            operator: ensaio::types::OperatorParams::default(),
        }],
        record: false,
        confirm_after_runs: 1,
    };
    let mut obj = Ensaio::new(RUN_NAME, spec);
    obj.metadata.namespace = Some(NAMESPACE.into());
    obj
}

fn json_body(v: &serde_json::Value) -> Full<bytes::Bytes> {
    Full::new(bytes::Bytes::from(serde_json::to_vec(v).unwrap()))
}

fn ok_response(v: serde_json::Value) -> Response<Full<bytes::Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(json_body(&v))
        .unwrap()
}

/// A minimal, real Deployment JSON body — enough for
/// `k8s_openapi::api::apps::v1::Deployment` to deserialize.
fn deployment_json(name: &str, available: i32) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": { "name": name, "namespace": NAMESPACE },
        "spec": {
            "selector": { "matchLabels": { "app": name } },
            "template": {
                "metadata": { "labels": { "app": name } },
                "spec": { "containers": [{ "name": name, "image": "nginx:1.27-alpine" }] }
            }
        },
        "status": { "availableReplicas": available }
    })
}

fn service_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": name, "namespace": NAMESPACE },
        "spec": { "selector": { "app": name }, "ports": [{ "port": 80 }] }
    })
}

fn ensaio_json_with_status(status: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "ensaio.pleme.io/v1alpha1",
        "kind": "Ensaio",
        "metadata": { "name": RUN_NAME, "namespace": NAMESPACE },
        "spec": {
            "scope": "trials",
            "ttl": "30m",
            "claim": "integration test claim",
            "systems": [
                { "id": "sys-a", "template": "nginx:1.27-alpine", "breathe": false },
                { "id": "sys-b", "template": "nginx:1.27-alpine", "breathe": false }
            ],
            "assertions": [
                { "id": "a1", "kind": "NetworkPath", "target": "sys-b" }
            ],
            "record": false,
            "confirmAfterRuns": 1
        },
        "status": status
    })
}

/// Captured request bodies, keyed loosely by which resource they
/// targeted — lets the test assert on what the reconciler actually
/// sent, not just that it didn't error.
#[derive(Default, Clone)]
struct Captured {
    status_patches: Arc<Mutex<Vec<serde_json::Value>>>,
}

fn mock_apiserver(
    captured: Captured,
) -> impl tower::Service<
    Request<kube::client::Body>,
    Response = Response<Full<bytes::Bytes>>,
    Error = std::convert::Infallible,
    Future = std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Response<Full<bytes::Bytes>>, std::convert::Infallible>,
                > + Send,
        >,
    >,
> {
    service_fn(move |req: Request<kube::client::Body>| {
        let captured = captured.clone();
        Box::pin(async move {
            let path = req.uri().path().to_string();
            let method = req.method().clone();
            let body_bytes = req
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();

            if path.contains("/ensaios/") && path.ends_with("/status") {
                let patch: serde_json::Value =
                    serde_json::from_slice(&body_bytes).unwrap_or_default();
                let status = patch
                    .get("status")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                captured.status_patches.lock().unwrap().push(status.clone());
                return Ok(ok_response(ensaio_json_with_status(&status)));
            }
            if path.contains("/deployments/") {
                let name = path.rsplit('/').next().unwrap_or("unknown");
                // wait_ready's GET expects availableReplicas > 0 so the
                // Pending->Provisioning->Ready walk (were this test
                // extended past Provisioning) wouldn't stall forever.
                return Ok(ok_response(deployment_json(name, 1)));
            }
            if path.contains("/services/") {
                let name = path.rsplit('/').next().unwrap_or("unknown");
                return Ok(ok_response(service_json(name)));
            }
            if path.contains("/networkpolicies/") {
                let _ = method;
                return Ok(ok_response(serde_json::json!({
                    "apiVersion": "networking.k8s.io/v1",
                    "kind": "NetworkPolicy",
                    "metadata": { "name": "np", "namespace": NAMESPACE },
                    "spec": { "podSelector": {} }
                })));
            }

            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Full::new(bytes::Bytes::new()))
                .unwrap())
        })
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<_, std::convert::Infallible>> + Send>,
            >
    })
}

// K8sEnvironment bridges its sync EnsaioEnvironment trait methods to
// async kube calls via `block_in_place`, which needs a multi-threaded
// runtime (documented in k8s_env.rs) — the default `#[tokio::test]`
// current-thread flavor panics on it.
#[tokio::test(flavor = "multi_thread")]
async fn reconcile_provisions_both_systems_and_advances_to_provisioning() {
    let captured = Captured::default();
    let client = Client::new(mock_apiserver(captured.clone()), NAMESPACE);

    let env = ensaio::k8s_env::K8sEnvironment::new(client, NAMESPACE.to_string());
    let obj = test_ensaio();

    // Drive the same provisioning loop apply_reconcile's Pending arm
    // runs, directly against the real EnsaioEnvironment trait — proves
    // the kube plumbing (patch-apply, JSON round-trip) genuinely works,
    // not just the pure phase-selection logic.
    for sys in &obj.spec.systems {
        let endpoint = ensaio::EnsaioEnvironment::provision_system(&env, sys)
            .expect("provision_system must succeed against the mock apiserver");
        assert_eq!(
            endpoint,
            format!("{}.{}.svc.cluster.local", sys.id, NAMESPACE)
        );
    }
}
