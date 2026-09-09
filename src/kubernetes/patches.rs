//! Manifest patches: ad hoc JSON Patch operations applied to rendered
//! manifests, for the knobs a config repo never declared (a debug mount, an
//! extra env var). Sticky like selections, with the same durability, and
//! recorded on every revision. A patch that no longer applies fails the
//! render, and therefore the reconcile, loudly: nothing is applied or
//! pruned until it is fixed or removed.

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::kubernetes::deploy_config::Template;
use crate::kubernetes::selections::Durability;

/// Which rendered manifest a patch applies to. `kind` and `name` identify
/// it; `file` narrows to the template file when two manifests share both.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PatchTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub kind: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PatchOp {
    Add,
    Replace,
    Remove,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ManifestPatch {
    pub target: PatchTarget,
    pub op: PatchOp,
    /// JSON pointer into the manifest, e.g. `/spec/replicas`.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(default)]
    pub durability: Durability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

impl ManifestPatch {
    pub fn is_temporary(&self) -> bool {
        self.durability == Durability::Temporary
    }

    /// The operation without its durability, the way the deploy page lists
    /// it: `replace /spec/replicas = 3 on Deployment/web (deployment.yaml)`.
    pub fn describe_op(&self) -> String {
        let file = self
            .target
            .file
            .as_deref()
            .map(|f| format!(" ({f})"))
            .unwrap_or_default();
        let target = format!("on {}/{}{}", self.target.kind, self.target.name, file);
        match self.op {
            PatchOp::Remove => format!("remove {} {}", self.path, target),
            PatchOp::Add => format!(
                "add {} = {} {}",
                self.path,
                self.value
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                target
            ),
            PatchOp::Replace => format!(
                "replace {} = {} {}",
                self.path,
                self.value
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                target
            ),
        }
    }

    /// A one-line description for lists and logs.
    pub fn describe(&self) -> String {
        let file = self
            .target
            .file
            .as_deref()
            .map(|f| format!("{f}:"))
            .unwrap_or_default();
        match self.op {
            PatchOp::Remove => format!(
                "{} {}{}/{} remove {}",
                self.durability.as_str(),
                file,
                self.target.kind,
                self.target.name,
                self.path
            ),
            op => format!(
                "{} {}{}/{} {} {} = {}",
                self.durability.as_str(),
                file,
                self.target.kind,
                self.target.name,
                match op {
                    PatchOp::Add => "add",
                    _ => "replace",
                },
                self.path,
                self.value
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            ),
        }
    }

    fn matches(&self, file: Option<&str>, manifest: &serde_json::Value) -> bool {
        let kind = manifest.get("kind").and_then(|k| k.as_str());
        let name = manifest
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(|n| n.as_str());
        kind == Some(self.target.kind.as_str())
            && name == Some(self.target.name.as_str())
            && self.target.file.as_deref().is_none_or(|f| Some(f) == file)
    }

    fn operation(&self) -> AppResult<json_patch::PatchOperation> {
        let mut op = serde_json::json!({ "op": match self.op {
            PatchOp::Add => "add",
            PatchOp::Replace => "replace",
            PatchOp::Remove => "remove",
        }, "path": self.path });
        if self.op != PatchOp::Remove {
            let value = self.value.clone().ok_or_else(|| {
                AppError::InvalidInput(format!("patch {} needs a value", self.describe()))
            })?;
            op["value"] = value;
        }
        serde_json::from_value(op).map_err(|e| {
            AppError::InvalidInput(format!("patch {} is malformed: {e}", self.describe()))
        })
    }
}

/// A pending edit to a config's patch list, carried by a deploy: existing
/// patches to drop (by position in the current list) and new ones to add.
/// Nothing is written until the deploy runs; the form keeps this in the
/// query string.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PatchChanges {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<ManifestPatch>,
}

