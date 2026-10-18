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
//! Autodeploys a pre-check held wait here too: a `WAITING` run is asked
//! again every `checks.retryInterval` by replaying "deploy latest" as
//! autodeploy, and goes ahead (keeping its run) once nothing holds it. It
//! is dropped if autodeploy is turned off, a blocker is added or the
//! config becomes a temporary deployment, and reported to Discord once if
//! it waits longer than `checks.maxWait`.
//!
//! Until rollout tracking has earned trust, failures are observed only:
//! nothing is rolled back or held, and the Discord message says what would
//! have happened. `CICD_FAILED_RUN_ACTIONS=act` turns the actions on.
use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use kube::{Api, Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::checks::{poll_post_check, PostAsk, PostPoll, PostStatus};
use crate::crab_ext::Octocrabs;
use crate::db::blocker::Blocker;
use crate::db::check_result::CheckResult;
use crate::db::deploy_run::{DeployRun, RunState, WatchState};
use crate::db::revision::Revision;
use crate::deploys::SelectionIntent;
use crate::discord;
use crate::error::{AppError, AppResult};
use crate::kubernetes::api::ListMode;
use crate::kubernetes::deploy_config::{
    parse_duration_ms, CheckSpec, DeployChecks, OnFailure, Severity,
};
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
    let (runs, waiting, verifying, watching) = {
        let conn = pool.get()?;
        (
            DeployRun::unfinished(&conn)?,
            DeployRun::waiting(&conn)?,
            DeployRun::verifying(&conn)?,
            DeployRun::watching(&conn)?,
        )
    };
    if runs.is_empty() && waiting.is_empty() && verifying.is_empty() && watching.is_empty() {
        return Ok(());
    }
    // One list per tick, and only while some run is not settled.
    let configs = Api::<DeployConfig>::all(client.clone())
        .list(&Default::default())
        .await?
        .items;
    let gone = |name: &str| format!("DeployConfig {name} no longer exists");
    for run in waiting {
        let config = configs.iter().find(|c| c.name_any() == run.config_name);
        retry_waiting(client, pool, octocrabs, &run, config).await;
    }
    for run in runs {
        let config = configs.iter().find(|c| c.name_any() == run.config_name);
        let verdict = match config {
            Some(config) => judge(client, config).await,
            None => Rollout::Failed(gone(&run.config_name)),
        };
        let checks = config.map(DeployConfig::checks).unwrap_or_default();
        let failed = advance(&pool.get()?, &run, verdict, &checks)?;
        if let Some(why) = failed {
            on_failure(client, pool, octocrabs, &run, config, &why).await;
        }
    }
    for run in verifying {
        let config = configs.iter().find(|c| c.name_any() == run.config_name);
        let failed = match config {
            Some(config) => verify(client, pool, &run, config).await?,
            None => {
                let why = gone(&run.config_name);
                DeployRun::finish(&pool.get()?, run.id, RunState::Failed, Some(&why))?
                    .then_some(why)
            }
        };
        if let Some(why) = failed {
            on_failure(client, pool, octocrabs, &run, config, &why).await;
        }
    }
    for run in watching {
        let config = configs.iter().find(|c| c.name_any() == run.config_name);
        let failed = match config {
            Some(config) => watch(client, pool, &run, config).await?,
            None => {
                DeployRun::end_watch(&pool.get()?, run.id, "ended: the config is gone")?;
                None
            }
        };
        if let Some(why) = failed {
            on_failure(client, pool, octocrabs, &run, config, &why).await;
        }
    }
    Ok(())
}

