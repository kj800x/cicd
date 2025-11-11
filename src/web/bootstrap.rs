use crate::crab_ext::Octocrabs;
use crate::db::{
    git_branch::GitBranchEgg, git_commit::GitCommitEgg, git_commit_build::GitCommitBuild,
    git_repo::GitRepo,
};
use crate::prelude::*;
use crate::webhooks::config_sync::sync_deploy_configs_for_commit;
use actix_web::http::StatusCode;
use chrono::Utc;
use kube::Client;
use octocrab::Octocrab;
use std::sync::{Arc, Mutex, OnceLock};

const PER_PAGE: u8 = 100;

static BOOTSTRAP_LOG: OnceLock<Arc<Mutex<String>>> = OnceLock::new();
static BOOTSTRAP_LOCK: OnceLock<Arc<Mutex<bool>>> = OnceLock::new();

fn get_bootstrap_log() -> Arc<Mutex<String>> {
    BOOTSTRAP_LOG
        .get_or_init(|| Arc::new(Mutex::new(String::new())))
        .clone()
}

fn get_bootstrap_lock() -> Arc<Mutex<bool>> {
    BOOTSTRAP_LOCK
        .get_or_init(|| Arc::new(Mutex::new(false)))
        .clone()
}

fn try_acquire_lock() -> bool {
    if let Ok(mut lock) = get_bootstrap_lock().lock() {
        if *lock {
            return false; // Already locked
        }
        *lock = true;
        true
    } else {
        false
    }
}

fn release_lock() {
    if let Ok(mut lock) = get_bootstrap_lock().lock() {
        *lock = false;
    }
}

fn log_clear() {
    if let Ok(mut s) = get_bootstrap_log().lock() {
        *s = String::new();
    }
}

fn log_append(line: impl AsRef<str>) {
    let timestamp = Utc::now().to_rfc3339();
    if let Ok(mut s) = get_bootstrap_log().lock() {
        s.push_str(&format!("[{}] {}\n", timestamp, line.as_ref()));
    }
    log::info!("{}", line.as_ref());
}

async fn log_rate_limit(crab: &Octocrab, label: &str) -> Option<usize> {
    // According to GitHub docs, GET /rate_limit doesn't count against primary rate limit
    // but can count against secondary rate limit, so use sparingly
    match crab.ratelimit().get().await {
        Ok(rate_info) => {
            let reset_time = rate_info.resources.core.reset;
            log_append(format!(
                "[ratelimit:{}] core {}/{} (resets at {})",
                label,
                rate_info.resources.core.remaining,
                rate_info.resources.core.limit,
                reset_time
            ));
            Some(rate_info.resources.core.remaining)
        }
        Err(e) => {
            log::warn!("Failed to fetch rate limit for {}: {:?}", label, e);
            log_append(format!("Failed to fetch rate limit for {}: {:?}", label, e));
            None
        }
    }
}

async fn list_all_repos(crab: &Octocrab) -> anyhow::Result<Vec<octocrab::models::Repository>> {
    let mut all: Vec<octocrab::models::Repository> = Vec::new();
    let mut page: u8 = 1;
    loop {
        let resp = crab
            .current()
            .list_repos_for_authenticated_user()
            .affiliation("owner")
            .per_page(PER_PAGE)
            .page(page)
            .send()
            .await?;
        let count = resp.items.len() as u8;
        all.extend(resp.items);
        if count < PER_PAGE {
            break;
        }
        page += 1;
    }
    Ok(all)
}

async fn list_all_branches(
    crab: &Octocrab,
    owner: &str,
    repo: &str,
) -> anyhow::Result<Vec<octocrab::models::repos::Branch>> {
    let mut all: Vec<octocrab::models::repos::Branch> = Vec::new();
    let mut page: u8 = 1;
    loop {
        let resp = crab
            .repos(owner, repo)
            .list_branches()
            .per_page(PER_PAGE)
            .page(page)
            .send()
            .await?;
        let count = resp.items.len() as u8;
        all.extend(resp.items);
        if count < PER_PAGE {
            break;
        }
        page += 1;
    }
    Ok(all)
}

