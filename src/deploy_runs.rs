//! The deploy run driver: one background loop that moves every unfinished
//! run forward.
//!
//! Every few seconds it looks at each run still rolling out, renders its
//! config the way the controller does, and judges the namespace's objects
//! against that render (`kubernetes::rollout`). A run whose rollout is
//! done becomes `SUCCEEDED`; one that failed, or is still going past its
//! deadline, becomes `FAILED`. State lives in sqlite, so a cicd restart
//! carries on from where the last one stopped.
use std::time::Duration;

use chrono::Utc;
use kube::{Api, Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::deploy_run::{DeployRun, RunState};
use crate::error::AppResult;
use crate::kubernetes::api::ListMode;
use crate::kubernetes::rollout::{self, Rollout};
use crate::kubernetes::{list_namespace_objects, DeployConfig};

const TICK: Duration = Duration::from_secs(5);

/// Run forever. Errors are logged and the next tick tries again.
pub async fn run(client: Client, pool: Pool<SqliteConnectionManager>) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(e) = tick(&client, &pool).await {
            log::warn!("Deploy run driver tick failed: {}", e);
        }
    }
}

async fn tick(client: &Client, pool: &Pool<SqliteConnectionManager>) -> AppResult<()> {
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
        advance(&pool.get()?, &run, verdict)?;
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
    let ns = config.namespace().unwrap_or_else(|| "default".to_string());
    match list_namespace_objects(client, &ns, ListMode::All).await {
        Ok(live) => rollout::evaluate(&desired, &live),
        Err(e) => Rollout::Waiting(format!("could not list objects in {ns}: {e}")),
    }
}

/// Apply a verdict to a run: end it, or record what it waits for unless
/// the deadline has passed.
fn advance(
    conn: &r2d2::PooledConnection<SqliteConnectionManager>,
    run: &DeployRun,
    verdict: Rollout,
) -> AppResult<()> {
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
                return Ok(());
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        Ok(DeployRun::after_revision(conn, &revision, "web")?.expect("a deploy starts a run"))
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
        advance(&conn, &run, Rollout::Failed("crash".into()))?;
        let run = DeployRun::get(&conn, run.id)?.expect("exists");
        assert_eq!(run.state, RunState::Failed);
        assert_eq!(run.message.as_deref(), Some("crash"));
        Ok(())
    }
}