/// Ask a waiting autodeploy's pre-checks again if it is due, unless the
/// reasons it was autodeploying have gone away; report it once if it has
/// waited too long.
async fn retry_waiting(
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
    run: &DeployRun,
    config: Option<&DeployConfig>,
) {
    let now = Utc::now().timestamp_millis();
    let name = &run.config_name;
    let cancel = |why: &str| match pool.get() {
        Ok(conn) => {
            if let Err(e) = DeployRun::cancel_waiting(&conn, run.id, why) {
                log::error!("Failed to drop waiting run {}: {}", run.id, e);
            } else {
                log::info!("Dropped waiting run {} for {}: {}", run.id, name, why);
            }
        }
        Err(e) => log::error!("Failed to drop waiting run {}: {}", run.id, e),
    };
    let Some(config) = config else {
        cancel("the config is gone");
        return;
    };
    if !config.autodeploy() {
        cancel("autodeploy was turned off");
        return;
    }
    if config.is_temporary_deployment() {
        cancel("the config became a temporary deployment");
        return;
    }
    if pool
        .get()
        .ok()
        .and_then(|conn| Blocker::active_for(&conn, name).ok())
        .is_some_and(|b| !b.is_empty())
    {
        cancel("a blocker was added");
        return;
    }

    let checks = &config.spec.spec.checks;
    if run.notified_at.is_none() && now - run.created_at > checks.max_wait_ms() {
        let hours = (now - run.created_at) / 3_600_000;
        discord::notify(&format!(
            "**{name}**: autodeploy has waited {hours} h on its pre-checks: {}\n{}/deploy?selected={name}",
            run.message.as_deref().unwrap_or("held"),
            discord::CICD_URL
        ))
        .await;
        if let Ok(conn) = pool.get() {
            let _ = DeployRun::mark_notified(&conn, run.id);
        }
    }

    if run.next_check_at.is_some_and(|at| at > now) {
        return;
    }
    let intent = SelectionIntent {
        waiting_run: Some(run.id),
        ..Default::default()
    };
    match crate::deploys::run_action(
        &Action::DeployLatest,
        config,
        client,
        octocrabs,
        pool,
        crate::web::feed::AUTODEPLOY_ACTOR,
        &intent,
    )
    .await
    {
        Ok(_) => log::info!("Waiting run {} for {} went ahead", run.id, name),
        Err(AppError::Held(why)) => log::info!("{}", why),
        Err(e) => {
            log::warn!("Retry of waiting run {} for {} failed: {}", run.id, name, e);
            if let Ok(conn) = pool.get() {
                let _ = DeployRun::keep_waiting(
                    &conn,
                    run.id,
                    &format!("the last retry failed: {e}"),
                    now + checks.retry_interval_ms(),
                );
            }
        }
    }
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

/// The two kinds of checks that run after a rollout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Part of deploying: the run is not done until they pass.
    Verify,
    /// After the deploy succeeded, for as long as they take.
    Watch,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Verify => "verify",
            Phase::Watch => "watch",
        }
    }

    fn specs(self, checks: &DeployChecks) -> &[CheckSpec] {
        match self {
            Phase::Verify => &checks.verify,
            Phase::Watch => &checks.watch,
        }
    }

    /// How long a check may take to reach a final answer by default.
    fn default_timeout_ms(self) -> i64 {
        match self {
            Phase::Verify => DEFAULT_VERIFY_TIMEOUT_MS,
            Phase::Watch => DEFAULT_WATCH_TIMEOUT_MS,
        }
    }

    /// The most a failing answer counts for unless the check says.
    fn default_on_fail(self) -> Severity {
        match self {
            Phase::Verify => Severity::Rollback,
            Phase::Watch => Severity::Warn,
        }
    }
}

const DEFAULT_VERIFY_TIMEOUT_MS: i64 = 10 * 60_000;
const DEFAULT_WATCH_TIMEOUT_MS: i64 = 60 * 60_000;