async fn list_commits_for_branch(
    crab: &Octocrab,
    owner: &str,
    repo: &str,
    branch: &str,
    limit: u8,
) -> anyhow::Result<Vec<octocrab::models::repos::RepoCommit>> {
    let resp = crab
        .repos(owner, repo)
        .list_commits()
        .sha(branch)
        .per_page(limit)
        .send()
        .await?;
    Ok(resp.items)
}

async fn get_status_state_for_sha(
    crab: &Octocrab,
    owner: &str,
    repo: &str,
    sha: &str,
) -> anyhow::Result<Option<String>> {
    let mut any_error_or_failure = false;
    let mut any_pending = false;
    let mut any_success = false;

    // Check the older Statuses API
    match crab
        .repos(owner, repo)
        .list_statuses(sha.to_string())
        .per_page(100)
        .page(1u32)
        .send()
        .await
    {
        Ok(resp) => {
            for st in resp.items {
                let state_str = format!("{:?}", st.state).to_lowercase();
                match state_str.as_str() {
                    "error" | "failure" => any_error_or_failure = true,
                    "pending" => any_pending = true,
                    "success" => any_success = true,
                    _ => {}
                }
            }
        }
        Err(e) => {
            log::debug!("Failed to fetch statuses for {}: {:?}", sha, e);
        }
    }

    // Check the newer Checks API (used by GitHub Actions and most modern CI)
    match crab
        .checks(owner, repo)
        .list_check_suites_for_git_ref(octocrab::params::repos::Commitish(sha.to_string()))
        .per_page(100)
        .send()
        .await
    {
        Ok(resp) => {
            log::debug!(
                "Check suites for {}: found {} suites",
                &sha[..7],
                resp.check_suites.len()
            );
            for suite in resp.check_suites {
                // status can be "queued", "in_progress", or "completed"
                // conclusion can be "success", "failure", "neutral", "cancelled", "timed_out", "action_required", "skipped", null
                log::debug!(
                    "Check suite for {}: status={:?}, conclusion={:?}",
                    &sha[..7],
                    suite.status,
                    suite.conclusion
                );
                match (suite.status.as_deref(), suite.conclusion.as_deref()) {
                    (Some("completed"), Some("success")) => any_success = true,
                    (Some("completed"), Some("failure" | "timed_out" | "action_required")) => {
                        any_error_or_failure = true
                    }
                    (Some("queued" | "in_progress"), _) => any_pending = true,
                    // Treat neutral, cancelled, and skipped as success for display purposes
                    (Some("completed"), Some("neutral" | "cancelled" | "skipped")) => {
                        any_success = true
                    }
                    _ => {
                        log::debug!(
                            "Unhandled check suite state for {}: status={:?}, conclusion={:?}",
                            &sha[..7],
                            suite.status,
                            suite.conclusion
                        );
                    }
                }
            }
        }
        Err(e) => {
            log::debug!("Failed to fetch check suites for {}: {:?}", &sha[..7], e);
        }
    }

    let combined = if any_error_or_failure {
        "failure"
    } else if any_pending {
        "pending"
    } else if any_success {
        "success"
    } else {
        return Ok(None); // No status information found
    };
    Ok(Some(combined.to_string()))
}

fn map_state_to_status(state: &str) -> String {
    match state {
        "success" => "Success".to_string(),
        "failure" | "error" => "Failure".to_string(),
        "pending" => "Pending".to_string(),
        _ => "None".to_string(),
    }
}

enum BootstrapMode {
    Quick, // 1 commit, default branch only
    Owner, // 10 commits, default branch only
    Repo {
        // 50 commits, all branches
        owner: String,
        repo: String,
    },
}

impl BootstrapMode {
    fn commits_per_branch(&self) -> u8 {
        match self {
            BootstrapMode::Quick => 1,
            BootstrapMode::Owner => 10,
            BootstrapMode::Repo { .. } => 50,
        }
    }

