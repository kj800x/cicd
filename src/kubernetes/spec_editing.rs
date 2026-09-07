use std::collections::{BTreeMap, BTreeSet};

use kube::api::DynamicObject;

/// `$NAME` tokens: an uppercase identifier after a dollar sign.
fn parameter_token_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    #[allow(clippy::expect_used)]
    RE.get_or_init(|| {
        regex::Regex::new(r"\$([A-Z][A-Z0-9_]*)").expect("valid parameter token regex")
    })
}

/// Substitute `$NAME` with the parameter's value wherever a string mentions
/// it. Tokens that are not in `values` are left exactly as they are: a
/// manifest may legitimately contain `$HOME` or `$(POD_NAME)` for a shell
/// or the kubelet to expand, and only declared parameters are ours.
pub trait WithParameters {
    fn with_parameters(&self, values: &BTreeMap<String, String>) -> Self;
}

/// Every `$NAME` token mentioned anywhere in a value.
pub fn referenced_parameters(value: &serde_json::Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect_tokens(value, &mut out);
    out
}

fn collect_tokens(value: &serde_json::Value, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => map.values().for_each(|v| collect_tokens(v, out)),
        serde_json::Value::Array(arr) => arr.iter().for_each(|v| collect_tokens(v, out)),
        serde_json::Value::String(s) => {
            for cap in parameter_token_regex().captures_iter(s) {
                if let Some(name) = cap.get(1) {
                    out.insert(name.as_str().to_string());
                }
            }
        }
        _ => {}
    }
}

fn substitute(s: &str, values: &BTreeMap<String, String>) -> String {
    parameter_token_regex()
        .replace_all(s, |cap: &regex::Captures| {
            let name = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
            match values.get(name) {
                Some(v) => v.clone(),
                None => cap
                    .get(0)
                    .map(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string(),
            }
        })
        .into_owned()
}

impl WithParameters for serde_json::Value {
    fn with_parameters(&self, values: &BTreeMap<String, String>) -> Self {
        match self {
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), v.with_parameters(values)))
                    .collect(),
            ),
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(|v| v.with_parameters(values)).collect())
            }
            serde_json::Value::String(s) => serde_json::Value::String(substitute(s, values)),
            _ => self.clone(),
        }
    }
}

impl WithParameters for DynamicObject {
    fn with_parameters(&self, values: &BTreeMap<String, String>) -> Self {
        let mut obj = self.clone();
        obj.data = obj.data.with_parameters(values);
        obj
    }
}

pub trait WithInjectedEnv {
    /// Upserts the given `(name, value)` environment variables into the `env`
    /// array of every container found in the spec.
    fn with_injected_env(&self, vars: &[(String, String)]) -> Self;
}

/// Upsert a single env var into a container's `env` array (by name), so we never
/// create duplicate entries and always win over a pre-existing literal value.
fn upsert_env(env_arr: &mut Vec<serde_json::Value>, name: &str, value: &str) {
    let existing = env_arr
        .iter_mut()
        .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(name));

    match existing {
        Some(serde_json::Value::Object(obj)) => {
            obj.insert(
                "value".to_string(),
                serde_json::Value::String(value.to_string()),
            );
            // Our literal value supersedes any prior valueFrom reference.
            obj.remove("valueFrom");
        }
        _ => {
            env_arr.push(serde_json::json!({ "name": name, "value": value }));
        }
    }
}

