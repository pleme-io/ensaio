//! The objects an `:apply-manifest` fixture creates, read from its manifest,
//! and the identities it records for the undo.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One object this run created: enough to address it, and its `uid`, so the
/// undo deletes the object it created and never a later one of the same name.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectRef {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
}

impl ObjectRef {
    /// The identity of a Kubernetes object, when it has one.
    #[must_use]
    pub fn of(obj: &Value) -> Option<Self> {
        let text = |p: &str| obj.pointer(p).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            api_version: text("/apiVersion")?,
            kind: text("/kind")?,
            name: text("/metadata/name")?,
            uid: text("/metadata/uid"),
        })
    }

    /// The kubectl resource argument: `kind.version.group`, or the bare kind
    /// for the core group, so a CRD whose kind collides with another resolves
    /// to the one in the manifest.
    #[must_use]
    pub fn resource(&self) -> String {
        let kind = self.kind.to_ascii_lowercase();
        match self.api_version.split_once('/') {
            Some((group, version)) => format!("{kind}.{version}.{group}"),
            None => kind,
        }
    }

    /// Whether `obj` is this object — the same name, and the same uid when
    /// this one recorded a uid.
    #[must_use]
    pub fn is(&self, obj: &Value) -> bool {
        Self::of(obj)
            .is_some_and(|o| o.name == self.name && (self.uid.is_none() || o.uid == self.uid))
    }
}

impl std::fmt::Display for ObjectRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.kind, self.name)
    }
}

/// The manifest's text: the file, else the inline YAML.
///
/// # Errors
///
/// Neither or both given, or a file that cannot be read.
pub fn source(file: Option<&str>, yaml: Option<&str>) -> Result<String, String> {
    match (file, yaml) {
        (Some(f), None) => {
            std::fs::read_to_string(Path::new(f)).map_err(|e| format!("manifest `{f}`: {e}"))
        }
        (None, Some(y)) => Ok(y.to_string()),
        (None, None) => Err("names no manifest: give :file or :yaml".into()),
        (Some(_), Some(_)) => Err("gives both :file and :yaml; one manifest per fixture".into()),
    }
}

/// Every object in a manifest: each YAML document, with a `List`'s items
/// flattened. Each must carry `apiVersion`, `kind` and `metadata.name` — a
/// `generateName` object could not be found again to delete — and a
/// `metadata.namespace`, when it has one, must be `namespace`.
///
/// # Errors
///
/// Unparseable YAML, an empty manifest, or an object that breaks the rules
/// above, each named.
pub fn objects(text: &str, namespace: &str) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for doc in serde_yaml_ng::Deserializer::from_str(text) {
        let v = Value::deserialize(doc).map_err(|e| format!("manifest is not YAML: {e}"))?;
        match v.get("items").and_then(Value::as_array) {
            Some(items) if v.get("kind").and_then(Value::as_str) == Some("List") => {
                out.extend(items.iter().cloned())
            }
            _ if v.is_null() => {}
            _ => out.push(v),
        }
    }
    if out.is_empty() {
        return Err("manifest holds no object".into());
    }
    for (i, o) in out.iter().enumerate() {
        let r = ObjectRef::of(o).ok_or_else(|| {
            format!("manifest object {i} lacks apiVersion, kind or metadata.name (generateName cannot be found again to delete)")
        })?;
        if let Some(ns) = o.pointer("/metadata/namespace").and_then(Value::as_str)
            && ns != namespace
        {
            return Err(format!(
                "manifest object {r} names namespace `{ns}`, not the fixture's `{namespace}`"
            ));
        }
    }
    Ok(out)
}