    fn name(&self) -> &str {
        match self {
            BootstrapMode::Quick => "Quick Scan",
            BootstrapMode::Owner => "Owner Sync",
            BootstrapMode::Repo { .. } => "Deep Repo Scan",
        }
    }
}

async fn run_bootstrap_with_mode(
    pool: Pool<SqliteConnectionManager>,
    octocrabs: Octocrabs,
    client: Client,
    mode: BootstrapMode,
) {
    tokio::spawn(async move {
        log_clear();
        log_append(format!("Starting {} bootstrap", mode.name()));

        // Route to appropriate implementation based on mode
        match &mode {
            BootstrapMode::Repo { owner, repo } => {
                run_repo_bootstrap_impl(pool, octocrabs, client, owner.clone(), repo.clone()).await;
            }
            _ => {
                run_owner_bootstrap_impl(pool, octocrabs, client, mode).await;
            }
        }

        release_lock();
    });
}

async fn run_owner_bootstrap_impl(
    pool: Pool<SqliteConnectionManager>,
    octocrabs: Octocrabs,
    client: Client,
    mode: BootstrapMode,
) {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Bootstrap: failed to get DB connection: {}", e);
            log_append(format!("Error: failed to get DB connection: {}", e));
            return;
        }
    };

    for (idx, crab) in octocrabs.iter().enumerate() {
        let token_label = format!("token-{}", idx + 1);
        let start_remaining = log_rate_limit(crab, &format!("start-{}", token_label)).await;
        match list_all_repos(crab).await {
            Ok(repos) => {
                log_append(format!("Discovered {} repositories", repos.len()));
                for r in repos {
                    let default_branch = r
                        .default_branch
                        .clone()
                        .unwrap_or_else(|| "main".to_string());
                    let (owner_name, name) = if let Some(full) = r.full_name.clone() {
                        let mut parts = full.splitn(2, '/');
                        (
                            parts.next().unwrap_or_default().to_string(),
                            parts.next().unwrap_or_default().to_string(),
                        )
                    } else {
                        (
                            r.owner
                                .as_ref()
                                .map(|o| o.login.clone())
                                .unwrap_or_default(),
                            r.name.clone(),
                        )
                    };
                    let repo = GitRepo {
                        id: r.id.0,
                        owner_name,
                        name,
                        default_branch: default_branch.clone(),
                        private: r.private.unwrap_or(false),
                        language: r
                            .language
                            .as_ref()
                            .and_then(|v| v.as_str().map(|s| s.to_string())),
                    };
                    if let Err(e) = repo.upsert(&conn) {
                        log::warn!(
                            "Bootstrap: upsert repo {}/{} failed: {}",
                            repo.owner_name,
                            repo.name,
                            e
                        );
                        log_append(format!(
                            "repo {}/{}: upsert failed: {}",
                            repo.owner_name, repo.name, e
                        ));
                        continue;
                    }

                    // Process only the default branch
                    let branch_name = default_branch.clone();
                    log_rate_limit(
                        crab,
                        &format!("before-commits-{}/{}", repo.owner_name, repo.name),
                    )
                    .await;
                    let commits = match list_commits_for_branch(
                        crab,
                        &repo.owner_name,
                        &repo.name,
                        &branch_name,
                        mode.commits_per_branch(),
                    )
                    .await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            log::warn!(
                                "Bootstrap: list commits for {}/{}@{} failed: {:?}",
                                repo.owner_name,
                                repo.name,
                                branch_name,
                                e
                            );
                            log_append(format!(
                                "repo {}/{}: list commits for {} failed: {:?}",
                                repo.owner_name, repo.name, branch_name, e
                            ));
                            continue;
                        }
                    };

                    log_append(format!(
                        "repo {}/{}: {} commits on {}",
                        repo.owner_name,
                        repo.name,
                        commits.len(),
                        branch_name
                    ));

                    if let Some(first) = commits.first() {
                        let branch_egg = GitBranchEgg {
                            name: branch_name.clone(),
                            head_commit_sha: first.sha.clone(),
                            repo_id: repo.id,
                            active: true,
                        };
                        let branch = match branch_egg.upsert(&conn) {
                            Ok(br) => br,
                            Err(e) => {
                                log::warn!(
                                    "Bootstrap: upsert branch {} for {}/{} failed: {}",
                                    branch_name,
                                    repo.owner_name,
                                    repo.name,
                                    e
                                );
                                log_append(format!(
                                    "repo {}/{}: upsert branch {} failed: {}",
                                    repo.owner_name, repo.name, branch_name, e
                                ));
                                continue;
                            }
                        };

                        for c in &commits {
                            let author_name = c
                                .commit
                                .author
                                .as_ref()
                                .map(|a| a.name.clone())
                                .unwrap_or_else(|| "unknown".to_string());
                            let committer_name = c
                                .commit
                                .committer
                                .as_ref()
                                .map(|a| a.name.clone())
                                .unwrap_or_else(|| "unknown".to_string());
                            let ts_millis = c
                                .commit
                                .author
                                .as_ref()
                                .and_then(|a| a.date.as_ref())
                                .or_else(|| {
                                    c.commit.committer.as_ref().and_then(|a| a.date.as_ref())
                                })
                                .map(|d| d.timestamp_millis())
                                .unwrap_or_else(|| Utc::now().timestamp_millis());

                            let egg = GitCommitEgg {
                                sha: c.sha.clone(),
                                repo_id: repo.id,
                                message: c.commit.message.clone(),
                                author: author_name,
                                committer: committer_name,
                                timestamp: ts_millis,
                            };
                            let commit = match crate::db::git_commit::GitCommit::upsert(&egg, &conn)
                            {
                                Ok(cc) => cc,
                                Err(e) => {
                                    log::warn!(
                                        "Bootstrap: upsert commit {} for {}/{} failed: {}",
                                        c.sha,
                                        repo.owner_name,
                                        repo.name,
                                        e
                                    );
                                    log_append(format!(
                                        "repo {}/{}: upsert commit {} failed: {}",
                                        repo.owner_name, repo.name, c.sha, e
                                    ));
                                    continue;
                                }
                            };

                            let parent_shas: Vec<String> =
                                c.parents.clone().into_iter().flat_map(|p| p.sha).collect();
                            if let Err(e) = commit.add_parent_shas(parent_shas, &conn) {
                                log::debug!(
                                    "Bootstrap: add parents for {} failed: {}",
                                    commit.sha,
                                    e
                                );
                                log_append(format!(
                                    "repo {}/{}: add parents for {} failed: {}",
                                    repo.owner_name, repo.name, commit.sha, e
                                ));
                            }
                            if let Err(e) = commit.add_branch(branch.id, &conn) {
                                log::debug!(
                                    "Bootstrap: add branch relation for {} failed: {}",
                                    commit.sha,
                                    e
                                );
                                log_append(format!(
                                    "repo {}/{}: add branch relation for {} failed: {}",
                                    repo.owner_name, repo.name, commit.sha, e
                                ));
                            }

                            // Best-effort: fetch combined status
                            match get_status_state_for_sha(
                                crab,
                                &repo.owner_name,
                                &repo.name,
                                &commit.sha,
                            )
                            .await
                            {
                                Ok(Some(cs)) => {
                                    let status = map_state_to_status(&cs);
                                    let build = GitCommitBuild {
                                        repo_id: repo.id,
                                        commit_id: commit.id,
                                        check_name: "default".to_string(),
                                        status,
                                        url: format!(
                                            "https://github.com/{}/{}/commit/{}/checks",
                                            repo.owner_name, repo.name, commit.sha
                                        ),
                                        start_time: None,
                                        settle_time: None,
                                    };
                                    if let Err(e) = GitCommitBuild::upsert(&build, &conn) {
                                        log::debug!(
                                            "Bootstrap: upsert build for {} failed: {}",
                                            commit.sha,
                                            e
                                        );
                                        log_append(format!(
                                            "repo {}/{}: upsert build for {} failed: {}",
                                            repo.owner_name, repo.name, commit.sha, e
                                        ));
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    log::debug!(
                                        "Bootstrap: fetch combined status for {} failed: {:?}",
                                        commit.sha,
                                        e
                                    );
                                    log_append(format!(
                                        "repo {}/{}: fetch combined status for {} failed: {:?}",
                                        repo.owner_name, repo.name, commit.sha, e
                                    ));
                                }
                            }
                        }

                        // Sync deploy configs only for the HEAD of the default branch
                        if branch_name == repo.default_branch {
                            if let Some(head_commit) = commits.first() {
                                match sync_deploy_configs_for_commit(
                                    &octocrabs,
                                    &client,
                                    &pool,
                                    &repo.owner_name,
                                    &repo.name,
                                    repo.id,
                                    &head_commit.sha,
                                )
                                .await
                                {
                                    Ok(_) => {
                                        log_append(format!(
                                            "repo {}/{}: synced deploy configs for HEAD of default branch ({})",
                                            repo.owner_name,
                                            repo.name,
                                            &head_commit.sha[..7]
                                        ));
                                    }
                                    Err(e) => {
                                        log::debug!(
                                            "Bootstrap: sync deploy configs for {}/{} @ {} failed: {:?}",
                                            repo.owner_name,
                                            repo.name,
                                            head_commit.sha,
                                            e
                                        );
                                        // Don't fail the entire bootstrap if deploy config sync fails
                                        // Some repos may not have .deploy directories
                                    }
                                }
                            }
                        }
                    } else {
                        log_append(format!(
                            "repo {}/{}: no commits on {}",
                            repo.owner_name, repo.name, branch_name
                        ));
                    }
                }
                let end_remaining = log_rate_limit(crab, &format!("end-{}", token_label)).await;

                // Calculate usage if we have both start and end measurements
                if let (Some(start), Some(end)) = (start_remaining, end_remaining) {
                    let used = start.saturating_sub(end);
                    log_append(format!(
                        "Bootstrap used approximately {} API requests for {}",
                        used, token_label
                    ));
                }

                log_append("Bootstrap completed");
            }
            Err(e) => {
                log::warn!("Bootstrap: list repos failed: {:?}", e);
                log_append(format!("Error: list repos failed: {:?}", e));
            }
        }
    }
}

