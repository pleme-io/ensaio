//! `ensaio-mcp` — the MCP surface a model uses to drive a running
//! Ensaio typed-architectural-trial instance.
//!
//! Mirrors `breathe-mcp`'s shape (list/get/apply/catalog over stdio,
//! one `#[tool]` per verb, the CRD IS the state) but for a single CRD
//! kind (`Ensaio`), driving `ensaio-lib` as a `path` dependency.
//! It lives as its own crate (not a second binary alongside `ensaio`
//! in one Cargo.toml) specifically so `rmcp`'s schemars 1.x
//! requirement never has to coexist in the same dependency graph as
//! `ensaio-lib`'s schemars 0.8 (`crd.rs`'s `CustomResource` derive
//! needs) — a Cargo package-rename does not work here, since
//! schemars 1.x's derive macro emits code with hardcoded absolute
//! paths back to `::schemars::...`, which resolve against the
//! calling crate's own unaliased dependency, not a rename alias.
//!
//! `apply`'s `spec` field is a raw JSON [`Value`] (matching
//! [`ensaio::crd::EnsaioCrdSpec`]'s wire shape, camelCase) rather than
//! a schemars-derived nested struct, so `EnsaioCrdSpec`'s own schemars
//! 0.8 derive never has to cross the crate boundary at all;
//! deserialization (`serde_json::from_value`) is independent of
//! schemars either way.

use anyhow::Result;
use ensaio::crd::{Ensaio, EnsaioCrdSpec};
use kube::api::{ObjectMeta, Patch, PatchParams};
use kube::{Api, Client, ResourceExt};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

// ─────────────────────────── tool inputs ───────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListInput {
    #[serde(default)]
    #[schemars(description = "Namespace to scope to. Omitted = all namespaces.")]
    pub namespace: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EnsaioRef {
    #[schemars(description = "Namespace of the Ensaio CR")]
    pub namespace: String,
    #[schemars(description = "Name of the Ensaio CR")]
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApplyInput {
    #[schemars(description = "Namespace to create/update the Ensaio CR in")]
    pub namespace: String,
    #[schemars(description = "Name of the Ensaio CR")]
    pub name: String,
    #[schemars(
        description = "EnsaioCrdSpec-shaped JSON: scope (must equal the controller's ENSAIO_SCOPE), ttl, claim, systems, wiring, assertions, record, confirmAfterRuns"
    )]
    pub spec: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Empty {}

// ─────────────────────────── the server ────────────────────────────

fn ok(v: &Value) -> String {
    kotae::Answer::found_value(v.clone()).render()
}

/// Nothing was found, and that IS the answer — not a failure to look.
fn empty(of: impl Into<String>) -> String {
    kotae::Answer::empty(of).render()
}

/// The caller must change something, and here is what would be accepted.
fn refused(because: impl Into<String>, legal: &[&str]) -> String {
    kotae::Answer::refused(because, legal.iter().copied()).render()
}

/// The question could not be put at all. The reader learns nothing about the
/// subject, only about the attempt.
fn blind(because: impl std::fmt::Display) -> String {
    kotae::Answer::blind(because.to_string()).render()
}

/// Classify a `kube` failure into the arm it actually is.
///
/// This is the whole value of the adoption. A 404 from `get` is not a failure:
/// the cluster answered, and the answer is that no such Ensaio exists. A 409
/// or 422 is not blindness either — the request was understood and rejected,
/// so the caller can fix it. Everything else (no route to the apiserver, an
/// expired credential, a TLS failure) is genuine blindness, where the reader
/// must conclude nothing about whether the object exists.
///
/// `subject` names what was being looked for, so an empty answer reads as
/// "no ensaio trials/foo" rather than a bare absence.
fn kube_answer(e: &kube::Error, subject: &str) -> String {
    match e {
        kube::Error::Api(resp) => match resp.code {
            404 => empty(subject.to_string()),
            400 | 403 | 405 | 409 | 422 => refused(
                format!("{subject}: {}", resp.message),
                &["a request the apiserver accepts — see .reason and .message"],
            ),
            _ => blind(format!("{subject}: {}", resp.message)),
        },
        other => blind(format!("{subject}: {other}")),
    }
}

/// The required fields of `EnsaioCrdSpec` in their wire (camelCase) form —
/// what a refused `apply` hands back so the retry costs one call.
const SPEC_REQUIRED: &[&str] = &["scope", "ttl", "claim", "systems", "wiring", "assertions"];

