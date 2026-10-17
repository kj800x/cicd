//! The deploy run driver: one background loop that moves every unfinished
//! run forward.
//!
//! Every few seconds it looks at each run still rolling out, renders its
//! config the way the controller does, and judges the namespace's objects
//! against that render (`kubernetes::rollout`). A run whose rollout is
//! done becomes `SUCCEEDED`; one that failed, or is still going past its
//! deadline, becomes `FAILED`. State lives in sqlite, so a cicd restart
//! carries on from where the last one stopped.
//!
//! A failed run then follows its config's `checks.onFailure`: roll back to
//! the last run that succeeded (which adds a blocker), or hold the config
//! with a blocker. The default is to roll back autodeploys, whose failures
//! nobody is watching, and hold everything else. A run started by a
//! rollback never rolls back again; its failure is only reported. Every
//! failure is posted to Discord.
//!
//! Until rollout tracking has earned trust, failures are observed only:
//! nothing is rolled back or held, and the Discord message says what would
//! have happened. `CICD_FAILED_RUN_ACTIONS=act` turns the actions on.
use std::time::Duration;

use chrono::Utc;
use kube::{Api, Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::crab_ext::Octocrabs;
use crate::db::blocker::Blocker;
use crate::db::deploy_run::{DeployRun, RunState};
use crate::deploys::SelectionIntent;
use crate::discord;
use crate::error::AppResult;
use crate::kubernetes::api::ListMode;
use crate::kubernetes::deploy_config::OnFailure;
use crate::kubernetes::discovery_cache;
use crate::kubernetes::rollout::{self, Rollout};
use crate::kubernetes::{list_namespace_objects, DeployConfig};
use crate::web::Action;

const TICK: Duration = Duration::from_secs(5);

/// Who cicd's own actions are recorded as.
const ACTOR: &str = "cicd";

/// Run forever. Errors are logged and the next tick tries again.
pub async fn run(client: Client, pool: Pool<SqliteConnectionManager>, octocrabs: Octocrabs) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(e) = tick(&client, &pool, &octocrabs).await {
            log::warn!("Deploy run driver tick failed: {}", e);
        }
    }
}

async fn tick(
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
) -> AppResult<()> {
    let runs = DeployRun::unfinished(&pool.get()?)?;
    if runs.is_empty() {
        return Ok(());
    }
    // One list per tick, and only while something is rolling out.
    let configs = Api::<DeployConfig>::all(client.clone())
        .list(&Default::default())
        .await?
        .items;
    for run in runs {
        let config = configs.iter().find(|c| c.name_any() == run.config_name);
        let verdict = match config {
            Some(config) => judge(client, config).await,
            None => Rollout::Failed(format!("DeployConfig {} no longer exists", run.config_name)),
        };
        let failed = advance(&pool.get()?, &run, verdict)?;
        if let Some(why) = failed {
            on_failure(client, pool, octocrabs, &run, config, &why).await;
        }
    }
    Ok(())
}

/// Judge a config's rollout from a fresh render and the namespace's objects
/// (from the watch-fed cache, so a tick costs the API server nothing).
async fn judge(client: &Client, config: &DeployConfig) -> Rollout {
    let desired = match config
        .render_manifests()
        .and_then(|rendered| config.child_objects(rendered))
    {
        Ok(desired) => desired,
        // The controller cannot apply what does not render either; the
        // deadline decides when to give up.
        Err(e) => return Rollout::Waiting(format!("the config does not render: {e}")),
    };
    // Only namespaced objects are in the namespace listing; a cluster-scoped
    // one would wait forever. Without discovery, judge everything.
    let desired: Vec<_> = match discovery_cache::namespaced_kinds(client).await {
        Ok(kinds) => desired
            .into_iter()
            .filter(|obj| {
                obj.types.as_ref().is_none_or(|t| {
                    kinds
                        .iter()
                        .any(|(ar, _)| ar.kind == t.kind && ar.api_version == t.api_version)
                })
            })
            .collect(),
        Err(_) => desired,
    };
    let ns = config.namespace().unwrap_or_else(|| "default".to_string());
    match list_namespace_objects(client, &ns, ListMode::All).await {
        Ok(live) => rollout::evaluate(&desired, &live),
        Err(e) => Rollout::Waiting(format!("could not list objects in {ns}: {e}")),
    }
}

