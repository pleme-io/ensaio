use std::path::Path;

use serde::Serialize;

use super::config::{FixtureClass, Tier, VisitaConfig};
use super::spec::VisitaSpec;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "refusal")]
pub enum TierRefusal {
    ClassNotAllowed {
        fixture: String,
        target: String,
        tier: Tier,
        class: FixtureClass,
    },
    NoOpenWindow {
        fixture: String,
        target: String,
        tier: Tier,
        class: FixtureClass,
        tag: String,
    },
}

impl std::fmt::Display for TierRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClassNotAllowed {
                fixture,
                target,
                tier,
                class,
            } => write!(
                f,
                "fixture `{fixture}`: target `{target}` (tier {tier:?}) does not allow class `{}`; its carve.allow must list it",
                class.word()
            ),
            Self::NoOpenWindow {
                fixture,
                target,
                tier,
                class,
                tag,
            } => write!(
                f,
                "fixture `{fixture}`: target `{target}` (tier {tier:?}) requires a change window for `{}` writes, and no window tagged `{tag}` is open now",
                class.word()
            ),
        }
    }
}

#[must_use]
pub fn open_windows(dir: &Path) -> Vec<guardrail::model::ChangeWindow> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json"))
        .collect();
    names.sort();
    guardrail::windows::from_files(&names, dir).0
}

#[must_use]
pub fn check(
    spec: &VisitaSpec,
    config: &VisitaConfig,
    windows: &[guardrail::model::ChangeWindow],
    now_unix: i64,
) -> Vec<TierRefusal> {
    let mut out = Vec::new();
    for f in &spec.fixtures {
        let actions = f.action.iter().chain(match &f.cleanup {
            crate::fixture::Cleanup::Inverse(inv) => inv.action.as_ref(),
            _ => None,
        });
        for a in actions {
            let class = FixtureClass::of(a);
            for ctx in a.contexts() {
                let Some((name, t)) = config.target_for_context(ctx) else {
                    continue;
                };
                if !t.carve.allow.contains(&class) {
                    out.push(TierRefusal::ClassNotAllowed {
                        fixture: f.name.clone(),
                        target: name.into(),
                        tier: t.tier,
                        class,
                    });
                    continue;
                }
                if t.carve.requires_window {
                    let tag = t
                        .carve
                        .window_tag
                        .clone()
                        .unwrap_or_else(|| name.to_string());
                    if guardrail::windows::open_window(windows, &tag, now_unix).is_none() {
                        out.push(TierRefusal::NoOpenWindow {
                            fixture: f.name.clone(),
                            target: name.into(),
                            tier: t.tier,
                            class,
                            tag,
                        });
                    }
                }
            }
        }
    }
    out
}