async fn run_repo_bootstrap_impl(
    pool: Pool<SqliteConnectionManager>,
    octocrabs: Octocrabs,
    client: Client,
    owner: String,
    repo_name: String,
) {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Bootstrap: failed to get DB connection: {}", e);
            log_append(format!("Error: failed to get DB connection: {}", e));
            return;
        }
    };

    log_append(format!("Scanning repo {}/{}", owner, repo_name));

    for (idx, crab) in octocrabs.iter().enumerate() {
        let token_label = format!("token-{}", idx + 1);
        let start_remaining = log_rate_limit(crab, &format!("start-{}", token_label)).await;

        // Fetch the specific repository
        let repo_data = match crab.repos(&owner, &repo_name).get().await {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "Bootstrap: fetch repo {}/{} failed: {:?}",
                    owner,
                    repo_name,
                    e
                );
                log_append(format!(
                    "Error: fetch repo {}/{} failed: {:?}",
                    owner, repo_name, e
                ));
                continue;
            }
        };

        let default_branch = repo_data
            .default_branch
            .clone()
            .unwrap_or_else(|| "main".to_string());

        let repo = GitRepo {
            id: repo_data.id.0,
            owner_name: owner.clone(),
            name: repo_name.clone(),
            default_branch: default_branch.clone(),
            private: repo_data.private.unwrap_or(false),
            language: repo_data
                .language
                .as_ref()
                .and_then(|v| v.as_str().map(|s| s.to_string())),
        };

        if let Err(e) = repo.upsert(&conn) {
            log::warn!(
                "Bootstrap: upsert repo {}/{} failed: {}",
                owner,
                repo_name,
                e
            );
            log_append(format!(
                "repo {}/{}: upsert failed: {}",
                owner, repo_name, e
            ));
            return;
        }

        log_append(format!(
            "repo {}/{}: fetching all branches",
            owner, repo_name
        ));

        // Fetch all branches for this repo
        let branches = match list_all_branches(crab, &owner, &repo_name).await {
            Ok(b) => b,
            Err(e) => {
                log::warn!(
                    "Bootstrap: list branches for {}/{} failed: {:?}",
                    owner,
                    repo_name,
                    e
                );
                log_append(format!(
                    "repo {}/{}: list branches failed: {:?}",
                    owner, repo_name, e
                ));
                return;
            }
        };

        log_append(format!(
            "repo {}/{}: processing {} branches",
            owner,
            repo_name,
            branches.len()
        ));

        for branch_data in branches {
            let branch_name = branch_data.name.clone();

            log_rate_limit(
                crab,
                &format!("before-commits-{}/{}@{}", owner, repo_name, branch_name),
            )
            .await;

            let commits = match list_commits_for_branch(
                crab,
                &owner,
                &repo_name,
                &branch_name,
                50, // Deep scan: 50 commits per branch
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    log::warn!(
                        "Bootstrap: list commits for {}/{}@{} failed: {:?}",
                        owner,
                        repo_name,
                        branch_name,
                        e
                    );
                    log_append(format!(
                        "repo {}/{}: list commits for {} failed: {:?}",
                        owner, repo_name, branch_name, e
                    ));
                    continue;
                }
            };

            log_append(format!(
                "repo {}/{}: {} commits on {}",
                owner,
                repo_name,
                commits.len(),
                branch_name
            ));

            if let Some(first) = commits.first() {
                let branch_egg = GitBranchEgg {
                    name: branch_name.clone(),
                    head_commit_sha: first.sha.clone(),
                    repo_id: repo.id,
                    active: true,
                };
                let branch = match branch_egg.upsert(&conn) {
                    Ok(br) => br,
                    Err(e) => {
                        log::warn!(
                            "Bootstrap: upsert branch {} for {}/{} failed: {}",
                            branch_name,
                            owner,
                            repo_name,
                            e
                        );
                        log_append(format!(
                            "repo {}/{}: upsert branch {} failed: {}",
                            owner, repo_name, branch_name, e
                        ));
                        continue;
                    }
                };

                for c in commits {
                    let author_name = c
                        .commit
                        .author
                        .as_ref()
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    let committer_name = c
                        .commit
                        .committer
                        .as_ref()
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    let ts_millis = c
                        .commit
                        .author
                        .as_ref()
                        .and_then(|a| a.date.as_ref())
                        .or_else(|| c.commit.committer.as_ref().and_then(|a| a.date.as_ref()))
                        .map(|d| d.timestamp_millis())
                        .unwrap_or_else(|| Utc::now().timestamp_millis());

                    let egg = GitCommitEgg {
                        sha: c.sha.clone(),
                        repo_id: repo.id,
                        message: c.commit.message.clone(),
                        author: author_name,
                        committer: committer_name,
                        timestamp: ts_millis,
                    };
                    let commit = match crate::db::git_commit::GitCommit::upsert(&egg, &conn) {
                        Ok(cc) => cc,
                        Err(e) => {
                            log::warn!(
                                "Bootstrap: upsert commit {} for {}/{} failed: {}",
                                c.sha,
                                owner,
                                repo_name,
                                e
                            );
                            log_append(format!(
                                "repo {}/{}: upsert commit {} failed: {}",
                                owner, repo_name, c.sha, e
                            ));
                            continue;
                        }
                    };

                    let parent_shas: Vec<String> =
                        c.parents.clone().into_iter().flat_map(|p| p.sha).collect();
                    if let Err(e) = commit.add_parent_shas(parent_shas, &conn) {
                        log::debug!("Bootstrap: add parents for {} failed: {}", commit.sha, e);
                    }
                    if let Err(e) = commit.add_branch(branch.id, &conn) {
                        log::debug!(
                            "Bootstrap: add branch relation for {} failed: {}",
                            commit.sha,
                            e
                        );
                    }

                    // Fetch build status for each commit
                    match get_status_state_for_sha(crab, &owner, &repo_name, &commit.sha).await {
                        Ok(Some(cs)) => {
                            let status = map_state_to_status(&cs);
                            let build = GitCommitBuild {
                                repo_id: repo.id,
                                commit_id: commit.id,
                                check_name: "default".to_string(),
                                status,
                                url: format!(
                                    "https://github.com/{}/{}/commit/{}/checks",
                                    owner, repo_name, commit.sha
                                ),
                                start_time: None,
                                settle_time: None,
                            };
                            if let Err(e) = GitCommitBuild::upsert(&build, &conn) {
                                log::debug!(
                                    "Bootstrap: upsert build for {} failed: {}",
                                    commit.sha,
                                    e
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log::debug!(
                                "Bootstrap: fetch combined status for {} failed: {:?}",
                                commit.sha,
                                e
                            );
                        }
                    }
                }

                // Sync deploy configs only for the HEAD of the default branch
                if branch_name == repo.default_branch {
                    let head_commit = &branch_egg.head_commit_sha;
                    match sync_deploy_configs_for_commit(
                        &octocrabs,
                        &client,
                        &pool,
                        &owner,
                        &repo_name,
                        repo.id as u64,
                        head_commit,
                    )
                    .await
                    {
                        Ok(_) => {
                            log_append(format!(
                                "repo {}/{}: synced deploy configs for HEAD of default branch ({})",
                                owner,
                                repo_name,
                                &head_commit[..7]
                            ));
                        }
                        Err(e) => {
                            log::debug!(
                                "Bootstrap: sync deploy configs for {}/{} @ {} failed: {:?}",
                                owner,
                                repo_name,
                                head_commit,
                                e
                            );
                        }
                    }
                }
            } else {
                log_append(format!(
                    "repo {}/{}: no commits on {}",
                    owner, repo_name, branch_name
                ));
            }
        }

        let end_remaining = log_rate_limit(crab, &format!("end-{}", token_label)).await;

        // Calculate usage if we have both start and end measurements
        if let (Some(start), Some(end)) = (start_remaining, end_remaining) {
            let used = start.saturating_sub(end);
            log_append(format!(
                "Deep repo scan used approximately {} API requests for {}",
                used, token_label
            ));
        }

        log_append(format!(
            "Deep repo scan completed for {}/{}",
            owner, repo_name
        ));
        break; // Only use the first token that works
    }
}

