//! Whether a config's deploy has rolled out, judged from the objects in
//! its namespace.
//!
//! The rule is generic, in the spirit of kstatus rather than per resource
//! kind:
//!
//! - Every object the config renders must exist, and every field the
//!   render sets must have the same value on the live object: the
//!   controller has applied this deploy. Fields the cluster fills in
//!   (defaults, status) are not in the render and are ignored. Server-side
//!   apply stores what it was sent, so on this cluster every config
//!   matches its render exactly once applied (checked 2026-10-17: 155 of
//!   155 objects). A field the API server rewrites (`cpu: 0.5` stored as
//!   `500m`) would keep a run waiting; the message names the field.
//! - Deployments, StatefulSets and DaemonSets are done when the workload
//!   controller has observed the new spec and every replica is updated and
//!   available. A Deployment that reports `ProgressDeadlineExceeded`, or a
//!   new pod that is crash-looping or cannot pull its image, has failed.
//! - A Job is done when it completed and failed when it failed.
//! - Any other object with a `Ready` condition (OnePasswordItem has one)
//!   is done when it is `Ready=True`.
//! - Everything else (Services, Ingresses, ConfigMaps, JobTemplates) is
//!   done once applied.
//!
//! The deadline is not judged here: the caller knows how long the run has
//! been going.
use kube::api::DynamicObject;
use kube::ResourceExt;
use serde_json::Value;

/// Container waiting reasons that mean a new pod will not come up on its
/// own. Transient ones (`ErrImagePull`, `ContainerCreating`) are left out:
/// they either resolve or turn into one of these.
const FATAL_WAITING_REASONS: &[&str] = &[
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "InvalidImageName",
    "CreateContainerConfigError",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rollout {
    Done,
    /// Still going; the message says what it is waiting for.
    Waiting(String),
    /// Will not finish on its own; the message says why.
    Failed(String),
}

/// Judge one deploy. `desired` is the config's freshly rendered children,
/// `live` every object in their namespace (pods and ReplicaSets included).
/// The first failure wins; otherwise the first thing still waiting is
/// reported, with a count of the rest.
pub fn evaluate(desired: &[DynamicObject], live: &[DynamicObject]) -> Rollout {
    let mut waiting: Vec<String> = Vec::new();
    for want in desired {
        match evaluate_one(want, live) {
            Rollout::Done => {}
            Rollout::Waiting(message) => waiting.push(message),
            failed @ Rollout::Failed(_) => return failed,
        }
    }
    match waiting.len() {
        0 => Rollout::Done,
        1 => Rollout::Waiting(waiting.remove(0)),
        n => Rollout::Waiting(format!("{} (and {} more)", waiting[0], n - 1)),
    }
}

fn kind(obj: &DynamicObject) -> &str {
    obj.types.as_ref().map(|t| t.kind.as_str()).unwrap_or("")
}

fn group(obj: &DynamicObject) -> &str {
    obj.types
        .as_ref()
        .and_then(|t| t.api_version.rsplit_once('/'))
        .map(|(group, _)| group)
        .unwrap_or("")
}

fn label(obj: &DynamicObject) -> String {
    format!("{}/{}", kind(obj), obj.name_any())
}

fn annotation<'a>(obj: &'a DynamicObject, key: &str) -> Option<&'a str> {
    obj.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(key))
        .map(String::as_str)
}

fn int(value: &Value, path: &[&str]) -> Option<i64> {
    path.iter()
        .try_fold(value, |v, key| v.get(key))
        .and_then(Value::as_i64)
}

fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |v, key| v.get(key))
        .and_then(Value::as_str)
}

/// The condition of type `type_` under `status.conditions`, if present.
fn condition<'a>(obj: &'a DynamicObject, type_: &str) -> Option<&'a Value> {
    obj.data
        .get("status")?
        .get("conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(type_))
}

fn condition_is(c: &Value, status: &str) -> bool {
    c.get("status").and_then(Value::as_str) == Some(status)
}

fn find<'a>(want: &DynamicObject, live: &'a [DynamicObject]) -> Option<&'a DynamicObject> {
    live.iter().find(|o| {
        kind(o) == kind(want)
            && group(o) == group(want)
            && o.name_any() == want.name_any()
            && (want.metadata.namespace.is_none()
                || o.metadata.namespace == want.metadata.namespace)
    })
}

