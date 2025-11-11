use crate::crab_ext::Octocrabs;
use crate::db::{
    git_branch::GitBranchEgg, git_commit::GitCommitEgg, git_commit_build::GitCommitBuild,
    git_repo::GitRepo,
};
use crate::prelude::*;
use actix_web::http::StatusCode;
use chrono::Utc;
use octocrab::Octocrab;
use std::sync::{Arc, Mutex, OnceLock};

const BOOTSTRAP_COMMITS_PER_BRANCH: u8 = 10;
const PER_PAGE: u8 = 100;

static BOOTSTRAP_LOG: OnceLock<Arc<Mutex<String>>> = OnceLock::new();

fn get_bootstrap_log() -> Arc<Mutex<String>> {
    BOOTSTRAP_LOG
        .get_or_init(|| Arc::new(Mutex::new(String::new())))
        .clone()
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

async fn log_rate_limit(crab: &Octocrab, label: &str) {
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
        }
        Err(e) => {
            log::warn!("Failed to fetch rate limit for {}: {:?}", label, e);
            log_append(format!("Failed to fetch rate limit for {}: {:?}", label, e));
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
) -> anyhow::Result<Vec<octocrab::models::repos::RepoCommit>> {
    let resp = crab
        .repos(owner, repo)
        .list_commits()
        .sha(branch)
        .per_page(BOOTSTRAP_COMMITS_PER_BRANCH)
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
    let resp = crab
        .repos(owner, repo)
        .list_statuses(sha.to_string())
        .per_page(100)
        .page(1u32)
        .send()
        .await?;

    let mut any_error_or_failure = false;
    let mut any_pending = false;
    let mut any_success = false;

    for st in resp.items {
        let state_str = format!("{:?}", st.state).to_lowercase();
        match state_str.as_str() {
            "error" | "failure" => any_error_or_failure = true,
            "pending" => any_pending = true,
            "success" => any_success = true,
            _ => {}
        }
    }

    let combined = if any_error_or_failure {
        "Failure"
    } else if any_pending {
        "Pending"
    } else if any_success {
        "Success"
    } else {
        "None"
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

async fn run_bootstrap(pool: Pool<SqliteConnectionManager>, octocrabs: Octocrabs) {
    tokio::spawn(async move {
        log_clear();
        log_append("Starting bootstrap scan");

        let conn = match pool.get() {
            Ok(c) => c,
            Err(e) => {
                log::error!("Bootstrap: failed to get DB connection: {}", e);
                log_append(format!("Error: failed to get DB connection: {}", e));
                return;
            }
        };

        for crab in &octocrabs {
            log_rate_limit(crab, "before-list-repos").await;
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
                                let commit =
                                    match crate::db::git_commit::GitCommit::upsert(&egg, &conn) {
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
                                    c.parents.into_iter().flat_map(|p| p.sha).collect();
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
                                log_rate_limit(
                                    crab,
                                    &format!("before-status-{}", commit.sha[..7].to_string()),
                                )
                                .await;
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
                        } else {
                            log_append(format!(
                                "repo {}/{}: no commits on {}",
                                repo.owner_name, repo.name, branch_name
                            ));
                        }
                    }
                    log_rate_limit(crab, "after-all-repos").await;
                    log_append("Bootstrap completed");
                }
                Err(e) => {
                    log::warn!("Bootstrap: list repos failed: {:?}", e);
                    log_append(format!("Error: list repos failed: {:?}", e));
                }
            }
        }
    });
}

#[post("/bootstrap")]
pub async fn bootstrap(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
) -> impl Responder {
    run_bootstrap(pool.get_ref().clone(), octocrabs.get_ref().clone()).await;

    HttpResponse::build(StatusCode::ACCEPTED)
        .content_type("text/html; charset=utf-8")
        .body("Bootstrap started. This may take a few minutes.")
}

#[get("/bootstrap/log")]
pub async fn bootstrap_log() -> impl Responder {
    let body = get_bootstrap_log()
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| "log unavailable".to_string());

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(maud::html! { (body) }.into_string())
}
