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
use std::sync::RwLock;
use std::time::Duration;

use futures_util::StreamExt;
use kube::api::{DynamicObject, TypeMeta};
use kube::core::discovery::ApiResource;
use kube::runtime::{reflector, watcher, WatchStreamExt};
use kube::{Api, Client};

use crate::kubernetes::api::ListMode;
use crate::kubernetes::discovery_cache;

const KIND_REFRESH: Duration = Duration::from_secs(600);
const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
const MANAGER: &str = "cicd-controller";

struct KindStore {
    types: TypeMeta,
    reader: reflector::Store<DynamicObject>,
}

#[derive(Default)]
struct Registry {
    kinds: HashMap<String, KindStore>,
}

static REGISTRY: RwLock<Option<Registry>> = RwLock::new(None);

fn key(ar: &ApiResource) -> String {
    format!("{}/{}", ar.api_version, ar.kind)
}

fn skip(ar: &ApiResource) -> bool {
    ar.kind == "Event" && (ar.group.is_empty() || ar.group == "events.k8s.io")
}

/// Every object in `ns` the cache knows, or `None` while the cache has not
/// started yet (callers fall back to a live list).
pub fn list(ns: &str, mode: &ListMode) -> Option<Vec<DynamicObject>> {
    let guard = REGISTRY.read().ok()?;
    let registry = guard.as_ref()?;
    let mut out = Vec::new();
    for store in registry.kinds.values() {
        for obj in store.reader.state() {
            if obj.metadata.namespace.as_deref() != Some(ns) {
                continue;
            }
            if matches!(mode, ListMode::Owned)
                && obj
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(MANAGED_BY))
                    .map(String::as_str)
                    != Some(MANAGER)
            {
                continue;
            }
            let mut obj = (*obj).clone();
            obj.types = obj.types.or(Some(store.types.clone()));
            out.push(obj);
        }
    }
    Some(out)
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

fn start_watch(client: &Client, ar: ApiResource) {
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let writer = reflector::store::Writer::<DynamicObject>::new(ar.clone());
    let reader = writer.as_reader();
    let types = TypeMeta {
        api_version: ar.api_version.clone(),
        kind: ar.kind.clone(),
    };
    if let Ok(mut guard) = REGISTRY.write() {
        guard
            .get_or_insert_with(Registry::default)
            .kinds
            .insert(key(&ar), KindStore { types, reader });
    }
    let label = key(&ar);
    tokio::spawn(async move {
        let stream = reflector(
            writer,
            watcher(api, watcher::Config::default().page_size(500)),
        )
        .default_backoff()
        .touched_objects();
        futures_util::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                log::debug!("object cache: watch of {} hiccup: {}", label, e);
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