fn evaluate_one(want: &DynamicObject, live: &[DynamicObject]) -> Rollout {
    let Some(obj) = find(want, live) else {
        return Rollout::Waiting(format!("waiting for {} to be created", label(want)));
    };
    if let Some(difference) = first_difference(want, obj) {
        return Rollout::Waiting(format!(
            "waiting for {} to be applied: {difference}",
            label(want)
        ));
    }
    let generation = obj.metadata.generation.unwrap_or(0);
    let observed = int(&obj.data, &["status", "observedGeneration"]);
    let is_workload = matches!(group(obj), "apps")
        && matches!(kind(obj), "Deployment" | "StatefulSet" | "DaemonSet");
    if is_workload && observed.unwrap_or(0) < generation {
        return Rollout::Waiting(format!("waiting for {} to see the new spec", label(obj)));
    }
    match (group(obj), kind(obj)) {
        ("apps", "Deployment") => deployment(obj, live),
        ("apps", "StatefulSet") => statefulset(obj, live),
        ("apps", "DaemonSet") => daemonset(obj, live),
        ("batch", "Job") => job(obj),
        _ => match condition(obj, "Ready") {
            Some(_) if observed.is_some_and(|o| o < generation) => {
                Rollout::Waiting(format!("waiting for {} to see the new spec", label(obj)))
            }
            Some(ready) if !condition_is(ready, "True") => Rollout::Waiting(format!(
                "waiting for {} to be Ready{}",
                label(obj),
                ready
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default()
            )),
            _ => Rollout::Done,
        },
    }
}

/// The first field the render sets that the live object does not have
/// with the same value, as `path is live, want desired`. Maps are compared
/// key by key, so anything the cluster added is ignored; lists element by
/// element, and must be the same length.
fn first_difference(want: &DynamicObject, live: &DynamicObject) -> Option<String> {
    let mut want = serde_json::to_value(want).ok()?;
    // Status is the cluster's to write; a manifest that carries one cannot
    // expect it to stick.
    if let Some(map) = want.as_object_mut() {
        map.remove("status");
    }
    let live = serde_json::to_value(live).ok()?;
    difference_at(&want, &live, "")
}

fn difference_at(want: &Value, live: &Value, path: &str) -> Option<String> {
    match (want, live) {
        (Value::Object(w), Value::Object(l)) => w.iter().find_map(|(key, wv)| {
            let path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            match l.get(key) {
                Some(lv) => difference_at(wv, lv, &path),
                None => Some(format!("{path} is missing, want {}", shown(wv))),
            }
        }),
        (Value::Array(w), Value::Array(l)) if w.len() != l.len() => {
            Some(format!("{path} has {} items, want {}", l.len(), w.len()))
        }
        (Value::Array(w), Value::Array(l)) => w
            .iter()
            .zip(l)
            .enumerate()
            .find_map(|(i, (wv, lv))| difference_at(wv, lv, &format!("{path}[{i}]"))),
        (w, l) if w == l => None,
        (w, l) => Some(format!("{path} is {}, want {}", shown(l), shown(w))),
    }
}

/// A JSON value for a message, cut short.
fn shown(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= 60 {
        return text;
    }
    let cut: String = text.chars().take(59).collect();
    format!("{cut}…")
}

fn owned_by<'a>(
    owner: &DynamicObject,
    live: &'a [DynamicObject],
    kind_: &'a str,
) -> impl Iterator<Item = &'a DynamicObject> + 'a {
    let uid = owner.uid();
    live.iter().filter(move |o| {
        kind(o) == kind_
            && uid.is_some()
            && o.metadata
                .owner_references
                .as_ref()
                .is_some_and(|refs| refs.iter().any(|r| Some(&r.uid) == uid.as_ref()))
    })
}

/// The first new pod that is stuck in a way it will not recover from.
fn stuck_pod<'a>(pods: impl Iterator<Item = &'a DynamicObject>) -> Option<String> {
    for pod in pods {
        let status = pod.data.get("status");
        let containers = ["initContainerStatuses", "containerStatuses"]
            .iter()
            .filter_map(|key| status?.get(key)?.as_array())
            .flatten();
        for container in containers {
            let Some(reason) = str_at(container, &["state", "waiting", "reason"]) else {
                continue;
            };
            if FATAL_WAITING_REASONS.contains(&reason) {
                let name = str_at(container, &["name"]).unwrap_or("?");
                return Some(format!(
                    "pod {} container {} is {}",
                    pod.name_any(),
                    name,
                    reason
                ));
            }
        }
    }
    None
}