async fn run_bootstrap(pool: Pool<SqliteConnectionManager>, octocrabs: Octocrabs, client: Client) {
    run_bootstrap_with_mode(pool, octocrabs, client, BootstrapMode::Quick).await;
}

#[post("/bootstrap")]
pub async fn bootstrap(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
    client: web::Data<Client>,
) -> impl Responder {
    run_bootstrap(
        pool.get_ref().clone(),
        octocrabs.get_ref().clone(),
        client.get_ref().clone(),
    )
    .await;

    HttpResponse::build(StatusCode::ACCEPTED)
        .content_type("text/html; charset=utf-8")
        .body("Bootstrap started. This may take a few minutes.")
}

#[post("/bootstrap/quick")]
pub async fn bootstrap_quick(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
    client: web::Data<Client>,
) -> impl Responder {
    if !try_acquire_lock() {
        return HttpResponse::build(StatusCode::CONFLICT)
            .content_type("text/html; charset=utf-8")
            .body("A bootstrap task is already running. Please wait for it to complete.");
    }

    run_bootstrap_with_mode(
        pool.get_ref().clone(),
        octocrabs.get_ref().clone(),
        client.get_ref().clone(),
        BootstrapMode::Quick,
    )
    .await;

    HttpResponse::build(StatusCode::ACCEPTED)
        .content_type("text/html; charset=utf-8")
        .body("Quick scan started.")
}