#[derive(Clone)]
pub struct EnsaioMcp {
    client: Client,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl EnsaioMcp {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List Ensaio trial CRs, optionally namespace-scoped. Returns each CR's spec + status.phase + results."
    )]
    pub async fn ensaio_list(&self, Parameters(p): Parameters<ListInput>) -> String {
        let api: Api<Ensaio> = match &p.namespace {
            Some(ns) => Api::namespaced(self.client.clone(), ns),
            None => Api::all(self.client.clone()),
        };
        let scope = match &p.namespace {
            Some(ns) => format!("ensaios in {ns}"),
            None => "ensaios in any namespace".to_string(),
        };
        match api.list(&Default::default()).await {
            // A named key, not kotae's fallback `result`: a bare array cannot
            // carry a discriminant, which is precisely why `[]` used to be
            // indistinguishable from a failed list.
            Ok(list) if !list.items.is_empty() => ok(&json!({ "ensaios": list.items })),
            Ok(_) => empty(scope),
            Err(e) => kube_answer(&e, &scope),
        }
    }

    #[tool(description = "Get one Ensaio trial CR by namespace/name (full spec + status).")]
    pub async fn ensaio_get(&self, Parameters(p): Parameters<EnsaioRef>) -> String {
        let api: Api<Ensaio> = Api::namespaced(self.client.clone(), &p.namespace);
        match api.get(&p.name).await {
            Ok(cr) => ok(&json!(cr)),
            Err(e) => kube_answer(&e, &format!("ensaio {}/{}", p.namespace, p.name)),
        }
    }

    #[tool(
        description = "Create or update (server-side apply) an Ensaio trial CR. The controller picks it up and drives status.phase through Pending -> Provisioning -> ... -> a terminal state (Done/Failed/TtlExceeded). scope must equal the controller's ENSAIO_SCOPE -- the controller ignores any other value and RBAC grants write only inside the deployment's own namespaces."
    )]
    pub async fn ensaio_apply(&self, Parameters(p): Parameters<ApplyInput>) -> String {
        let spec: EnsaioCrdSpec = match serde_json::from_value(p.spec) {
            Ok(s) => s,
            Err(e) => {
                return refused(
                    format!("spec does not match EnsaioCrdSpec: {e}"),
                    SPEC_REQUIRED,
                );
            }
        };
        let api: Api<Ensaio> = Api::namespaced(self.client.clone(), &p.namespace);
        let cr = Ensaio {
            metadata: ObjectMeta {
                name: Some(p.name.clone()),
                namespace: Some(p.namespace.clone()),
                ..Default::default()
            },
            spec,
            status: None,
        };
        match api
            .patch(
                &p.name,
                &PatchParams::apply("ensaio-mcp").force(),
                &Patch::Apply(&cr),
            )
            .await
        {
            Ok(applied) => ok(&json!({
                "name": applied.name_any(),
                "namespace": p.namespace,
                "phase": applied.status.as_ref().map(|s| format!("{:?}", s.phase)),
            })),
            Err(e) => kube_answer(&e, &format!("ensaio {}/{}", p.namespace, p.name)),
        }
    }

    #[tool(
        description = "Retire (delete) an Ensaio trial CR. Rare, deliberate -- the controller's own teardown on reaching a terminal phase already scales every provisioned system to zero; retiring removes the declaration itself."
    )]
    pub async fn ensaio_retire(&self, Parameters(p): Parameters<EnsaioRef>) -> String {
        let api: Api<Ensaio> = Api::namespaced(self.client.clone(), &p.namespace);
        match api.delete(&p.name, &Default::default()).await {
            Ok(_) => ok(&json!({ "retired": p.name, "namespace": p.namespace })),
            // A 404 here is not a failed retirement — there was nothing to
            // retire, which is the same end state the caller wanted.
            Err(e) => kube_answer(&e, &format!("ensaio {}/{}", p.namespace, p.name)),
        }
    }

    #[tool(
        description = "The self-describing assertion-kind catalog (CATALOG REFLECTION): every AssertionKind an Ensaio trial can declare."
    )]
    pub async fn ensaio_catalog(&self, Parameters(_): Parameters<Empty>) -> String {
        let kinds: Vec<Value> = ensaio::AssertionKind::ALL
            .iter()
            .map(|k| json!({ "kind": format!("{k:?}") }))
            .collect();
        ok(&json!({ "assertionKinds": kinds }))
    }
}