fn deployment(obj: &DynamicObject, live: &[DynamicObject]) -> Rollout {
    let progressing = condition(obj, "Progressing");
    if progressing.is_some_and(|c| {
        condition_is(c, "False")
            && c.get("reason").and_then(Value::as_str) == Some("ProgressDeadlineExceeded")
    }) {
        return Rollout::Failed(format!("{} exceeded its progress deadline", label(obj)));
    }

    // The new pods are those of the ReplicaSet carrying the Deployment's
    // current revision.
    let revision = annotation(obj, "deployment.kubernetes.io/revision");
    let new_pods = owned_by(obj, live, "ReplicaSet")
        .filter(|rs| {
            revision.is_some() && annotation(rs, "deployment.kubernetes.io/revision") == revision
        })
        .flat_map(|rs| owned_by(rs, live, "Pod"));
    if let Some(stuck) = stuck_pod(new_pods) {
        return Rollout::Failed(format!("{}: {}", label(obj), stuck));
    }

    let wanted = int(&obj.data, &["spec", "replicas"]).unwrap_or(1);
    let updated = int(&obj.data, &["status", "updatedReplicas"]).unwrap_or(0);
    let total = int(&obj.data, &["status", "replicas"]).unwrap_or(0);
    let available = int(&obj.data, &["status", "availableReplicas"]).unwrap_or(0);
    if updated < wanted {
        return Rollout::Waiting(format!(
            "{}: {updated} of {wanted} replicas updated",
            label(obj)
        ));
    }
    if total > updated {
        return Rollout::Waiting(format!(
            "{}: {} old replicas still terminating",
            label(obj),
            total - updated
        ));
    }
    if available < updated {
        return Rollout::Waiting(format!(
            "{}: {available} of {updated} updated replicas available",
            label(obj)
        ));
    }
    Rollout::Done
}

fn statefulset(obj: &DynamicObject, live: &[DynamicObject]) -> Rollout {
    let update_revision = str_at(&obj.data, &["status", "updateRevision"]);
    let new_pods = owned_by(obj, live, "Pod").filter(|pod| {
        update_revision.is_some()
            && pod
                .labels()
                .get("controller-revision-hash")
                .map(String::as_str)
                == update_revision
    });
    if let Some(stuck) = stuck_pod(new_pods) {
        return Rollout::Failed(format!("{}: {}", label(obj), stuck));
    }
    let wanted = int(&obj.data, &["spec", "replicas"]).unwrap_or(1);
    let updated = int(&obj.data, &["status", "updatedReplicas"]).unwrap_or(0);
    let ready = int(&obj.data, &["status", "readyReplicas"]).unwrap_or(0);
    if updated < wanted || update_revision != str_at(&obj.data, &["status", "currentRevision"]) {
        return Rollout::Waiting(format!(
            "{}: {updated} of {wanted} replicas updated",
            label(obj)
        ));
    }
    if ready < wanted {
        return Rollout::Waiting(format!(
            "{}: {ready} of {wanted} replicas ready",
            label(obj)
        ));
    }
    Rollout::Done
}

fn daemonset(obj: &DynamicObject, live: &[DynamicObject]) -> Rollout {
    if let Some(stuck) = stuck_pod(owned_by(obj, live, "Pod")) {
        return Rollout::Failed(format!("{}: {}", label(obj), stuck));
    }
    let wanted = int(&obj.data, &["status", "desiredNumberScheduled"]).unwrap_or(0);
    let updated = int(&obj.data, &["status", "updatedNumberScheduled"]).unwrap_or(0);
    let available = int(&obj.data, &["status", "numberAvailable"]).unwrap_or(0);
    if updated < wanted {
        return Rollout::Waiting(format!(
            "{}: {updated} of {wanted} pods updated",
            label(obj)
        ));
    }
    if available < wanted {
        return Rollout::Waiting(format!(
            "{}: {available} of {wanted} pods available",
            label(obj)
        ));
    }
    Rollout::Done
}

