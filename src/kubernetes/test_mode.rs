//! What changes under the `test-crd` cargo feature.
//!
//! The feature exists so a development controller can run against the real
//! cluster alongside the production one. Swapping the CRD kind to
//! `TestDeployConfig` keeps the two from fighting over custom resources, but
//! not over the workloads those resources create, so test mode also:
//!
//! - prefixes every DeployConfig namespace with [`NAMESPACE_PREFIX`], so the
//!   same `.deploy/*.yaml` deploys into `test-crd-<ns>` instead of `<ns>`;
//! - skips `Ingress` manifests, because two ingresses claiming the same host
//!   confuse the ingress controller;
//! - does not report deployments to GitHub.
//!
//! Webhooks are deliberately not special-cased: the proxy fans events out to
//! every reader, and reacting to them is most of what the controller does, so
//! the dev instance connects exactly as production does.
//!
//! All of it is compile-time: the production binary contains none of these
//! branches.

/// Whether this binary was built with the `test-crd` feature.
pub const ENABLED: bool = cfg!(feature = "test-crd");

/// Prefix applied to every DeployConfig namespace in test mode.
pub const NAMESPACE_PREFIX: &str = "test-crd-";

/// The namespace a `.deploy/*.yaml` config actually deploys into.
pub fn deploy_namespace(namespace: &str) -> String {
    if ENABLED {
        format!("{NAMESPACE_PREFIX}{namespace}")
    } else {
        namespace.to_string()
    }
}

/// Whether a manifest of this kind is left out of the deploy in test mode.
pub fn skips_kind(kind: &str) -> bool {
    ENABLED && kind == "Ingress"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_prefix_follows_the_feature() {
        let ns = deploy_namespace("cicd");
        if ENABLED {
            assert_eq!(ns, "test-crd-cicd");
        } else {
            assert_eq!(ns, "cicd");
        }
    }

    #[test]
    fn only_ingress_is_skipped_and_only_in_test_mode() {
        assert!(!skips_kind("Deployment"));
        assert_eq!(skips_kind("Ingress"), ENABLED);
    }
}
