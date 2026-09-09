//! A watch-fed cache of every namespaced object in the cluster, for reads.
//!
//! Pages and previews used to list all 83 kinds in a namespace on every
//! request, and the deploy preview does that every two seconds per open
//! tab. Instead, one long-lived watch per kind keeps an in-memory store
//! current, and a read is a filter over memory. A watch costs the API
//! server almost nothing between events, so polling can stay fast.
//!
//! Kinds come from the (cached) discovery, refreshed every ten minutes so
//! a newly installed CRD gets a watch too. No assumption is made about what
//! a config may deploy: every namespaced kind is watched, except the two
//! Events kinds, which carry no ownership and churn constantly.
//!
//! Reads are read-only by design. The controller's prune deletes objects
//! and keeps using live lists: a store can lag an apply by an event, and
//! a prune that saw yesterday's annotations would delete today's object.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures_util::StreamExt;
use kube::api::{DynamicObject, TypeMeta};
use kube::core::discovery::ApiResource;
use kube::runtime::{watcher, WatchStreamExt};
use kube::{Api, Client};

use crate::kubernetes::api::ListMode;
use crate::kubernetes::discovery_cache;

const KIND_REFRESH: Duration = Duration::from_secs(600);
const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
const MANAGER: &str = "cicd-controller";

/// Objects of one kind, indexed by namespace then name, so a read touches
/// only the namespace it asks for. Watch events update it in place; a
/// watch restart rebuilds it from a fresh initial list.
#[derive(Default)]
struct KindIndex {
    types: Option<TypeMeta>,
    by_namespace: HashMap<String, HashMap<String, Arc<DynamicObject>>>,
    /// The initial list of a (re)started watch, swapped in when complete.
    pending: Option<HashMap<String, HashMap<String, Arc<DynamicObject>>>>,
}

#[derive(Default)]
struct Registry {
    kinds: HashMap<String, KindIndex>,
    /// A materialized view per namespace, shared with every reader and
    /// rebuilt only after that namespace changed. Twenty configs in one
    /// namespace read one snapshot instead of cloning it twenty times.
    snapshots: HashMap<String, Arc<Vec<DynamicObject>>>,
}

static REGISTRY: RwLock<Option<Registry>> = RwLock::new(None);

fn key(ar: &ApiResource) -> String {
    format!("{}/{}", ar.api_version, ar.kind)
}

fn skip(ar: &ApiResource) -> bool {
    ar.kind == "Event" && (ar.group.is_empty() || ar.group == "events.k8s.io")
}

fn is_owned(obj: &DynamicObject) -> bool {
    obj.metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(MANAGED_BY))
        .map(String::as_str)
        == Some(MANAGER)
}

fn materialize(registry: &Registry, ns: &str, mode: &ListMode) -> Vec<DynamicObject> {
    let mut out = Vec::new();
    for index in registry.kinds.values() {
        let Some(objects) = index.by_namespace.get(ns) else {
            continue;
        };
        for obj in objects.values() {
            if matches!(mode, ListMode::Owned) && !is_owned(obj) {
                continue;
            }
            let mut obj = (**obj).clone();
            if obj.types.is_none() {
                obj.types = index.types.clone();
            }
            out.push(obj);
        }
    }
    out
}

/// Every object in `ns` the cache knows, or `None` while the cache has not
/// started yet (callers fall back to a live list). The `All` view is a
/// shared snapshot; `Owned` is built on the spot (only the controller
/// asks for it, and it uses live lists anyway).
pub fn list(ns: &str, mode: &ListMode) -> Option<Arc<Vec<DynamicObject>>> {
    {
        let guard = REGISTRY.read().ok()?;
        let registry = guard.as_ref()?;
        if matches!(mode, ListMode::Owned) {
            return Some(Arc::new(materialize(registry, ns, mode)));
        }
        if let Some(snapshot) = registry.snapshots.get(ns) {
            return Some(snapshot.clone());
        }
    }
    let mut guard = REGISTRY.write().ok()?;
    let registry = guard.as_mut()?;
    if let Some(snapshot) = registry.snapshots.get(ns) {
        return Some(snapshot.clone());
    }
    let snapshot = Arc::new(materialize(registry, ns, mode));
    registry.snapshots.insert(ns.to_string(), snapshot.clone());
    Some(snapshot)
}

