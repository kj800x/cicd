use std::sync::OnceLock;

use opentelemetry::{
    global,
    metrics::{Counter, Histogram},
};
use prometheus::{IntGaugeVec, Opts};

pub struct Metrics {
    pub deploy_actions: Counter<u64>,
    pub commits_observed: Counter<u64>,
    pub builds_started: Counter<u64>,
    pub builds_resolved: Counter<u64>,
    pub build_duration_seconds: Histogram<f64>,
    pub github_rate_limit_remaining: IntGaugeVec,
    pub github_rate_limit_limit: IntGaugeVec,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

pub fn init(registry: &prometheus::Registry) -> Result<(), anyhow::Error> {
    let meter = global::meter("cicd");

    let github_rate_limit_remaining = IntGaugeVec::new(
        Opts::new(
            "cicd_github_rate_limit_remaining",
            "GitHub API rate limit remaining requests",
        ),
        &["token"],
    )?;
    registry.register(Box::new(github_rate_limit_remaining.clone()))?;

    let github_rate_limit_limit = IntGaugeVec::new(
        Opts::new(
            "cicd_github_rate_limit_limit",
            "GitHub API rate limit total requests allowed",
        ),
        &["token"],
    )?;
    registry.register(Box::new(github_rate_limit_limit.clone()))?;

    let metrics = Metrics {
        deploy_actions: meter.u64_counter("cicd_deploy_actions_total").init(),
        commits_observed: meter.u64_counter("cicd_commits_observed_total").init(),
        builds_started: meter.u64_counter("cicd_builds_started_total").init(),
        builds_resolved: meter.u64_counter("cicd_builds_resolved_total").init(),
        build_duration_seconds: meter.f64_histogram("cicd_build_duration_seconds").init(),
        github_rate_limit_remaining,
        github_rate_limit_limit,
    };

    METRICS
        .set(metrics)
        .map_err(|_| anyhow::anyhow!("Metrics already initialized"))?;

    Ok(())
}

#[allow(clippy::expect_used)]
pub fn get() -> &'static Metrics {
    METRICS
        .get()
        .expect("Metrics not initialized - call metrics::init() first")
}
