//! What a bounce restarts.
//!
//! A bounce is `kubectl rollout restart` for every workload a config owns:
//! the pod template gets a `kubectl.kubernetes.io/restartedAt` annotation
//! and the workload controller rolls the pods. That works the same way for
//! Deployments, StatefulSets and DaemonSets, but the live object decides
//! whether anything actually happens:
//!
//! - A workload scaled to zero has no pods to restart.
//! - A StatefulSet or DaemonSet with the `OnDelete` update strategy ignores
//!   the annotation; its pods only change when they are deleted. Bounce
//!   skips it and says so rather than reporting a restart that never comes.
//! - A StatefulSet with a `rollingUpdate.partition` only restarts the pods
//!   at or above that ordinal.
//! - A StatefulSet restarts one pod at a time, highest ordinal first,
//!   waiting for each to be Ready, so it is slower than a Deployment.
//!
//! [`plan`] judges one workload from its live object (as JSON) so the
//! preview and the handler agree on what a bounce will do.
use serde_json::Value;

/// The kinds a bounce restarts.
pub const KINDS: &[&str] = &["Deployment", "StatefulSet", "DaemonSet"];

/// Whether a bounce restarts objects of this kind.
pub fn restarts_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// `(kind, name)` of every workload in these manifests a bounce restarts.
pub fn targets(manifests: &[Value]) -> Vec<(String, String)> {
    manifests
        .iter()
        .filter_map(|m| {
            let kind = m.get("kind")?.as_str()?;
            let name = m.get("metadata")?.get("name")?.as_str()?;
            restarts_kind(kind).then(|| (kind.to_string(), name.to_string()))
        })
        .collect()
}

/// What a bounce does to one workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Restarted, with anything worth knowing about how.
    Restart { note: Option<String> },
    /// Left alone, and why.
    Skip(String),
}

impl Outcome {
    pub fn restarts(&self) -> bool {
        matches!(self, Outcome::Restart { .. })
    }
}

/// What a bounce does to the workload `kind`, given its live object.
pub fn plan(kind: &str, live: Option<&Value>) -> Outcome {
    let Some(live) = live else {
        return Outcome::Skip("not in the cluster".to_string());
    };
    let spec = live.get("spec");
    let replicas = spec.and_then(|s| s.get("replicas")).and_then(Value::as_i64);
    if kind != "DaemonSet" && replicas == Some(0) {
        return Outcome::Skip("scaled to zero".to_string());
    }
    let strategy = spec.and_then(|s| s.get("updateStrategy"));
    let strategy_type = strategy.and_then(|s| s.get("type")).and_then(Value::as_str);
    if kind != "Deployment" && strategy_type == Some("OnDelete") {
        return Outcome::Skip(
            "OnDelete update strategy; its pods only restart when deleted".to_string(),
        );
    }
    if kind != "StatefulSet" {
        return Outcome::Restart { note: None };
    }
    let partition = strategy
        .and_then(|s| s.get("rollingUpdate"))
        .and_then(|r| r.get("partition"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let replicas = replicas.unwrap_or(1);
    if partition >= replicas {
        return Outcome::Skip(format!(
            "partition {partition} covers all {replicas} replicas; no pod would restart"
        ));
    }
    let note = if partition > 0 {
        format!(
            "only pods {partition}..{} restart (partition {partition}); one at a time",
            replicas - 1
        )
    } else {
        "one pod at a time, highest ordinal first".to_string()
    };
    Outcome::Restart { note: Some(note) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn targets_keep_restartable_workloads() {
        let manifests = vec![
            json!({"kind": "Deployment", "metadata": {"name": "web"}}),
            json!({"kind": "Service", "metadata": {"name": "web"}}),
            json!({"kind": "StatefulSet", "metadata": {"name": "db"}}),
            json!({"kind": "DaemonSet", "metadata": {"name": "agent"}}),
            json!({"kind": "CronJob", "metadata": {"name": "nightly"}}),
        ];
        assert_eq!(
            targets(&manifests),
            vec![
                ("Deployment".to_string(), "web".to_string()),
                ("StatefulSet".to_string(), "db".to_string()),
                ("DaemonSet".to_string(), "agent".to_string()),
            ]
        );
    }

    #[test]
    fn missing_and_scaled_to_zero_are_skipped() {
        assert!(!plan("Deployment", None).restarts());
        let zero = json!({"spec": {"replicas": 0}});
        assert_eq!(
            plan("StatefulSet", Some(&zero)),
            Outcome::Skip("scaled to zero".to_string())
        );
        assert!(!plan("Deployment", Some(&zero)).restarts());
    }

    #[test]
    fn deployments_and_daemonsets_restart() {
        let d = json!({"spec": {"replicas": 2, "strategy": {"type": "RollingUpdate"}}});
        assert_eq!(
            plan("Deployment", Some(&d)),
            Outcome::Restart { note: None }
        );
        let ds = json!({"spec": {"updateStrategy": {"type": "RollingUpdate"}}});
        assert_eq!(
            plan("DaemonSet", Some(&ds)),
            Outcome::Restart { note: None }
        );
    }

    #[test]
    fn on_delete_is_skipped() {
        let sts = json!({"spec": {"replicas": 1, "updateStrategy": {"type": "OnDelete"}}});
        assert!(!plan("StatefulSet", Some(&sts)).restarts());
        let ds = json!({"spec": {"updateStrategy": {"type": "OnDelete"}}});
        assert!(!plan("DaemonSet", Some(&ds)).restarts());
    }

    #[test]
    fn statefulsets_note_ordering_and_partition() {
        let sts = json!({"spec": {"replicas": 3, "updateStrategy": {"type": "RollingUpdate"}}});
        assert_eq!(
            plan("StatefulSet", Some(&sts)),
            Outcome::Restart {
                note: Some("one pod at a time, highest ordinal first".to_string())
            }
        );
        let part = json!({"spec": {"replicas": 3, "updateStrategy": {
            "type": "RollingUpdate", "rollingUpdate": {"partition": 1}}}});
        assert_eq!(
            plan("StatefulSet", Some(&part)),
            Outcome::Restart {
                note: Some("only pods 1..2 restart (partition 1); one at a time".to_string())
            }
        );
        let all = json!({"spec": {"replicas": 2, "updateStrategy": {
            "type": "RollingUpdate", "rollingUpdate": {"partition": 2}}}});
        assert!(!plan("StatefulSet", Some(&all)).restarts());
    }
}