#[post("/bootstrap/owner")]
pub async fn bootstrap_owner(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
    client: web::Data<Client>,
) -> impl Responder {
    if !try_acquire_lock() {
        return HttpResponse::build(StatusCode::CONFLICT)
            .content_type("text/html; charset=utf-8")
            .body("A bootstrap task is already running. Please wait for it to complete.");
    }

    run_bootstrap_with_mode(
        pool.get_ref().clone(),
        octocrabs.get_ref().clone(),
        client.get_ref().clone(),
        BootstrapMode::Owner,
    )
    .await;

    HttpResponse::build(StatusCode::ACCEPTED)
        .content_type("text/html; charset=utf-8")
        .body("Owner sync started.")
}

#[derive(serde::Deserialize)]
pub struct RepoBootstrapRequest {
    owner: String,
    repo: String,
}

#[post("/bootstrap/repo")]
pub async fn bootstrap_repo(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
    client: web::Data<Client>,
    req: web::Json<RepoBootstrapRequest>,
) -> impl Responder {
    if !try_acquire_lock() {
        return HttpResponse::build(StatusCode::CONFLICT)
            .content_type("text/html; charset=utf-8")
            .body("A bootstrap task is already running. Please wait for it to complete.");
    }

    // Validate that the repo exists
    let mut repo_found = false;
    for crab in octocrabs.iter() {
        if (crab.repos(&req.owner, &req.repo).get().await).is_ok() {
            repo_found = true;
            break;
        }
    }

    if !repo_found {
        release_lock();
        return HttpResponse::build(StatusCode::NOT_FOUND)
            .content_type("text/html; charset=utf-8")
            .body(format!(
                "Repository {}/{} not found or not accessible.",
                req.owner, req.repo
            ));
    }

    run_bootstrap_with_mode(
        pool.get_ref().clone(),
        octocrabs.get_ref().clone(),
        client.get_ref().clone(),
        BootstrapMode::Repo {
            owner: req.owner.clone(),
            repo: req.repo.clone(),
        },
    )
    .await;

    HttpResponse::build(StatusCode::ACCEPTED)
        .content_type("text/html; charset=utf-8")
        .body(format!(
            "Deep repo scan started for {}/{}.",
            req.owner, req.repo
        ))
}