/// Apply a verdict to a run: end it, or record what it waits for unless
/// the deadline has passed. Returns the reason when this call failed the
/// run, so the caller acts on each failure exactly once.
fn advance(
    conn: &r2d2::PooledConnection<SqliteConnectionManager>,
    run: &DeployRun,
    verdict: Rollout,
) -> AppResult<Option<String>> {
    let (state, message) = match verdict {
        Rollout::Done => (RunState::Succeeded, None),
        Rollout::Failed(why) => (RunState::Failed, Some(why)),
        Rollout::Waiting(what) => {
            let now = Utc::now().timestamp_millis();
            if run.deadline_at.is_some_and(|deadline| now > deadline) {
                let minutes = run
                    .deadline_at
                    .map(|d| (d - run.created_at) / 60_000)
                    .unwrap_or_default();
                (
                    RunState::Failed,
                    Some(format!(
                        "did not roll out within {minutes} minutes; still {what}"
                    )),
                )
            } else {
                if run.message.as_deref() != Some(what.as_str()) {
                    DeployRun::set_progress(conn, run.id, &what)?;
                }
                return Ok(None);
            }
        }
    };
    if DeployRun::finish(conn, run.id, state, message.as_deref())? {
        match &message {
            Some(why) => log::warn!(
                "Deploy run {} for {} {}: {}",
                run.id,
                run.config_name,
                state,
                why
            ),
            None => log::info!("Deploy run {} for {} {}", run.id, run.config_name, state),
        }
        if state == RunState::Failed {
            return Ok(message);
        }
    }
    Ok(None)
}

/// What a failed run does.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Plan {
    /// Roll back to this revision.
    RollBack(i64),
    /// Add a blocker. `no_target` when the config asked for a rollback
    /// but there is no successful run to go back to.
    Hold { no_target: bool },
    /// Report only: the run was itself a rollback (its blocker is already
    /// in place), or the config is gone.
    Report,
}

/// Decide what a failed run does from its trigger, the config's
/// `onFailure` and the revision of the last run that succeeded.
fn plan(trigger: &str, on_failure: Option<OnFailure>, last_good: Option<i64>) -> Plan {
    if trigger == "rollback" {
        return Plan::Report;
    }
    let policy = on_failure.unwrap_or(if trigger == "autodeploy" {
        OnFailure::Rollback
    } else {
        OnFailure::Hold
    });
    match (policy, last_good) {
        (OnFailure::Rollback, Some(revision)) => Plan::RollBack(revision),
        (OnFailure::Rollback, None) => Plan::Hold { no_target: true },
        (OnFailure::Hold, _) => Plan::Hold { no_target: false },
    }
}

/// Whether failures are acted on, or only observed and reported.
fn acting() -> bool {
    std::env::var("CICD_FAILED_RUN_ACTIONS")
        .map(|v| v.trim().eq_ignore_ascii_case("act"))
        .unwrap_or(false)
}

/// Follow a failed run's plan and report it to Discord. Never fails: a
/// step that goes wrong is reported in the same message.
async fn on_failure(
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
    run: &DeployRun,
    config: Option<&DeployConfig>,
    why: &str,
) {
    let name = &run.config_name;
    let last_good = pool
        .get()
        .ok()
        .and_then(|conn| DeployRun::last_good(&conn, name, run.id, run.revision_id).ok())
        .flatten()
        .and_then(|r| r.revision_id);
    let plan = match config {
        Some(config) => plan(&run.trigger, config.spec.spec.checks.on_failure, last_good),
        None => Plan::Report,
    };
    let acting = acting();
    let summary = format!(
        "deploy run {} (revision {}, {}) failed: {}",
        run.id,
        run.revision_id
            .map(|r| r.to_string())
            .unwrap_or_else(|| "none".to_string()),
        run.trigger,
        why
    );

    let outcome = match (&plan, config, acting) {
        (Plan::Report, _, _) => "Nothing was rolled back or held.".to_string(),
        (Plan::RollBack(revision), _, false) => {
            format!("Observe-only: would have rolled back to revision {revision}.")
        }
        (Plan::Hold { no_target }, _, false) => format!(
            "Observe-only: would have held {name} with a blocker{}.",
            if *no_target {
                " (no successful run to roll back to)"
            } else {
                ""
            }
        ),
        (Plan::RollBack(revision), Some(config), true) => {
            match roll_back(client, pool, octocrabs, run, config, *revision, &summary).await {
                Ok(()) => format!(
                    "Rolled back to revision {revision}; deploys are held until the blocker is cleared."
                ),
                Err(e) => match hold(pool, name, &summary) {
                    Ok(()) => format!("Rollback to revision {revision} failed ({e}); held instead."),
                    Err(e2) => format!(
                        "Rollback to revision {revision} failed ({e}), and so did adding a blocker ({e2})."
                    ),
                },
            }
        }
        (Plan::Hold { no_target }, _, true) => match hold(pool, name, &summary) {
            Ok(()) => format!(
                "Held with a blocker{}; clear it when it is safe to deploy again.",
                if *no_target {
                    " (no successful run to roll back to)"
                } else {
                    ""
                }
            ),
            Err(e) => format!("Adding a blocker failed: {e}"),
        },
        (Plan::RollBack(_), None, true) => "The config is gone; nothing to roll back.".to_string(),
    };
    log::warn!("{}: {} {}", name, summary, outcome);
    discord::notify(&format!(
        "**{name}**: {summary}\n{outcome}\n{}/deploy-history/{name}",
        discord::CICD_URL
    ))
    .await;
}