impl PatchChanges {
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.add.is_empty()
    }

    pub fn removes(&self, index: usize) -> bool {
        self.remove.contains(&index)
    }

    /// These edits with position `index` of the current list marked removed.
    pub fn removing(&self, index: usize) -> Self {
        let mut out = self.clone();
        if !out.remove.contains(&index) {
            out.remove.push(index);
        }
        out
    }

    /// These edits with position `index` kept after all.
    pub fn keeping(&self, index: usize) -> Self {
        let mut out = self.clone();
        out.remove.retain(|r| *r != index);
        out
    }

    /// These edits without the `index`th addition.
    pub fn without_addition(&self, index: usize) -> Self {
        let mut out = self.clone();
        if index < out.add.len() {
            out.add.remove(index);
        }
        out
    }

    /// The list a deploy would leave: `current` without the removed
    /// positions, then the additions, each stamped with the deploy's
    /// durability and the time of the deploy.
    pub fn apply_to(
        &self,
        current: &[ManifestPatch],
        durability: Durability,
    ) -> Vec<ManifestPatch> {
        let mut out: Vec<ManifestPatch> = current
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.removes(*i))
            .map(|(_, p)| p.clone())
            .collect();
        for patch in &self.add {
            let mut patch = patch.clone();
            patch.durability = durability;
            patch.since = Some(chrono::Utc::now().to_rfc3339());
            out.push(patch);
        }
        out
    }

    /// Parse the query-string form; empty or absent is no change, and
    /// anything unparseable is refused rather than silently ignored.
    pub fn from_json(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(PatchChanges::default());
        }
        serde_json::from_str(raw).map_err(|e| format!("pending patches are not readable: {e}"))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Apply every patch to the rendered manifests, in order. `rendered` is