#[get("/bootstrap/log")]
pub async fn bootstrap_log() -> impl Responder {
    let log_text = get_bootstrap_log()
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| "log unavailable".to_string());

    // Split into lines and reverse for flex-direction: column-reverse
    let lines: Vec<&str> = log_text.lines().collect();

    let markup = maud::html! {
        @for line in lines.iter().rev() {
            div class="log-line" { (line) }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

#[get("/rate-limits")]
pub async fn rate_limits(octocrabs: web::Data<Octocrabs>) -> impl Responder {
    let mut results = Vec::new();

    for (idx, crab) in octocrabs.iter().enumerate() {
        let token_num = idx + 1;
        match crab.ratelimit().get().await {
            Ok(rate_info) => {
                // Convert Unix timestamp to New York time
                use chrono::{TimeZone, Utc};
                use chrono_tz::America::New_York;
                let reset_dt = Utc
                    .timestamp_opt(rate_info.resources.core.reset as i64, 0)
                    .single()
                    .map(|dt| {
                        let ny_time = dt.with_timezone(&New_York);
                        ny_time.format("%Y-%m-%d %H:%M:%S %Z").to_string()
                    })
                    .unwrap_or_else(|| "Unknown".to_string());

                results.push((
                    token_num,
                    Some(rate_info.resources.core.remaining),
                    Some(rate_info.resources.core.limit),
                    Some(reset_dt),
                ));
            }
            Err(e) => {
                log::debug!(
                    "Failed to fetch rate limit for token {}: {:?}",
                    token_num,
                    e
                );
                results.push((token_num, None, None, None));
            }
        }
    }

    let markup = maud::html! {
        @for (token_num, remaining, limit, reset) in results {
            div class="rate-limit-row" {
                div class="rate-limit-token" { "Token " (token_num) }
                @if let (Some(rem), Some(lim), Some(rst)) = (remaining, limit, reset) {
                    div class="rate-limit-usage" {
                        span class="rate-limit-numbers" { (rem) " / " (lim) }
                        span class="rate-limit-reset" { " (resets at " (rst) ")" }
                    }
                } @else {
                    div class="rate-limit-error" { "Unable to fetch" }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
