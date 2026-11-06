use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use shikumi::TieredConfig;

use super::spec::Action;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum FixtureClass {
    Annotate,
    Patch,
    ResizePod,
    RolloutRestart,
    DispatchWorkflow,
    ArgoSync,
    RabbitmqCloseConnections,
    ApplyManifest,
    Exec,
    CarveNamespace,
    Fence,
    CarveNode,
    ArgocdPause,
    DeleteRunnerPod,
}

impl FixtureClass {
    pub const ALL: &'static [Self] = &[
        Self::Annotate,
        Self::Patch,
        Self::ResizePod,
        Self::RolloutRestart,
        Self::DispatchWorkflow,
        Self::ArgoSync,
        Self::RabbitmqCloseConnections,
        Self::ApplyManifest,
        Self::Exec,
        Self::CarveNamespace,
        Self::Fence,
        Self::CarveNode,
        Self::ArgocdPause,
        Self::DeleteRunnerPod,
    ];

    #[must_use]
    pub fn of(a: &Action) -> Self {
        match a {
            Action::Annotate { .. } => Self::Annotate,
            Action::Patch { .. } => Self::Patch,
            Action::ResizePod { .. } => Self::ResizePod,
            Action::RolloutRestart { .. } => Self::RolloutRestart,
            Action::DispatchWorkflow { .. } => Self::DispatchWorkflow,
            Action::ArgoSync { .. } => Self::ArgoSync,
            Action::RabbitmqCloseConnections { .. } => Self::RabbitmqCloseConnections,
            Action::ApplyManifest { .. } => Self::ApplyManifest,
            Action::Exec { .. } => Self::Exec,
            Action::CarveNamespace { .. } => Self::CarveNamespace,
            Action::Fence { .. } => Self::Fence,
            Action::CarveNode { .. } => Self::CarveNode,
            Action::ArgocdPause { .. } => Self::ArgocdPause,
            Action::DeleteRunnerPod { .. } => Self::DeleteRunnerPod,
        }
    }

    #[must_use]
    pub fn word(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    Production,
    Staging,
    Development,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Carve {
    pub allow: BTreeSet<FixtureClass>,
    pub requires_window: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_tag: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub context: String,
    pub tier: Tier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default)]
    pub carve: Carve,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct LeaseDefaults {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub ttl: String,
}

impl Default for LeaseDefaults {
    fn default() -> Self {
        Self {
            enabled: false,
            namespace: None,
            ttl: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Windows {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Kubectl {
    pub client_validation: bool,
}

impl Default for Kubectl {
    fn default() -> Self {
        Self {
            client_validation: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct VisitaConfig {
    pub targets: BTreeMap<String, Target>,
    pub lease: LeaseDefaults,
    pub windows: Windows,
    pub kubectl: Kubectl,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
}

pub const APP: &str = "visita";

impl TieredConfig for VisitaConfig {
    fn bare() -> Self {
        Self::default()
    }

    fn prescribed_default() -> Self {
        Self {
            lease: LeaseDefaults {
                enabled: true,
                namespace: None,
                ttl: "2h".into(),
            },
            ..Self::default()
        }
    }
}

#[derive(Serialize, Debug, Clone, Default, clap::Args)]
pub struct VisitaFlags {
    #[arg(long = "operator")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    #[command(flatten)]
    #[serde(skip_serializing_if = "LeaseFlags::is_empty")]
    pub lease: LeaseFlags,
}

#[derive(Serialize, Debug, Clone, Default, clap::Args)]
pub struct LeaseFlags {
    #[arg(long = "lease-ttl")]
    #[serde(rename = "ttl", skip_serializing_if = "Option::is_none")]
    pub lease_ttl: Option<String>,
    #[arg(long = "lease-namespace")]
    #[serde(rename = "namespace", skip_serializing_if = "Option::is_none")]
    pub lease_namespace: Option<String>,
    #[arg(long = "lease-enabled")]
    #[serde(rename = "enabled", skip_serializing_if = "Option::is_none")]
    pub lease_enabled: Option<bool>,
}

impl LeaseFlags {
    fn is_empty(&self) -> bool {
        self.lease_ttl.is_none() && self.lease_namespace.is_none() && self.lease_enabled.is_none()
    }
}

impl VisitaConfig {
    pub fn resolve(args: &shikumi::cli::ConfigArgs, flags: &VisitaFlags) -> Result<Self, String> {
        args.resolve::<Self>(APP, flags)
            .map(shikumi::ProgressiveResolution::into_value)
            .map_err(|e| format!("visita config: {e}"))
    }

    pub fn from_layers(layers: &[shikumi::ProgressiveLayer]) -> Result<Self, String> {
        Self::try_resolve_progressive_with(layers)
            .map(shikumi::ProgressiveResolution::into_value)
            .map_err(|e| format!("visita config: {e}"))
    }

    pub fn check(&self) -> Result<(), String> {
        let mut errs = Vec::new();
        for (name, t) in &self.targets {
            if t.context.is_empty() {
                errs.push(format!("target `{name}`: no context"));
            }
            if t.tier == Tier::Production && !t.carve.requires_window {
                errs.push(format!(
                    "target `{name}` is tier production and does not require a change window; production writes only inside one"
                ));
            }
        }
        if self.lease.enabled && humantime::parse_duration(&self.lease.ttl).is_err() {
            errs.push(format!("lease.ttl `{}` is not a duration", self.lease.ttl));
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("\n"))
        }
    }

    #[must_use]
    pub fn target_for_context(&self, context: &str) -> Option<(&str, &Target)> {
        self.targets
            .iter()
            .find(|(_, t)| t.context == context)
            .map(|(n, t)| (n.as_str(), t))
    }

    #[must_use]
    pub fn operator_id(&self) -> String {
        self.operator
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "operator".into())
    }

    #[must_use]
    pub fn lease_namespace(&self, target: &Target) -> String {
        self.lease
            .namespace
            .clone()
            .or_else(|| target.namespace.clone())
            .unwrap_or_else(|| "default".into())
    }

    #[must_use]
    pub fn windows_dir(&self) -> PathBuf {
        self.windows
            .dir
            .clone()
            .unwrap_or_else(|| state_home().join("guardrail/windows.d"))
    }
}

#[must_use]
pub fn state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
}