/// Inject env vars into each container object of a `containers`/`initContainers` array.
fn inject_env_into_containers(
    containers: &serde_json::Value,
    vars: &[(String, String)],
) -> serde_json::Value {
    let serde_json::Value::Array(arr) = containers else {
        return containers.clone();
    };

    let new_arr = arr
        .iter()
        .map(|container| {
            let serde_json::Value::Object(obj) = container else {
                return container.clone();
            };
            let mut obj = obj.clone();
            let mut env_arr: Vec<serde_json::Value> = match obj.get("env") {
                Some(serde_json::Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            for (name, value) in vars {
                upsert_env(&mut env_arr, name, value);
            }
            obj.insert("env".to_string(), serde_json::Value::Array(env_arr));
            serde_json::Value::Object(obj)
        })
        .collect();

    serde_json::Value::Array(new_arr)
}

impl WithInjectedEnv for serde_json::Value {
    fn with_injected_env(&self, vars: &[(String, String)]) -> Self {
        match self {
            serde_json::Value::Object(json) => {
                let mut new_json = serde_json::Map::new();
                for (key, value) in json {
                    let new_value =
                        if (key == "containers" || key == "initContainers") && value.is_array() {
                            inject_env_into_containers(value, vars)
                        } else {
                            value.with_injected_env(vars)
                        };
                    new_json.insert(key.clone(), new_value);
                }
                serde_json::Value::Object(new_json)
            }
            serde_json::Value::Array(array) => {
                serde_json::Value::Array(array.iter().map(|v| v.with_injected_env(vars)).collect())
            }
            _ => self.clone(),
        }
    }
}

impl WithInjectedEnv for DynamicObject {
    fn with_injected_env(&self, vars: &[(String, String)]) -> Self {
        let mut obj = self.clone();
        obj.data = obj.data.with_injected_env(vars);
        obj
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars() -> Vec<(String, String)> {
        vec![
            ("CICD_NON_LATEST_DEPLOY".to_string(), "true".to_string()),
            ("CICD_ARTIFACT_SHA".to_string(), "abc123".to_string()),
        ]
    }

    /// Collect the env arrays of all containers/initContainers found anywhere.
    fn all_env_arrays(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, v) in map {
                    if (key == "containers" || key == "initContainers") && v.is_array() {
                        for container in v.as_array().into_iter().flatten() {
                            if let Some(env) = container.get("env") {
                                out.push(env.clone());
                            }
                        }
                    }
                    all_env_arrays(v, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    all_env_arrays(v, out);
                }
            }
            _ => {}
        }
    }

    fn env_value(env: &serde_json::Value, name: &str) -> Option<String> {
        env.as_array()?
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(name))?
            .get("value")?
            .as_str()
            .map(|s| s.to_string())
    }

    #[test]
    fn substitutes_declared_parameters_and_leaves_the_rest() {
        let spec = json!({
            "image": "ghcr.io/x/app:commit-$SHA",
            "args": ["--nginx=$NGINX", "--home=$HOME", "$(POD_NAME)", "$SHA_SUFFIX"],
            "replicas": "$REPLICAS",
            "nested": { "list": [ { "v": "$SHA-$NGINX" } ] }
        });
        let values = BTreeMap::from([
            ("SHA".to_string(), "abc".to_string()),
            ("NGINX".to_string(), "1.25".to_string()),
        ]);
        let out = spec.with_parameters(&values);
        assert_eq!(out["image"], "ghcr.io/x/app:commit-abc");
        assert_eq!(out["args"][0], "--nginx=1.25");
        assert_eq!(
            out["args"][1], "--home=$HOME",
            "undeclared tokens are untouched"
        );
        assert_eq!(out["args"][2], "$(POD_NAME)");
        assert_eq!(
            out["args"][3], "$SHA_SUFFIX",
            "a longer name is not a prefix match"
        );
        assert_eq!(
            out["replicas"], "$REPLICAS",
            "declared-looking but absent stays"
        );
        assert_eq!(out["nested"]["list"][0]["v"], "abc-1.25");

        let refs = referenced_parameters(&spec);
        assert_eq!(
            refs.into_iter().collect::<Vec<_>>(),
            vec!["HOME", "NGINX", "REPLICAS", "SHA", "SHA_SUFFIX"]
        );
    }

    #[test]
    fn injects_into_deployment_container() {
        let spec = json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": { "template": { "spec": { "containers": [
                { "name": "app", "image": "app:latest" }
            ] } } }
        });

        let out = spec.with_injected_env(&vars());
        let mut envs = Vec::new();
        all_env_arrays(&out, &mut envs);

        assert_eq!(envs.len(), 1);
        assert_eq!(
            env_value(&envs[0], "CICD_NON_LATEST_DEPLOY").as_deref(),
            Some("true")
        );
        assert_eq!(
            env_value(&envs[0], "CICD_ARTIFACT_SHA").as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn injects_into_cronjob_nested_template() {
        let spec = json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "spec": { "jobTemplate": { "spec": { "template": { "spec": { "containers": [
                { "name": "job", "image": "job:latest" }
            ] } } } } }
        });

        let out = spec.with_injected_env(&vars());
        let mut envs = Vec::new();
        all_env_arrays(&out, &mut envs);

        assert_eq!(envs.len(), 1);
        assert_eq!(
            env_value(&envs[0], "CICD_NON_LATEST_DEPLOY").as_deref(),
            Some("true")
        );
    }

    #[test]
    fn injects_into_init_containers() {
        let spec = json!({
            "spec": { "template": { "spec": {
                "initContainers": [ { "name": "migrate", "image": "app:latest" } ],
                "containers": [ { "name": "app", "image": "app:latest" } ]
            } } }
        });

        let out = spec.with_injected_env(&vars());
        let mut envs = Vec::new();
        all_env_arrays(&out, &mut envs);

        // Both the init container and the app container get the vars.
        assert_eq!(envs.len(), 2);
        for env in &envs {
            assert_eq!(
                env_value(env, "CICD_ARTIFACT_SHA").as_deref(),
                Some("abc123")
            );
        }
    }

    #[test]
    fn upserts_preserving_other_vars_and_overwriting_collisions() {
        let spec = json!({
            "spec": { "template": { "spec": { "containers": [ {
                "name": "app",
                "image": "app:latest",
                "env": [
                    { "name": "EXISTING", "value": "keep" },
                    { "name": "CICD_ARTIFACT_SHA", "value": "stale" }
                ]
            } ] } } }
        });

        let out = spec.with_injected_env(&vars());
        let mut envs = Vec::new();
        all_env_arrays(&out, &mut envs);

        let env = &envs[0];
        // Pre-existing unrelated var is preserved.
        assert_eq!(env_value(env, "EXISTING").as_deref(), Some("keep"));
        // Colliding var is overwritten, not duplicated.
        assert_eq!(
            env_value(env, "CICD_ARTIFACT_SHA").as_deref(),
            Some("abc123")
        );
        let count = env
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e.get("name").and_then(|n| n.as_str()) == Some("CICD_ARTIFACT_SHA"))
            .count();
        assert_eq!(count, 1);
        // New var added.
        assert_eq!(
            env_value(env, "CICD_NON_LATEST_DEPLOY").as_deref(),
            Some("true")
        );
    }

    #[test]
    fn valuefrom_collision_is_replaced_with_literal() {
        let spec = json!({
            "spec": { "template": { "spec": { "containers": [ {
                "name": "app",
                "env": [
                    { "name": "CICD_ARTIFACT_SHA", "valueFrom": { "secretKeyRef": { "name": "s", "key": "k" } } }
                ]
            } ] } } }
        });

        let out = spec.with_injected_env(&vars());
        let mut envs = Vec::new();
        all_env_arrays(&out, &mut envs);

        let entry = envs[0]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("CICD_ARTIFACT_SHA"))
            .unwrap();
        assert_eq!(entry.get("value").and_then(|v| v.as_str()), Some("abc123"));
        assert!(entry.get("valueFrom").is_none());
    }
}
