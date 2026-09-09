//! API discovery, cached.
//!
//! Discovery (which groups, versions and kinds the cluster serves) is a few
//! dozen requests and changes only when a CRD is installed. It used to run
//! on every reconcile and every page that listed a namespace, which at
//! sixty configs requeueing every five seconds was most of the load on the
//! API server. Now it runs at most once per [`TTL`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kube::core::discovery::{ApiCapabilities, ApiResource};
use kube::{core::discovery, Client, Discovery};

use crate::error::AppResult;

pub const TTL: Duration = Duration::from_secs(600);

/// Every namespaced, top-level (not a subresource) kind the cluster serves.
pub type NamespacedKinds = Arc<Vec<(ApiResource, ApiCapabilities)>>;

static CACHE: Mutex<Option<(Instant, NamespacedKinds)>> = Mutex::new(None);

fn cached() -> Option<NamespacedKinds> {
    let guard = CACHE.lock().ok()?;
    guard
        .as_ref()
        .filter(|(at, _)| at.elapsed() < TTL)
        .map(|(_, kinds)| kinds.clone())
}

/// The namespaced kinds, from cache or a fresh discovery run.
pub async fn namespaced_kinds(client: &Client) -> AppResult<NamespacedKinds> {
    if let Some(kinds) = cached() {
        return Ok(kinds);
    }
    refresh(client).await
}

/// Run discovery now and replace the cache.
pub async fn refresh(client: &Client) -> AppResult<NamespacedKinds> {
    let disc = Discovery::new(client.clone()).run().await?;
    let mut kinds = Vec::new();
    for group in disc.groups() {
        for (ar, caps) in group.resources_by_stability() {
            if caps.scope != discovery::Scope::Namespaced || ar.plural.contains('/') {
                continue;
            }
            kinds.push((ar, caps));
        }
    }
    log::info!("API discovery: {} namespaced kinds", kinds.len());
    let kinds: NamespacedKinds = Arc::new(kinds);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((Instant::now(), kinds.clone()));
    }
    Ok(kinds)
}