fn job(obj: &DynamicObject) -> Rollout {
    if condition(obj, "Complete").is_some_and(|c| condition_is(c, "True")) {
        return Rollout::Done;
    }
    if let Some(failed) = condition(obj, "Failed").filter(|c| condition_is(c, "True")) {
        let why = failed
            .get("message")
            .or_else(|| failed.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        return Rollout::Failed(format!("{} failed: {why}", label(obj)));
    }
    Rollout::Waiting(format!("waiting for {} to complete", label(obj)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(value: Value) -> DynamicObject {
        serde_json::from_value(value).expect("valid object")
    }

    fn deployment_json(image: &str, status: Value) -> Value {
        json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {
                "name": "web",
                "namespace": "ns",
                "uid": "d1",
                "generation": 3,
                "annotations": {"configSha": "c1", "deployment.kubernetes.io/revision": "7"}
            },
            "spec": {
                "replicas": 2,
                "progressDeadlineSeconds": 600,
                "template": {"spec": {"containers": [
                    {"name": "app", "image": image, "imagePullPolicy": "IfNotPresent"}
                ]}}
            },
            "status": status
        })
    }

    fn desired() -> Vec<DynamicObject> {
        vec![obj(json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "ns", "annotations": {"configSha": "c1"}},
            "spec": {
                "replicas": 2,
                "template": {"spec": {"containers": [{"name": "app", "image": "web:2"}]}}
            }
        }))]
    }

    fn rolled_out() -> Value {
        json!({"observedGeneration": 3, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 2})
    }

    #[test]
    fn waits_until_the_controller_applied_this_render() {
        let live = vec![obj(deployment_json("web:1", rolled_out()))];
        assert_eq!(
            evaluate(&desired(), &live),
            Rollout::Waiting(
                "waiting for Deployment/web to be applied: spec.template.spec.containers[0].image is \"web:1\", want \"web:2\"".into()
            )
        );
        // Defaults the cluster added (progressDeadlineSeconds,
        // imagePullPolicy, status) do not count as differences.
        assert_eq!(
            evaluate(&desired(), &[obj(deployment_json("web:2", rolled_out()))]),
            Rollout::Done
        );
        assert_eq!(
            evaluate(&desired(), &[]),
            Rollout::Waiting("waiting for Deployment/web to be created".into())
        );
    }

    #[test]
    fn a_deployment_is_done_when_every_replica_is_new_and_available() {
        let live = vec![obj(deployment_json("web:2", rolled_out()))];
        assert_eq!(evaluate(&desired(), &live), Rollout::Done);

        let stale = json!({"observedGeneration": 2, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 2});
        assert_eq!(
            evaluate(&desired(), &[obj(deployment_json("web:2", stale))]),
            Rollout::Waiting("waiting for Deployment/web to see the new spec".into())
        );
        let surging = json!({"observedGeneration": 3, "replicas": 3, "updatedReplicas": 2, "availableReplicas": 2});
        assert_eq!(
            evaluate(&desired(), &[obj(deployment_json("web:2", surging))]),
            Rollout::Waiting("Deployment/web: 1 old replicas still terminating".into())
        );
        let starting = json!({"observedGeneration": 3, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 1});
        assert_eq!(
            evaluate(&desired(), &[obj(deployment_json("web:2", starting))]),
            Rollout::Waiting("Deployment/web: 1 of 2 updated replicas available".into())
        );
    }

    #[test]
    fn a_deployment_fails_on_its_progress_deadline() {
        let status = json!({
            "observedGeneration": 3, "replicas": 3, "updatedReplicas": 1, "availableReplicas": 2,
            "conditions": [{"type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded"}]
        });
        assert_eq!(
            evaluate(&desired(), &[obj(deployment_json("web:2", status))]),
            Rollout::Failed("Deployment/web exceeded its progress deadline".into())
        );
    }

    #[test]
    fn a_crash_looping_new_pod_fails_but_an_old_one_does_not() {
        let status = json!({"observedGeneration": 3, "replicas": 3, "updatedReplicas": 1, "availableReplicas": 2});
        let rs = |name: &str, uid: &str, revision: &str| {
            obj(json!({
                "apiVersion": "apps/v1", "kind": "ReplicaSet",
                "metadata": {"name": name, "namespace": "ns", "uid": uid,
                    "annotations": {"deployment.kubernetes.io/revision": revision},
                    "ownerReferences": [{"apiVersion": "apps/v1", "kind": "Deployment", "name": "web", "uid": "d1"}]}
            }))
        };
        let pod = |name: &str, rs_uid: &str| {
            obj(json!({
                "apiVersion": "v1", "kind": "Pod",
                "metadata": {"name": name, "namespace": "ns",
                    "ownerReferences": [{"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "rs", "uid": rs_uid}]},
                "status": {"containerStatuses": [{"name": "app", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}
            }))
        };
        let old_only = vec![
            obj(deployment_json("web:2", status.clone())),
            rs("web-old", "rs-old", "6"),
            rs("web-new", "rs-new", "7"),
            pod("web-old-a", "rs-old"),
        ];
        assert!(matches!(
            evaluate(&desired(), &old_only),
            Rollout::Waiting(_)
        ));

        let mut new_too = old_only;
        new_too.push(pod("web-new-a", "rs-new"));
        assert_eq!(
            evaluate(&desired(), &new_too),
            Rollout::Failed(
                "Deployment/web: pod web-new-a container app is CrashLoopBackOff".into()
            )
        );
    }

    #[test]
    fn ready_conditions_jobs_and_plain_objects() {
        let item = |status: &str| {
            obj(json!({
                "apiVersion": "onepassword.com/v1", "kind": "OnePasswordItem",
                "metadata": {"name": "creds", "namespace": "ns"},
                "status": {"conditions": [{"type": "Ready", "status": status, "message": "item not found"}]}
            }))
        };
        let want_item = obj(json!({
            "apiVersion": "onepassword.com/v1", "kind": "OnePasswordItem",
            "metadata": {"name": "creds", "namespace": "ns"}
        }));
        assert_eq!(
            evaluate(&[want_item.clone()], &[item("True")]),
            Rollout::Done
        );
        assert_eq!(
            evaluate(&[want_item], &[item("False")]),
            Rollout::Waiting(
                "waiting for OnePasswordItem/creds to be Ready: item not found".into()
            )
        );

        let service = obj(json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": {"name": "web", "namespace": "ns"}
        }));
        assert_eq!(evaluate(&[service.clone()], &[service]), Rollout::Done);

        let job = |conditions: Value| {
            obj(json!({
                "apiVersion": "batch/v1", "kind": "Job",
                "metadata": {"name": "migrate", "namespace": "ns"},
                "status": {"conditions": conditions}
            }))
        };
        let want_job = job(json!([]));
        assert_eq!(
            evaluate(
                &[want_job.clone()],
                &[job(json!([{"type": "Complete", "status": "True"}]))]
            ),
            Rollout::Done
        );
        assert_eq!(
            evaluate(
                &[want_job.clone()],
                &[job(
                    json!([{"type": "Failed", "status": "True", "message": "BackoffLimitExceeded"}])
                )]
            ),
            Rollout::Failed("Job/migrate failed: BackoffLimitExceeded".into())
        );
        assert_eq!(
            evaluate(&[want_job.clone()], &[want_job]),
            Rollout::Waiting("waiting for Job/migrate to complete".into())
        );
    }

    #[test]
    fn differences_name_the_field() {
        let want = json!({"a": {"b": [1, {"c": "x"}]}, "d": "y"});
        assert_eq!(difference_at(&want, &want, ""), None);
        let extra = json!({"a": {"b": [1, {"c": "x", "z": 0}], "z": 0}, "d": "y", "z": 0});
        assert_eq!(difference_at(&want, &extra, ""), None);
        assert_eq!(
            difference_at(&want, &json!({"a": {"b": [1, {"c": "w"}]}, "d": "y"}), ""),
            Some("a.b[1].c is \"w\", want \"x\"".into())
        );
        assert_eq!(
            difference_at(&want, &json!({"a": {"b": [1]}, "d": "y"}), ""),
            Some("a.b has 1 items, want 2".into())
        );
        assert_eq!(
            difference_at(&want, &json!({"a": {"b": [1, {"c": "x"}]}}), ""),
            Some("d is missing, want \"y\"".into())
        );
        let long = json!("x".repeat(100));
        assert_eq!(shown(&long).chars().count(), 60);
    }

    #[test]
    fn several_waiting_objects_are_counted() {
        let want = |name: &str| {
            obj(
                json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": name, "namespace": "ns"}}),
            )
        };
        assert_eq!(
            evaluate(&[want("a"), want("b"), want("c")], &[]),
            Rollout::Waiting("waiting for Service/a to be created (and 2 more)".into())
        );
    }
}