/// One `PENDING` row per check of `phase`, due now.
fn start_checks(
    conn: &r2d2::PooledConnection<SqliteConnectionManager>,
    run: &DeployRun,
    phase: Phase,
    specs: &[CheckSpec],
) -> AppResult<()> {
    let checks: Vec<(String, i64)> = specs
        .iter()
        .map(|spec| {
            let deadline = spec
                .timeout
                .as_deref()
                .and_then(parse_duration_ms)
                .unwrap_or(phase.default_timeout_ms());
            (spec.name.clone(), deadline)
        })
        .collect();
    CheckResult::start(
        conn,
        run.id,
        phase.as_str(),
        &checks,
        Utc::now().timestamp_millis(),
    )?;
    log::info!(
        "Deploy run {} for {}: {} {} check{}",
        run.id,
        run.config_name,
        match phase {
            Phase::Verify => "rolled out; verifying",
            Phase::Watch => "succeeded; watching",
        },
        checks.len(),
        if checks.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Start a rolled-out run's verify checks. Returns whether it is now
/// verifying; without verify checks it is done.
fn after_rollout(
    conn: &r2d2::PooledConnection<SqliteConnectionManager>,
    run: &DeployRun,
    checks: &DeployChecks,
) -> AppResult<bool> {
    if checks.verify.is_empty() || !DeployRun::verify(conn, run.id)? {
        return Ok(false);
    }
    start_checks(conn, run, Phase::Verify, &checks.verify)?;
    Ok(true)
}

/// Start a succeeded run's watch checks, if it has any.
fn begin_watch(
    conn: &r2d2::PooledConnection<SqliteConnectionManager>,
    run: &DeployRun,
    checks: &DeployChecks,
) -> AppResult<()> {
    if checks.watch.is_empty() || !DeployRun::start_watch(conn, run.id)? {
        return Ok(());
    }
    start_checks(conn, run, Phase::Watch, &checks.watch)
}

/// The most a check's failing answer may count for: its `onFail` (or the
/// phase's default), and never more than a warning on a run that is
/// itself a rollback.
fn cap(spec: Option<&CheckSpec>, trigger: &str, phase: Phase) -> Severity {
    let configured = spec
        .and_then(|s| s.on_fail)
        .unwrap_or(phase.default_on_fail());
    if trigger == "rollback" {
        configured.min(Severity::Warn)
    } else {
        configured
    }
}

/// Fold one poll into a check row: a final answer is capped and ends it;
/// a pending one schedules the next ask.
fn apply_poll(row: &mut CheckResult, poll: PostPoll, cap: Severity, now: i64) {
    row.title = poll.title;
    row.message = poll.message;
    if poll.details_url.is_some() {
        row.details_url = poll.details_url;
    }
    if poll.handle.is_some() {
        row.handle = poll.handle;
    }
    match poll.status.severity() {
        Some(severity) => {
            row.status = PostStatus::from_severity(severity.min(cap))
                .as_str()
                .to_string();
            row.finished_at = Some(now);
            row.next_poll_at = None;
        }
        None => {
            let wait_s = poll.retry_after.unwrap_or(15).clamp(5, 300) as i64;
            row.status = PostStatus::Pending.as_str().to_string();
            row.next_poll_at = Some(now + wait_s * 1000);
        }
    }
}

/// The worst final answer of a phase's checks, and a note on each that
/// was not OK. `None` while any is pending.
fn verdict(rows: &[CheckResult], phase: Phase) -> Option<(Severity, Option<String>)> {
    let mut worst = Severity::Ok;
    let mut notes = Vec::new();
    for row in rows {
        let severity = PostStatus::parse(&row.status)?.severity()?;
        if severity > Severity::Ok {
            notes.push(format!(
                "{} check {}: {}",
                phase.as_str(),
                row.check_name,
                row.title.as_deref().unwrap_or(&row.status)
            ));
        }
        worst = worst.max(severity);
    }
    Some((worst, (!notes.is_empty()).then(|| notes.join("; "))))
}

/// Ask each of a run's `phase` checks that is due, and return every row of
/// the phase as it now stands.
async fn poll_due(
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    run: &DeployRun,
    config: &DeployConfig,
    phase: Phase,
) -> AppResult<Vec<CheckResult>> {
    let now = Utc::now().timestamp_millis();
    let (mut rows, parameters) = {
        let conn = pool.get()?;
        let parameters = match run.revision_id {
            Some(id) => Revision::get(&conn, id)?
                .map(|r| {
                    r.parameters
                        .into_iter()
                        .map(|p| (p.name, p.value))
                        .collect()
                })
                .unwrap_or_default(),
            None => BTreeMap::new(),
        };
        (
            CheckResult::for_run_phase(&conn, run.id, phase.as_str())?,
            parameters,
        )
    };
    let checks = config.checks();
    let specs = phase.specs(&checks);
    for row in rows.iter_mut() {
        if row.status != PostStatus::Pending.as_str() || row.next_poll_at.is_some_and(|at| at > now)
        {
            continue;
        }
        let Some(spec) = specs.iter().find(|s| s.name == row.check_name) else {
            row.status = PostStatus::Ok.as_str().to_string();
            row.title = Some("no longer in the config".to_string());
            row.finished_at = Some(now);
            row.next_poll_at = None;
            row.save(&pool.get()?)?;
            continue;
        };
        if row.deadline_at.is_some_and(|deadline| now > deadline) {
            let on_timeout =
                spec.on_timeout
                    .unwrap_or(Severity::Warn)
                    .min(cap(Some(spec), &run.trigger, phase));
            row.status = PostStatus::from_severity(on_timeout).as_str().to_string();
            row.title = Some(format!(
                "no final answer in time{}",
                row.title
                    .as_deref()
                    .map(|t| format!(" (last: {t})"))
                    .unwrap_or_default()
            ));
            row.finished_at = Some(now);
            row.next_poll_at = None;
            row.save(&pool.get()?)?;
            continue;
        }
        row.attempt += 1;
        let ask = PostAsk {
            phase: phase.as_str(),
            run_id: run.id,
            rolled_out_at: run.rolled_out_at,
            revision_id: run.revision_id,
            trigger: run.trigger.clone(),
            parameters: parameters.clone(),
            attempt: row.attempt,
            handle: row.handle.clone(),
        };
        match poll_post_check(client, config, spec, &ask).await {
            Ok(poll) => apply_poll(
                row,
                poll,
                cap(Some(spec), &run.trigger, phase),
                Utc::now().timestamp_millis(),
            ),
            Err(why) => {
                log::warn!(
                    "{} check {} of run {} did not answer: {}",
                    phase.as_str(),
                    row.check_name,
                    run.id,
                    why
                );
                row.message = Some(why);
                row.next_poll_at = Some(now + 30_000);
            }
        }
        row.save(&pool.get()?)?;
    }
    Ok(rows)
}

/// Poll a verifying run's due checks and end the run once they have all
/// answered; a run that succeeds starts watching. Returns the reason when
/// this call failed the run.
async fn verify(
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    run: &DeployRun,
    config: &DeployConfig,
) -> AppResult<Option<String>> {
    let rows = poll_due(client, pool, run, config, Phase::Verify).await?;
    let Some((worst, message)) = verdict(&rows, Phase::Verify) else {
        return Ok(None);
    };
    let state = match worst {
        Severity::Ok => RunState::Succeeded,
        Severity::Warn => RunState::SucceededWithWarnings,
        Severity::Rollback => RunState::Failed,
    };
    let conn = pool.get()?;
    if !DeployRun::finish(&conn, run.id, state, message.as_deref())? {
        return Ok(None);
    }
    log::info!("Deploy run {} for {} {}", run.id, run.config_name, state);
    if state != RunState::Failed {
        begin_watch(&conn, run, &config.checks())?;
    }
    match state {
        RunState::Failed => Ok(message),
        RunState::SucceededWithWarnings => {
            let name = &run.config_name;
            discord::notify(&format!(
                "**{name}**: deploy run {} rolled out with warnings: {}\n{}/deploy-history/{name}",
                run.id,
                message.as_deref().unwrap_or("a verify check warned"),
                discord::CICD_URL
            ))
            .await;
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Poll a watched run's due checks and end the watch once they have all
/// answered. A warning is reported; a rollback fails the run. Returns the
/// reason when this call failed the run.
async fn watch(
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    run: &DeployRun,
    config: &DeployConfig,
) -> AppResult<Option<String>> {
    let rows = poll_due(client, pool, run, config, Phase::Watch).await?;
    let Some((worst, message)) = verdict(&rows, Phase::Watch) else {
        return Ok(None);
    };
    let outcome = match worst {
        Severity::Ok => WatchState::Ok,
        Severity::Warn => WatchState::Warn,
        Severity::Rollback => WatchState::Rollback,
    };
    if !DeployRun::finish_watch(&pool.get()?, run.id, outcome, message.as_deref())? {
        return Ok(None);
    }
    log::info!(
        "Deploy run {} for {}: watch ended {}",
        run.id,
        run.config_name,
        outcome.as_str()
    );
    match outcome {
        WatchState::Rollback => Ok(message),
        WatchState::Warn => {
            let name = &run.config_name;
            discord::notify(&format!(
                "**{name}**: a watch check warned after deploy run {}: {}\n{}/deploy-history/{name}",
                run.id,
                message.as_deref().unwrap_or("a watch check warned"),
                discord::CICD_URL
            ))
            .await;
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Apply a verdict to a run: end it, or record what it waits for unless
/// the deadline has passed. A run that succeeds starts watching. Returns
/// the reason when this call failed the run, so the caller acts on each
/// failure exactly once.
fn advance(
    conn: &r2d2::PooledConnection<SqliteConnectionManager>,
    run: &DeployRun,
    verdict: Rollout,
    checks: &DeployChecks,
) -> AppResult<Option<String>> {
    let (state, message) = match verdict {
        Rollout::Done if after_rollout(conn, run, checks)? => return Ok(None),
        Rollout::Done => (RunState::Succeeded, None),
        Rollout::Failed(why) => (RunState::Failed, Some(why)),
        Rollout::Waiting(what) => {
            let now = Utc::now().timestamp_millis();
            if run.deadline_at.is_some_and(|deadline| now > deadline) {
                (
                    RunState::Failed,
                    Some(format!("did not roll out by its deadline; still {what}")),
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
        begin_watch(conn, run, checks)?;
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
            &DeployChecks::default(),
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
            &DeployChecks::default(),
        )?;
        let run = DeployRun::get(&conn, overdue.id)?.expect("exists");
        assert_eq!(run.state, RunState::Failed);
        assert_eq!(
            run.message.as_deref(),
            Some("did not roll out by its deadline; still waiting for Deployment/web")
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

    fn row(name: &str, status: &str, title: &str) -> CheckResult {
        CheckResult {
            id: 0,
            run_id: 1,
            phase: "verify".into(),
            check_name: name.into(),
            status: status.into(),
            title: Some(title.into()),
            message: None,
            details_url: None,
            attempt: 1,
            started_at: 0,
            finished_at: None,
            next_poll_at: Some(0),
            deadline_at: Some(600_000),
            handle: None,
        }
    }

    fn poll(status: PostStatus, retry_after: Option<u64>) -> PostPoll {
        PostPoll {
            status,
            title: Some("t".into()),
            message: None,
            details_url: None,
            retry_after,
            handle: None,
        }
    }

    fn job_check(name: &str) -> CheckSpec {
        CheckSpec {
            name: name.into(),
            job: Some("media/jellyfin-at".into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_rollout_with_verify_checks_verifies_them() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let run = start(&conn)?;
        let checks = DeployChecks {
            verify: vec![job_check("smoke")],
            watch: vec![job_check("alerts")],
            ..Default::default()
        };
        assert_eq!(advance(&conn, &run, Rollout::Done, &checks)?, None);
        let run = DeployRun::get(&conn, run.id)?.expect("exists");
        assert_eq!(run.state, RunState::Verifying);
        assert_eq!(run.watch_state, None, "watching waits for verify");
        assert!(run.rolled_out_at.is_some());
        let rows = CheckResult::for_run_phase(&conn, run.id, "verify")?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "PENDING");
        assert_eq!(
            rows[0]
                .deadline_at
                .zip(Some(rows[0].started_at))
                .map(|(d, s)| d - s),
            Some(DEFAULT_VERIFY_TIMEOUT_MS)
        );
        Ok(())
    }

    #[test]
    fn a_run_without_verify_checks_succeeds_and_starts_watching() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let run = start(&conn)?;
        let checks = DeployChecks {
            watch: vec![job_check("alerts")],
            ..Default::default()
        };
        assert_eq!(advance(&conn, &run, Rollout::Done, &checks)?, None);
        let run = DeployRun::get(&conn, run.id)?.expect("exists");
        assert_eq!(run.state, RunState::Succeeded);
        assert_eq!(run.watch_state, Some(WatchState::Watching));
        let rows = CheckResult::for_run_phase(&conn, run.id, "watch")?;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].deadline_at.map(|d| d - rows[0].started_at),
            Some(DEFAULT_WATCH_TIMEOUT_MS)
        );

        // A warning marks the run; a newer deploy would have ended it.
        assert!(DeployRun::finish_watch(
            &conn,
            run.id,
            WatchState::Warn,
            Some("watch check alerts: JellyfinDown fired")
        )?);
        let run = DeployRun::get(&conn, run.id)?.expect("exists");
        assert_eq!(run.state, RunState::SucceededWithWarnings);
        assert_eq!(run.watch_state, Some(WatchState::Warn));
        assert_eq!(
            run.message.as_deref(),
            Some("watch check alerts: JellyfinDown fired")
        );
        assert!(
            !DeployRun::finish_watch(&conn, run.id, WatchState::Ok, None)?,
            "a watch ends once"
        );
        Ok(())
    }

    #[test]
    fn a_newer_deploy_ends_a_watch_early() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let first = start(&conn)?;
        let checks = DeployChecks {
            watch: vec![job_check("alerts")],
            ..Default::default()
        };
        advance(&conn, &first, Rollout::Done, &checks)?;
        let second = start(&conn)?;
        let first = DeployRun::get(&conn, first.id)?.expect("exists");
        assert_eq!(first.state, RunState::Succeeded, "still deployed");
        assert_eq!(first.watch_state, Some(WatchState::Ended));
        let rows = CheckResult::for_run_phase(&conn, first.id, "watch")?;
        assert_eq!(rows[0].status, "ENDED");
        assert_eq!(
            rows[0].title,
            Some(format!("ended: superseded by run {}", second.id))
        );
        assert!(DeployRun::watching(&conn)?.is_empty());

        // A watch that asked for a rollback is not somewhere to roll back to.
        advance(&conn, &second, Rollout::Done, &checks)?;
        DeployRun::finish_watch(&conn, second.id, WatchState::Rollback, Some("bad"))?;
        let second = DeployRun::get(&conn, second.id)?.expect("exists");
        assert_eq!(second.state, RunState::Failed);
        assert_eq!(
            DeployRun::last_good(&conn, "site", second.id + 1, None)?.map(|r| r.id),
            Some(first.id)
        );
        Ok(())
    }

    #[test]
    fn answers_are_capped_by_on_fail_and_by_rollback_runs() {
        let warn_only = CheckSpec {
            on_fail: Some(Severity::Warn),
            ..Default::default()
        };
        assert_eq!(cap(None, "web", Phase::Verify), Severity::Rollback);
        assert_eq!(
            cap(Some(&warn_only), "autodeploy", Phase::Verify),
            Severity::Warn
        );
        assert_eq!(cap(None, "rollback", Phase::Verify), Severity::Warn);
        // Watch checks only warn unless they say otherwise.
        assert_eq!(cap(None, "web", Phase::Watch), Severity::Warn);
        let rollback = CheckSpec {
            on_fail: Some(Severity::Rollback),
            ..Default::default()
        };
        assert_eq!(
            cap(Some(&rollback), "web", Phase::Watch),
            Severity::Rollback
        );

        let mut r = row("smoke", "PENDING", "");
        apply_poll(&mut r, poll(PostStatus::Rollback, None), Severity::Warn, 50);
        assert_eq!(r.status, "WARN");
        assert_eq!(r.finished_at, Some(50));

        let mut r = row("quiet", "PENDING", "");
        apply_poll(
            &mut r,
            poll(PostStatus::Pending, Some(1)),
            Severity::Rollback,
            1000,
        );
        assert_eq!(r.status, "PENDING");
        assert_eq!(r.next_poll_at, Some(6000), "retryAfter is at least 5 s");
        apply_poll(
            &mut r,
            poll(PostStatus::Pending, Some(3600)),
            Severity::Rollback,
            1000,
        );
        assert_eq!(r.next_poll_at, Some(301_000), "and at most 5 min");
    }

    #[test]
    fn the_worst_final_answer_decides() {
        let v = |rows: &[CheckResult]| verdict(rows, Phase::Verify);
        assert_eq!(v(&[row("a", "OK", "fine"), row("b", "PENDING", "")]), None);
        assert_eq!(
            v(&[row("a", "OK", "fine"), row("b", "OK", "fine")]),
            Some((Severity::Ok, None))
        );
        assert_eq!(
            v(&[row("a", "WARN", "slow"), row("b", "OK", "fine")]),
            Some((Severity::Warn, Some("verify check a: slow".into())))
        );
        assert_eq!(
            v(&[row("a", "WARN", "slow"), row("b", "ROLLBACK", "job failed")]),
            Some((
                Severity::Rollback,
                Some("verify check a: slow; verify check b: job failed".into())
            ))
        );
    }

    #[test]
    fn done_and_failed_end_the_run() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let run = start(&conn)?;
        advance(&conn, &run, Rollout::Done, &DeployChecks::default())?;
        assert_eq!(
            DeployRun::get(&conn, run.id)?.map(|r| r.state),
            Some(RunState::Succeeded)
        );
        let run = start(&conn)?;
        let failed = advance(
            &conn,
            &run,
            Rollout::Failed("crash".into()),
            &DeployChecks::default(),
        )?;
        assert_eq!(failed.as_deref(), Some("crash"));
        let run = DeployRun::get(&conn, run.id)?.expect("exists");
        assert_eq!(run.state, RunState::Failed);
        assert_eq!(run.message.as_deref(), Some("crash"));
        Ok(())
    }
}