/// paired with the templates it came from so `file` can be matched.
pub fn apply_patches(
    templates: &[Template],
    mut rendered: Vec<serde_json::Value>,
    patches: &[ManifestPatch],
) -> AppResult<Vec<serde_json::Value>> {
    for patch in patches {
        let matching: Vec<usize> = rendered
            .iter()
            .enumerate()
            .filter(|(i, m)| patch.matches(templates.get(*i).and_then(|t| t.file.as_deref()), m))
            .map(|(i, _)| i)
            .collect();
        let index = match matching.as_slice() {
            [one] => *one,
            [] => {
                return Err(AppError::InvalidInput(format!(
                    "patch {} matches no manifest",
                    patch.describe()
                )))
            }
            _ => {
                return Err(AppError::InvalidInput(format!(
                    "patch {} matches {} manifests; add a file to the target",
                    patch.describe(),
                    matching.len()
                )))
            }
        };
        let op = patch.operation()?;
        json_patch::patch(&mut rendered[index], &[op]).map_err(|e| {
            AppError::InvalidInput(format!("patch {} failed to apply: {e}", patch.describe()))
        })?;
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deployment(name: &str) -> serde_json::Value {
        json!({"kind": "Deployment", "metadata": {"name": name},
               "spec": {"replicas": 1, "template": {"spec": {"containers": [{"name": "app", "env": []}]}}}})
    }

    fn templates(files: &[(&str, serde_json::Value)]) -> Vec<Template> {
        files
            .iter()
            .map(|(f, m)| Template {
                file: Some((*f).to_string()),
                manifest: m.clone(),
            })
            .collect()
    }

    fn patch(
        kind: &str,
        name: &str,
        op: PatchOp,
        path: &str,
        value: Option<serde_json::Value>,
    ) -> ManifestPatch {
        ManifestPatch {
            target: PatchTarget {
                file: None,
                kind: kind.into(),
                name: name.into(),
            },
            op,
            path: path.into(),
            value,
            durability: Durability::Temporary,
            note: None,
            by: None,
            since: None,
        }
    }

    #[test]
    fn applies_add_replace_remove_to_the_matching_manifest() -> AppResult<()> {
        let t = templates(&[
            ("deployment.yaml", deployment("web")),
            ("other.yaml", deployment("worker")),
        ]);
        let rendered = t.iter().map(|x| x.manifest.clone()).collect();
        let patches = vec![
            patch(
                "Deployment",
                "web",
                PatchOp::Replace,
                "/spec/replicas",
                Some(json!(5)),
            ),
            patch(
                "Deployment",
                "web",
                PatchOp::Add,
                "/spec/template/spec/containers/0/env/-",
                Some(json!({"name": "RUST_LOG", "value": "debug"})),
            ),
            patch(
                "Deployment",
                "worker",
                PatchOp::Remove,
                "/spec/replicas",
                None,
            ),
        ];
        let out = apply_patches(&t, rendered, &patches)?;
        assert_eq!(out[0]["spec"]["replicas"], 5);
        assert_eq!(
            out[0]["spec"]["template"]["spec"]["containers"][0]["env"][0]["name"],
            "RUST_LOG"
        );
        assert!(out[1]["spec"].get("replicas").is_none());
        Ok(())
    }

    #[test]
    fn failures_are_loud() {
        let t = templates(&[("a.yaml", deployment("web")), ("b.yaml", deployment("web"))]);
        let rendered: Vec<_> = t.iter().map(|x| x.manifest.clone()).collect();
        // Ambiguous without a file.
        let p = patch(
            "Deployment",
            "web",
            PatchOp::Replace,
            "/spec/replicas",
            Some(json!(2)),
        );
        assert!(apply_patches(&t, rendered.clone(), std::slice::from_ref(&p)).is_err());
        // Narrowed by file it works.
        let mut narrowed = p.clone();
        narrowed.target.file = Some("b.yaml".into());
        let out = apply_patches(&t, rendered.clone(), &[narrowed]).unwrap_or_default();
        assert_eq!(out[1]["spec"]["replicas"], 2);
        assert_eq!(out[0]["spec"]["replicas"], 1);
        // No such manifest.
        let missing = patch(
            "Deployment",
            "nope",
            PatchOp::Replace,
            "/spec/replicas",
            Some(json!(2)),
        );
        assert!(apply_patches(&t, rendered.clone(), &[missing]).is_err());
        // Bad path.
        let bad = patch(
            "Deployment",
            "web",
            PatchOp::Replace,
            "/spec/nothing/here",
            Some(json!(2)),
        );
        let mut bad_narrowed = bad;
        bad_narrowed.target.file = Some("a.yaml".into());
        assert!(apply_patches(&t, rendered.clone(), &[bad_narrowed]).is_err());
        // Add without a value.
        let mut no_value = patch("Deployment", "web", PatchOp::Add, "/spec/x", None);
        no_value.target.file = Some("a.yaml".into());
        assert!(apply_patches(&t, rendered, &[no_value]).is_err());
    }

    #[test]
    fn pending_changes_apply_at_deploy_time() {
        let current = vec![
            patch(
                "Deployment",
                "web",
                PatchOp::Replace,
                "/spec/replicas",
                Some(json!(2)),
            ),
            patch("Deployment", "web", PatchOp::Remove, "/spec/x", None),
        ];
        let mut added = patch(
            "Deployment",
            "web",
            PatchOp::Replace,
            "/spec/replicas",
            Some(json!(3)),
        );
        added.since = None;
        let changes = PatchChanges {
            remove: vec![0],
            add: vec![added],
        };
        assert!(!changes.is_empty() && changes.removes(0) && !changes.removes(1));
        let out = changes.apply_to(&current, Durability::Standing);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, "/spec/x", "the kept one comes first");
        assert_eq!(out[1].value, Some(json!(3)));
        assert_eq!(
            out[1].durability,
            Durability::Standing,
            "additions take the deploy's durability"
        );
        assert!(out[1].since.is_some());

        assert_eq!(changes.removing(0).remove, vec![0], "no duplicates");
        assert_eq!(changes.removing(1).remove, vec![0, 1]);
        assert!(changes.keeping(0).remove.is_empty());
        assert!(changes.without_addition(0).add.is_empty());
        assert_eq!(changes.without_addition(5), changes);
        let round = PatchChanges::from_json(&changes.to_json()).unwrap_or_default();
        assert_eq!(round, changes);
        assert!(PatchChanges::from_json("  ").unwrap_or_default().is_empty());
        assert!(PatchChanges::from_json("{nope").is_err());
        assert!(PatchChanges::default().apply_to(&current, Durability::Temporary) == current);
    }

    #[test]
    fn describe_op_reads_like_the_deploy_page() {
        let mut p = patch(
            "Deployment",
            "web",
            PatchOp::Replace,
            "/spec/replicas",
            Some(json!(3)),
        );
        assert_eq!(
            p.describe_op(),
            "replace /spec/replicas = 3 on Deployment/web"
        );
        p.target.file = Some("deployment.yaml".into());
        assert_eq!(
            p.describe_op(),
            "replace /spec/replicas = 3 on Deployment/web (deployment.yaml)"
        );
        let r = patch("Deployment", "web", PatchOp::Remove, "/spec/x", None);
        assert_eq!(r.describe_op(), "remove /spec/x on Deployment/web");
    }

    #[test]
    fn serde_shape() -> Result<(), serde_json::Error> {
        let json = json!({"target": {"kind": "Deployment", "name": "web"}, "op": "replace", "path": "/spec/replicas", "value": 3, "durability": "temporary", "note": "load test"});
        let p: ManifestPatch = serde_json::from_value(json.clone())?;
        assert!(p.is_temporary());
        assert_eq!(serde_json::to_value(&p)?, json);
        Ok(())
    }
}
