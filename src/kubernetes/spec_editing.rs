use kube::api::DynamicObject;

pub trait WithInterpolatedVersion {
    fn with_interpolated_version(&self, version: &str) -> Self;
}

pub trait WithVersion {
    fn with_version(&self, version: &str) -> Self;
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

impl WithInterpolatedVersion for serde_json::Value {
    fn with_interpolated_version(&self, version: &str) -> Self {
        match self {
            serde_json::Value::Object(json) => {
                let mut new_json = serde_json::Map::new();
                for (key, value) in json {
                    new_json.insert(key.clone(), value.with_interpolated_version(version));
                }
                serde_json::Value::Object(new_json)
            }
            serde_json::Value::Array(array) => {
                let mut new_array = Vec::new();
                for value in array {
                    new_array.push(value.with_interpolated_version(version));
                }
                serde_json::Value::Array(new_array)
            }
            serde_json::Value::String(string) => {
                serde_json::Value::String(string.replace("$SHA", version))
            }
            _ => self.clone(),
        }
    }
}

impl WithInterpolatedVersion for serde_json::Map<String, serde_json::Value> {
    fn with_interpolated_version(&self, version: &str) -> Self {
        #[allow(clippy::expect_used)]
        serde_json::Value::Object(self.clone())
            .with_interpolated_version(version)
            .as_object()
            .expect("with_interpolated_version should return an object")
            .clone()
    }
}

impl WithVersion for DynamicObject {
    /// Interpolates the data with the given version
    fn with_version(&self, version: &str) -> Self {
        let mut obj = self.clone();

        obj.data = obj.data.with_interpolated_version(version);
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