#[tool_handler]
impl ServerHandler for EnsaioMcp {
    fn get_info(&self) -> ServerInfo {
        // Built by mutating a default rather than with a struct expression:
        // rmcp 1.x marks ServerInfo #[non_exhaustive], which forbids the struct
        // form from outside the defining crate EVEN WITH `..Default::default()`
        // (E0639). That is deliberate on rmcp's side -- it reserves the right to
        // add fields without a breaking change -- so this shape is the stable one
        // to write against, not a workaround to undo later.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "Drive a running Ensaio typed-architectural-trial instance. List/get/apply/retire \
             Ensaio CRs (ensaio.pleme.io/v1alpha1) and read the assertion-kind catalog. Applying \
             a CR hands it to the controller, which reconciles status.phase toward a terminal \
             state (Done/Failed/TtlExceeded) -- this tool only declares, it never runs the trial \
             logic itself. scope must equal the controller's ENSAIO_SCOPE; the controller enforces this \
             at runtime and \
             RBAC enforces it at the API layer."
                .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ensaio_mcp=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("ensaio-mcp starting (stdio transport)");

    let client = Client::try_default()
        .await
        .map_err(|e| anyhow::anyhow!("failed to initialise kube client: {e}"))?;
    let server = EnsaioMcp::new(client);

    let service = server
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("serve: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("waiting: {e}"))?;

    tracing::info!("ensaio-mcp exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_err(code: u16) -> kube::Error {
        kube::Error::Api(kube::core::ErrorResponse {
            status: "Failure".into(),
            message: "boom".into(),
            reason: "Whatever".into(),
            code,
        })
    }

    fn outcome(raw: &str) -> String {
        serde_json::from_str::<Value>(raw).expect("renders valid json")["outcome"]
            .as_str()
            .expect("every answer carries an outcome discriminant")
            .to_string()
    }

    /// The point of the whole adoption: a 404 is an ANSWER, not a failure.
    /// The apiserver was reached and it told us the object is not there.
    #[test]
    fn not_found_is_empty_not_blind() {
        assert_eq!(outcome(&kube_answer(&api_err(404), "ensaio ns/x")), "empty");
    }

    /// Understood and rejected — the caller can fix the request, so it must
    /// not be reported as blindness (which would say "retry, learn nothing").
    #[test]
    fn rejected_requests_are_refused() {
        for code in [400, 403, 405, 409, 422] {
            assert_eq!(
                outcome(&kube_answer(&api_err(code), "ensaio ns/x")),
                "refused",
                "code {code}"
            );
        }
    }

    /// A 500, or anything that is not an apiserver reply at all (no route,
    /// expired credential, TLS failure), leaves us knowing nothing about the
    /// subject — only about the attempt.
    #[test]
    fn server_errors_and_transport_failures_are_blind() {
        assert_eq!(outcome(&kube_answer(&api_err(500), "ensaio ns/x")), "blind");
        assert_eq!(outcome(&kube_answer(&api_err(503), "ensaio ns/x")), "blind");
    }

    /// The four arms must never render the same discriminant, or the reader is
    /// back to guessing — which is the entire failure kotae exists to remove.
    #[test]
    fn the_four_arms_are_mutually_distinguishable() {
        let seen = [
            outcome(&ok(&json!({ "a": 1 }))),
            outcome(&empty("ensaios in ns")),
            outcome(&refused("bad spec", SPEC_REQUIRED)),
            outcome(&blind("no route to apiserver")),
        ];
        let mut uniq = seen.clone().to_vec();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), 4, "arms collided: {seen:?}");
    }

    /// A refusal carries what WOULD be accepted, so the retry is one call
    /// rather than a conversation.
    #[test]
    fn a_refusal_names_the_legal_set() {
        let v: Value = serde_json::from_str(&refused("nope", SPEC_REQUIRED)).unwrap();
        let legal: Vec<&str> = v["legal"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert!(legal.contains(&"assertions"), "got {legal:?}");
        assert!(legal.contains(&"scope"), "got {legal:?}");
    }

    /// An empty list must not render as a bare `[]`. That shape is exactly
    /// what made "no ensaios here" unreadable against "the list call failed".
    #[test]
    fn an_empty_list_is_not_a_bare_array() {
        let raw = empty("ensaios in trials");
        assert!(
            !raw.trim_start().starts_with('['),
            "still a bare array: {raw}"
        );
        assert_eq!(outcome(&raw), "empty");
        assert!(raw.contains("trials"), "the subject is named: {raw}");
    }
}
