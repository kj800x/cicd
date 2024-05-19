use crate::{prelude::*, PortainerConfig};
use std::collections::HashMap;

use async_graphql::{Context, Object, Result, SimpleObject};
use serde_variant::to_variant_name;

#[derive(Clone, SimpleObject)]
pub struct TrackedBuild {
    id: i64,
    sha: String,
    message: String,
    timestamp: i64,
    build_status: Option<String>,
}

#[derive(Clone, SimpleObject)]
pub struct TrackedBuildAndRepo {
    id: i64,
    sha: String,
    message: String,
    timestamp: i64,
    build_status: Option<String>,
    repo_name: String,
    repo_owner_name: String,
}

pub struct Machine {
    id: u64,
    name: String,
    url: String,
}

#[derive(Clone, SimpleObject)]
pub struct DockerImage {
    id: String,
    labels: Option<HashMap<String, String>>,
}

pub struct DockerContainer {
    id: String,
    name: String,
    image_descriptor: String,
    image: DockerImage,
    created: u64,
    labels: Option<HashMap<String, String>>,
    state: String,
}

struct RepoCoords {
    owner_name: String,
    repo_name: String,
}

impl DockerContainer {
    fn repo_coords(&self) -> Option<RepoCoords> {
        let repo = self
            .image
            .clone()
            .labels?
            .get("org.opencontainers.image.source")?
            .clone();

        let re =
            regex::Regex::new(r"^https?://github.com/(?P<owner_name>[^/]+)/(?P<repo_name>[^/]+)$")
                .unwrap();
        let Some(caps) = re.captures(&repo) else {
            return None;
        };

        Some(RepoCoords {
            owner_name: caps["owner_name"].to_string(),
            repo_name: caps["repo_name"].to_string(),
        })
    }
}

#[Object]
impl DockerContainer {
    async fn id(&self) -> &str {
        &self.id
    }

    async fn name(&self) -> &str {
        &self.name
    }

    async fn image_descriptor(&self) -> &str {
        &self.image_descriptor
    }

    async fn image(&self) -> DockerImage {
        (&self.image).clone()
    }

    async fn created(&self) -> u64 {
        self.created
    }

    async fn labels(&self) -> Option<&HashMap<String, String>> {
        self.labels.as_ref()
    }

    async fn state(&self) -> &str {
        &self.state
    }

    async fn is_first_party(&self) -> bool {
        let maybe_first_party: Option<bool> = (|| -> Option<bool> {
            let repo_coords = self.repo_coords()?;
            // TODO: This shouldn't be checked in the resolver, should be configured as a Data on startup
            let first_party_owners = std::env::var("FIRST_PARTY_OWNERS").ok()?;
            let first_party_owners: Vec<String> = first_party_owners
                .split(',')
                .map(|x| x.to_string())
                .collect();

            Some(first_party_owners.contains(&repo_coords.owner_name))
        })();

        maybe_first_party.unwrap_or(false)
    }

    pub async fn latest_build<'a>(&self, ctx: &Context<'a>) -> Option<TrackedBuild> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();
        let conn = pool.get().unwrap();
        let repo_coords = self.repo_coords()?;
        let repo = get_repo(&conn, repo_coords.owner_name, repo_coords.repo_name).ok()??;
        let branch = get_branch(&conn, repo.id, repo.default_branch).ok()??;
        let commit = get_commit(&conn, repo.id, branch.head_commit_sha).ok()??;

        Some(TrackedBuild {
            id: commit.id,
            sha: commit.sha,
            message: commit.message,
            timestamp: commit.timestamp,
            build_status: Some(to_variant_name(&commit.build_status).unwrap().to_string()),
        })
    }

    pub async fn current_build<'a>(&self, ctx: &Context<'a>) -> Option<TrackedBuild> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();
        let conn = pool.get().unwrap();
        let repo_coords = self.repo_coords()?;
        let repo = get_repo(&conn, repo_coords.owner_name, repo_coords.repo_name).ok()??;
        let sha = self
            .image
            .clone()
            .labels?
            .get("org.opencontainers.image.revision")?
            .clone();
        let commit = get_commit(&conn, repo.id, sha).ok()??;

        Some(TrackedBuild {
            id: commit.id,
            sha: commit.sha,
            message: commit.message,
            timestamp: commit.timestamp,
            build_status: Some(to_variant_name(&commit.build_status).unwrap().to_string()),
        })
    }
}

#[Object]
impl Machine {
    async fn id(&self) -> u64 {
        self.id
    }

    async fn name(&self) -> &str {
        &self.name
    }

    async fn url(&self) -> &str {
        &self.url
    }

    async fn containers<'a>(&self, ctx: &Context<'a>) -> Result<Vec<DockerContainer>> {
        let pconfig = ctx.data_unchecked::<PortainerConfig>();

        Ok(crate::portainer::get_endpoint(self.id, pconfig)
            .await
            .map(|endpoint| {
                let snapshot_raw = &endpoint.snapshots.get(0).unwrap().docker_snapshot_raw;

                snapshot_raw
                    .containers
                    .clone()
                    .into_iter()
                    .map(|x| {
                        let image = snapshot_raw
                            .images
                            .iter()
                            .find(|y| y.id == x.image_id)
                            .map(|x| DockerImage {
                                id: x.id.clone(),
                                labels: x.labels.clone(),
                            })
                            .unwrap();

                        DockerContainer {
                            id: x.id,
                            name: x.names.get(0).unwrap().trim_start_matches("/").to_string(),
                            image_descriptor: x.image,
                            image: image,
                            created: x.created,
                            labels: x.labels,
                            state: x.state,
                        }
                    })
                    .collect()
            })
            .unwrap())
    }
}

impl From<EndpointBrief> for Machine {
    fn from(endpoint: EndpointBrief) -> Self {
        Machine {
            id: endpoint.id,
            name: endpoint.name,
            url: endpoint.public_url,
        }
    }
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    async fn machines<'a>(&self, ctx: &Context<'a>) -> Result<Vec<Machine>> {
        let pconfig = ctx.data_unchecked::<PortainerConfig>();
        Ok(get_endpoints(pconfig)
            .await
            .unwrap()
            .into_iter()
            .map(|x| Into::<Machine>::into(x))
            .collect())
    }

    async fn recent_builds<'a>(&self, ctx: &Context<'a>) -> Result<Vec<TrackedBuildAndRepo>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        let since = Utc::now() - chrono::Duration::hours(1);
        let commits = get_commits_since(&conn, since.timestamp_millis());

        Ok(commits
            .unwrap()
            .into_iter()
            .map(|x| TrackedBuildAndRepo {
                id: x.commit.id,
                sha: x.commit.sha,
                message: x.commit.message,
                timestamp: x.commit.timestamp,
                build_status: Some(to_variant_name(&x.commit.build_status).unwrap().to_string()),
                repo_name: x.repo.name,
                repo_owner_name: x.repo.owner_name,
            })
            .collect())
    }
}

pub async fn index_graphiql() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(GraphiQLSource::build().endpoint("/api/graphql").finish())
}