async fn roll_back(
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
    run: &DeployRun,
    config: &DeployConfig,
    revision: i64,
    summary: &str,
) -> AppResult<()> {
    let intent = SelectionIntent {
        note: Some(summary.to_string()),
        by: Some(ACTOR.to_string()),
        ..Default::default()
    };
    crate::deploys::run_action(
        &Action::Rollback { revision },
        config,
        client,
        octocrabs,
        pool,
        ACTOR,
        &intent,
    )
    .await?;
    DeployRun::mark_rolled_back(&pool.get()?, run.id)
}

fn hold(pool: &Pool<SqliteConnectionManager>, name: &str, summary: &str) -> AppResult<()> {
    let reason = format!("Held after {summary}. Clear when it is safe to deploy again.");
    Blocker::create(&pool.get()?, name, &reason, ACTOR)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::deploy_run::DEFAULT_ROLLOUT_DEADLINE_MS;
    use crate::db::revision::{NewRevision, Revision};
    use crate::db::test_support::migrated_memory_pool;

    fn start(conn: &r2d2::PooledConnection<SqliteConnectionManager>) -> AppResult<DeployRun> {
        let revision = Revision::record(
            conn,
            NewRevision {
                config_name: "site".to_string(),
                actor: "web".to_string(),
                action: "deploy".to_string(),
                reason: None,
                config_sha: Some("c1".to_string()),
                config_branch: None,
                config_version_hash: None,
                patches: None,
                temporary: false,
                parameters: vec![],
            },
        )?;
        Ok(
            DeployRun::after_revision(conn, &revision, "web", DEFAULT_ROLLOUT_DEADLINE_MS)?
                .expect("a deploy starts a run"),
        )
    }

    /// Judge every deployed config on the cluster, as the driver would.
    /// Everything that is rolled out should read as done; anything else is
    /// printed with its reason. Needs a kubeconfig:
    /// `cargo test live_judge -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_judge_every_config() -> AppResult<()> {
        let client = Client::try_default().await?;
        let configs = Api::<DeployConfig>::all(client.clone())
            .list(&Default::default())
            .await?
            .items;
        let mut not_done = 0;
        for config in &configs {
            if config
                .status
                .as_ref()
                .and_then(|s| s.config.as_ref())
                .is_none()
            {
                continue;
            }
            match judge(&client, config).await {
                Rollout::Done => {}
                other => {
                    not_done += 1;
                    println!("{}: {:?}", config.name_any(), other);
                }
            }
        }
        println!("{} configs judged, {} not done", configs.len(), not_done);
        Ok(())
    }

    #[test]
    fn waiting_records_progress_until_the_deadline() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let run = start(&conn)?;
        advance(
            &conn,
            &run,
            Rollout::Waiting("waiting for Deployment/web".into()),
        )?;
        let run = DeployRun::get(&conn, run.id)?.expect("exists");
        assert_eq!(run.state, RunState::RollingOut);
        assert_eq!(run.message.as_deref(), Some("waiting for Deployment/web"));

        let overdue = DeployRun {
            deadline_at: Some(Utc::now().timestamp_millis() - 1),
            created_at: Utc::now().timestamp_millis() - 10 * 60_000 - 1,
            ..run
        };
        advance(
            &conn,
            &overdue,
            Rollout::Waiting("waiting for Deployment/web".into()),
        )?;
        let run = DeployRun::get(&conn, overdue.id)?.expect("exists");
        assert_eq!(run.state, RunState::Failed);
        assert_eq!(
            run.message.as_deref(),
            Some("did not roll out within 10 minutes; still waiting for Deployment/web")
        );
        Ok(())
    }

    #[test]
    fn failures_roll_back_autodeploys_and_hold_the_rest() {
        assert_eq!(plan("autodeploy", None, Some(7)), Plan::RollBack(7));
        assert_eq!(
            plan("autodeploy", None, None),
            Plan::Hold { no_target: true }
        );
        assert_eq!(plan("web", None, Some(7)), Plan::Hold { no_target: false });
        assert_eq!(
            plan("mcp", Some(OnFailure::Rollback), Some(7)),
            Plan::RollBack(7)
        );
        assert_eq!(
            plan("autodeploy", Some(OnFailure::Hold), Some(7)),
            Plan::Hold { no_target: false }
        );
        // A rollback never rolls back again, whatever the config says.
        assert_eq!(
            plan("rollback", Some(OnFailure::Rollback), Some(7)),
            Plan::Report
        );
    }

    #[test]
    fn done_and_failed_end_the_run() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let run = start(&conn)?;
        advance(&conn, &run, Rollout::Done)?;
        assert_eq!(
            DeployRun::get(&conn, run.id)?.map(|r| r.state),
            Some(RunState::Succeeded)
        );
        let run = start(&conn)?;
        let failed = advance(&conn, &run, Rollout::Failed("crash".into()))?;
        assert_eq!(failed.as_deref(), Some("crash"));
        let run = DeployRun::get(&conn, run.id)?.expect("exists");
        assert_eq!(run.state, RunState::Failed);
        assert_eq!(run.message.as_deref(), Some("crash"));
        Ok(())
    }
}