fn invalidate(registry: &mut Registry, ns: &str) {
    registry.snapshots.remove(ns);
}

/// Start watches for every namespaced kind and keep the set current as
/// discovery changes. Runs forever; failures are logged and retried.
pub async fn run(client: Client) {
    loop {
        match discovery_cache::refresh(&client).await {
            Ok(kinds) => {
                let mut started = 0;
                for (ar, caps) in kinds.iter() {
                    if skip(ar) || !caps.supports_operation(kube::core::discovery::verbs::WATCH) {
                        continue;
                    }
                    if already_watched(ar) {
                        continue;
                    }
                    start_watch(&client, ar.clone());
                    started += 1;
                }
                if started > 0 {
                    log::info!("object cache: watching {} more kinds", started);
                }
            }
            Err(e) => log::warn!("object cache: discovery failed: {}", e),
        }
        tokio::time::sleep(KIND_REFRESH).await;
    }
}

fn already_watched(ar: &ApiResource) -> bool {
    REGISTRY
        .read()
        .ok()
        .is_some_and(|g| g.as_ref().is_some_and(|r| r.kinds.contains_key(&key(ar))))
}

fn with_index<R>(label: &str, f: impl FnOnce(&mut KindIndex) -> R) -> Option<R> {
    let mut guard = REGISTRY.write().ok()?;
    let registry = guard.get_or_insert_with(Registry::default);
    Some(f(registry.kinds.entry(label.to_string()).or_default()))
}

/// Apply a change to one kind's index and drop the snapshots it affects.
fn update(label: &str, ns: Option<&str>, f: impl FnOnce(&mut KindIndex)) {
    let Ok(mut guard) = REGISTRY.write() else {
        return;
    };
    let registry = guard.get_or_insert_with(Registry::default);
    f(registry.kinds.entry(label.to_string()).or_default());
    match ns {
        Some(ns) => invalidate(registry, ns),
        None => registry.snapshots.clear(),
    }
}

fn start_watch(client: &Client, ar: ApiResource) {
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let types = TypeMeta {
        api_version: ar.api_version.clone(),
        kind: ar.kind.clone(),
    };
    let label = key(&ar);
    with_index(&label, |index| index.types = Some(types));
    tokio::spawn(async move {
        let stream = watcher(api, watcher::Config::default().page_size(500)).default_backoff();
        futures_util::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(watcher::Event::Init) => {
                    with_index(&label, |index| index.pending = Some(HashMap::new()));
                }
                Ok(watcher::Event::InitApply(obj)) => {
                    with_index(&label, |index| {
                        if let Some(pending) = index.pending.as_mut() {
                            insert(pending, obj);
                        }
                    });
                }
                Ok(watcher::Event::InitDone) => {
                    update(&label, None, |index| {
                        if let Some(pending) = index.pending.take() {
                            index.by_namespace = pending;
                        }
                    });
                }
                Ok(watcher::Event::Apply(obj)) => {
                    let ns = obj.metadata.namespace.clone().unwrap_or_default();
                    update(&label, Some(&ns), |index| {
                        insert(&mut index.by_namespace, obj)
                    });
                }
                Ok(watcher::Event::Delete(obj)) => {
                    let ns = obj.metadata.namespace.clone().unwrap_or_default();
                    let name = obj.metadata.name.clone().unwrap_or_default();
                    update(&label, Some(&ns), |index| {
                        if let Some(objects) = index.by_namespace.get_mut(&ns) {
                            objects.remove(&name);
                        }
                    });
                }
                Err(e) => log::debug!("object cache: watch of {} hiccup: {}", label, e),
            }
        }
        log::warn!("object cache: watch of {} ended", label);
        if let Ok(mut guard) = REGISTRY.write() {
            if let Some(r) = guard.as_mut() {
                r.kinds.remove(&label);
            }
        }
    });
}

fn insert(map: &mut HashMap<String, HashMap<String, Arc<DynamicObject>>>, obj: DynamicObject) {
    let ns = obj.metadata.namespace.clone().unwrap_or_default();
    let name = obj.metadata.name.clone().unwrap_or_default();
    map.entry(ns).or_default().insert(name, Arc::new(obj));
}
