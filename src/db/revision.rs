//! Revisions: the immutable record of each deploy or undeploy.
//!
//! A revision captures everything a deploy was made from: the config commit,
//! and for every parameter the value that was deployed and the channel it
//! was resolved from. Rollback replays a revision and the history page lists
//! them. They replaced the `deploy_event` table, which is kept only because
//! migration 5 reads it.

use chrono::Utc;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension, Row};

use crate::{
    db::{deploy_config_version::DeployConfigVersion, git_repo::GitRepo},
    error::AppResult,
    kubernetes::{deploy_handlers::DeployAction, parameters::SHA_PARAMETER, DeployConfig},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionParameter {
    pub name: String,
    /// Parameter source type, e.g. `commit`.
    pub kind: String,
    pub value: String,
    /// The channel the value was resolved from: a branch for commits, a
    /// semver range for tags (stored in the `branch` column either way).
    /// `None` means the value was pinned rather than tracked.
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub id: i64,
    pub config_name: String,
    /// Milliseconds since the epoch.
    pub created_at: i64,
    /// Where the action came from: `web` or `mcp` until users exist.
    pub actor: String,
    /// `deploy` or `undeploy`.
    pub action: String,
    pub reason: Option<String>,
    pub config_sha: Option<String>,
    pub config_branch: Option<String>,
    pub config_version_hash: Option<String>,
    /// The manifest patches active at the time, as JSON, if any.
    pub patches: Option<String>,
    pub parameters: Vec<RevisionParameter>,
}

/// What to record; the id and timestamp are assigned on insert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRevision {
    pub config_name: String,
    pub actor: String,
    pub action: String,
    pub reason: Option<String>,
    pub config_sha: Option<String>,
    pub config_branch: Option<String>,
    pub config_version_hash: Option<String>,
    pub patches: Option<String>,
    pub parameters: Vec<RevisionParameter>,
}

/// The config's active patches as the JSON stored on a revision.
fn patches_json(config: &DeployConfig) -> Option<String> {
    if config.spec.spec.patches.is_empty() {
        None
    } else {
        serde_json::to_string(&config.spec.spec.patches).ok()
    }
}

impl NewRevision {
    /// A revision recording a patch change: what is deployed stays the
    /// same, the patch set does not.
    pub fn patch_change(config: &DeployConfig, actor: &str, reason: &str) -> Self {
        let status = config.status.as_ref();
        let parameters = status
            .map(|s| {
                s.parameters
                    .iter()
                    .map(|(name, value)| RevisionParameter {
                        name: name.clone(),
                        kind: value.type_name().to_string(),
                        value: value.rendered(),
                        branch: value.channel().map(String::from),
                    })
                    .collect()
            })
            .unwrap_or_default();
        NewRevision {
            config_name: kube::ResourceExt::name_any(config),
            actor: actor.to_string(),
            action: "patch".to_string(),
            reason: Some(reason.to_string()),
            config_sha: status
                .and_then(|s| s.config.as_ref())
                .map(|c| c.sha.clone()),
            config_branch: status
                .and_then(|s| s.config.as_ref())
                .and_then(|c| c.branch.clone()),
            config_version_hash: None,
            patches: patches_json(config),
            parameters,
        }
    }

    /// Describe an executed [`DeployAction`]. Returns `None` for actions
    /// that do not change what is deployed (bounce, execute job, toggles).
    pub fn from_deploy_action(
        action: &DeployAction,
        config: &DeployConfig,
        conn: &PooledConnection<SqliteConnectionManager>,
        actor: &str,
    ) -> Option<Self> {
        match action {
            DeployAction::Deploy {
                name,
                artifact,
                config: cfg,
                values,
            } => {
                let repo = config.config_repository();
                let config_version_hash = GitRepo::get_by_name(&repo.owner, &repo.repo, conn)
                    .ok()
                    .flatten()
                    .and_then(|r| DeployConfigVersion::get_hash(name, r.id, &cfg.sha, conn).ok())
                    .flatten();
                let mut parameters: Vec<RevisionParameter> = artifact
                    .iter()
                    .map(|a| RevisionParameter {
                        name: SHA_PARAMETER.to_string(),
                        kind: "commit".to_string(),
                        value: a.sha.clone(),
                        branch: a.branch.clone(),
                    })
                    .collect();
                for (pname, value) in values {
                    parameters.push(RevisionParameter {
                        name: pname.clone(),
                        kind: value.type_name().to_string(),
                        value: value.rendered(),
                        branch: value.channel().map(String::from),
                    });
                }
                Some(NewRevision {
                    config_name: name.clone(),
                    actor: actor.to_string(),
                    action: "deploy".to_string(),
                    reason: None,
                    config_sha: Some(cfg.sha.clone()),
                    config_branch: cfg.branch.clone(),
                    config_version_hash,
                    patches: patches_json(config),
                    parameters,
                })
            }
            DeployAction::Undeploy { name } => Some(NewRevision {
                config_name: name.clone(),
                actor: actor.to_string(),
                action: "undeploy".to_string(),
                reason: None,
                config_sha: None,
                config_branch: None,
                config_version_hash: None,
                patches: None,
                parameters: vec![],
            }),
            DeployAction::Bounce { .. }
            | DeployAction::ExecuteJob { .. }
            | DeployAction::ToggleAutodeploy { .. } => None,
        }
    }
}

const COLUMNS: &str =
    "id, config_name, created_at, actor, action, reason, config_sha, config_branch, config_version_hash, patches";

impl Revision {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Revision {
            id: row.get(0)?,
            config_name: row.get(1)?,
            created_at: row.get(2)?,
            actor: row.get(3)?,
            action: row.get(4)?,
            reason: row.get(5)?,
            config_sha: row.get(6)?,
            config_branch: row.get(7)?,
            config_version_hash: row.get(8)?,
            patches: row.get(9)?,
            parameters: vec![],
        })
    }

    fn load_parameters(
        conn: &PooledConnection<SqliteConnectionManager>,
        revision_id: i64,
    ) -> AppResult<Vec<RevisionParameter>> {
        let mut stmt = conn.prepare(
            "SELECT name, type, value, branch FROM revision_parameter WHERE revision_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![revision_id], |row| {
            Ok(RevisionParameter {
                name: row.get(0)?,
                kind: row.get(1)?,
                value: row.get(2)?,
                branch: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Write a revision and its parameters atomically.
    pub fn record(
        conn: &PooledConnection<SqliteConnectionManager>,
        new: NewRevision,
    ) -> AppResult<Self> {
        let created_at = Utc::now().timestamp_millis();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO revision (config_name, created_at, actor, action, reason, config_sha, config_branch, config_version_hash, patches) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                new.config_name,
                created_at,
                new.actor,
                new.action,
                new.reason,
                new.config_sha,
                new.config_branch,
                new.config_version_hash,
                new.patches
            ],
        )?;
        let id = tx.last_insert_rowid();
        for p in &new.parameters {
            tx.execute(
                "INSERT INTO revision_parameter (revision_id, name, type, value, branch) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, p.name, p.kind, p.value, p.branch],
            )?;
        }
        tx.commit()?;
        Ok(Revision {
            id,
            config_name: new.config_name,
            created_at,
            actor: new.actor,
            action: new.action,
            reason: new.reason,
            config_sha: new.config_sha,
            config_branch: new.config_branch,
            config_version_hash: new.config_version_hash,
            patches: new.patches,
            parameters: new.parameters,
        })
    }

    pub fn get(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
    ) -> AppResult<Option<Self>> {
        let rev = conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM revision WHERE id = ?1"),
                params![id],
                Self::from_row,
            )
            .optional()?;
        match rev {
            Some(mut rev) => {
                rev.parameters = Self::load_parameters(conn, rev.id)?;
                Ok(Some(rev))
            }
            None => Ok(None),
        }
    }

    /// The newest revision for a config, if it has any.
    pub fn latest_for(
        conn: &PooledConnection<SqliteConnectionManager>,
        config_name: &str,
    ) -> AppResult<Option<Self>> {
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM revision WHERE config_name = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![config_name],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => Self::get(conn, id),
            None => Ok(None),
        }
    }

    /// Revisions for every config on a team, newest first.
    pub fn list_for_team(
        conn: &PooledConnection<SqliteConnectionManager>,
        team: &str,
        limit: usize,
    ) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM revision \
             WHERE config_name IN (SELECT name FROM deploy_config WHERE team = ?1) \
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        ))?;
        let mut revs = stmt
            .query_map(params![team, limit as i64], Self::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for rev in &mut revs {
            rev.parameters = Self::load_parameters(conn, rev.id)?;
        }
        Ok(revs)
    }

    /// The parameter value deployed under `name`, if any.
    pub fn parameter(&self, name: &str) -> Option<&RevisionParameter> {
        self.parameters.iter().find(|p| p.name == name)
    }

    /// Pair each revision with the one that preceded it for the same config,
    /// so a row can show what changed. `revisions` must be newest first; the
    /// oldest revision of each config in the list gets `None`, which only
    /// means the list did not reach further back.
    pub fn with_previous(revisions: Vec<Self>) -> Vec<(Self, Option<Self>)> {
        let mut out = Vec::with_capacity(revisions.len());
        for (i, rev) in revisions.iter().enumerate() {
            let prev = revisions[i + 1..]
                .iter()
                .find(|r| r.config_name == rev.config_name)
                .cloned();
            out.push((rev.clone(), prev));
        }
        out
    }

    /// Revisions for a config, newest first.
    pub fn list_for(
        conn: &PooledConnection<SqliteConnectionManager>,
        config_name: &str,
        limit: usize,
    ) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM revision WHERE config_name = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2"
        ))?;
        let mut revs = stmt
            .query_map(params![config_name, limit as i64], Self::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for rev in &mut revs {
            rev.parameters = Self::load_parameters(conn, rev.id)?;
        }
        Ok(revs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::migrated_memory_pool;
    use crate::kubernetes::deploy_config::{DeployConfigSpec, DeployConfigSpecFields};
    use crate::kubernetes::parameters::ParameterSource;
    use crate::kubernetes::repo::{RepositoryBranch, ShaMaybeBranch};
    use crate::kubernetes::Repository;

    fn config() -> DeployConfig {
        DeployConfig::new(
            "site",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "t".into(),
                    kind: "service".into(),
                    parameters: ParameterSource::sha_map(Some(RepositoryBranch {
                        owner: "o".into(),
                        repo: "site".into(),
                        branch: "master".into(),
                    })),
                    selections: Default::default(),
                    patches: vec![],
                    config: Repository {
                        owner: "o".into(),
                        repo: "site".into(),
                    },
                    specs: vec![],
                },
            },
        )
    }

    #[test]
    fn deploy_and_undeploy_become_revisions() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let cfg = config();

        let deploy = DeployAction::Deploy {
            name: "site".into(),
            artifact: Some(ShaMaybeBranch {
                sha: "abc".into(),
                branch: Some("master".into()),
            }),
            config: ShaMaybeBranch {
                sha: "cfg".into(),
                branch: Some("master".into()),
            },
            values: Default::default(),
        };
        let new = NewRevision::from_deploy_action(&deploy, &cfg, &conn, "web")
            .ok_or_else(|| crate::error::AppError::Internal("expected a revision".into()))?;
        assert_eq!(new.action, "deploy");
        assert_eq!(new.parameters.len(), 1);
        assert_eq!(new.parameters[0].name, SHA_PARAMETER);
        assert_eq!(new.parameters[0].value, "abc");
        let first = Revision::record(&conn, new)?;

        let undeploy = DeployAction::Undeploy {
            name: "site".into(),
        };
        let new = NewRevision::from_deploy_action(&undeploy, &cfg, &conn, "mcp")
            .ok_or_else(|| crate::error::AppError::Internal("expected a revision".into()))?;
        assert!(new.parameters.is_empty());
        let second = Revision::record(&conn, new)?;

        assert_eq!(Revision::get(&conn, first.id)?, Some(first.clone()));
        let latest = Revision::latest_for(&conn, "site")?;
        assert_eq!(latest.as_ref().map(|r| r.id), Some(second.id));
        assert_eq!(latest.as_ref().map(|r| r.actor.as_str()), Some("mcp"));
        let all = Revision::list_for(&conn, "site", 10)?;
        assert_eq!(
            all.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![second.id, first.id]
        );
        assert_eq!(all[1].parameters, first.parameters);
        assert!(Revision::latest_for(&conn, "other")?.is_none());
        Ok(())
    }

    #[test]
    fn team_listing_and_previous_pairing() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO git_repo (id, owner_name, name, default_branch, private) VALUES (1, 'o', 'r', 'master', 0)",
            [],
        )?;
        conn.execute(
            "INSERT INTO deploy_config (name, team, kind, config_repo_id, active) VALUES ('a', 'blue', 'service', 1, 1), ('b', 'blue', 'service', 1, 1), ('c', 'red', 'service', 1, 1)",
            [],
        )?;
        let rev = |name: &str, sha: &str| NewRevision {
            config_name: name.into(),
            actor: "web".into(),
            action: "deploy".into(),
            reason: None,
            config_sha: Some(sha.into()),
            config_branch: Some("master".into()),
            config_version_hash: None,
            patches: None,
            parameters: vec![RevisionParameter {
                name: SHA_PARAMETER.into(),
                kind: "commit".into(),
                value: sha.into(),
                branch: Some("master".into()),
            }],
        };
        let a1 = Revision::record(&conn, rev("a", "a1"))?;
        let b1 = Revision::record(&conn, rev("b", "b1"))?;
        let a2 = Revision::record(&conn, rev("a", "a2"))?;
        let _c1 = Revision::record(&conn, rev("c", "c1"))?;

        let blue = Revision::list_for_team(&conn, "blue", 10)?;
        assert_eq!(
            blue.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![a2.id, b1.id, a1.id]
        );
        assert_eq!(
            blue[0].parameter(SHA_PARAMETER).map(|p| p.value.as_str()),
            Some("a2")
        );
        assert!(blue[0].parameter("NOPE").is_none());

        let paired = Revision::with_previous(blue);
        assert_eq!(
            paired[0].1.as_ref().map(|r| r.id),
            Some(a1.id),
            "a2's previous is a1, skipping b1"
        );
        assert_eq!(paired[1].1, None, "b1 is the oldest b in the list");
        assert_eq!(paired[2].1, None);
        Ok(())
    }

    #[test]
    fn non_deploy_actions_record_nothing() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let cfg = config();
        for action in [
            DeployAction::Bounce {
                name: "site".into(),
            },
            DeployAction::ExecuteJob {
                name: "site".into(),
            },
            DeployAction::ToggleAutodeploy {
                name: "site".into(),
            },
        ] {
            assert!(NewRevision::from_deploy_action(&action, &cfg, &conn, "web").is_none());
        }
        Ok(())
    }

    #[test]
    fn pinned_deploy_has_no_branch() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let deploy = DeployAction::Deploy {
            name: "site".into(),
            artifact: Some(ShaMaybeBranch {
                sha: "abc".into(),
                branch: None,
            }),
            config: ShaMaybeBranch {
                sha: "cfg".into(),
                branch: Some("master".into()),
            },
            values: Default::default(),
        };
        let new = NewRevision::from_deploy_action(&deploy, &config(), &conn, "web")
            .ok_or_else(|| crate::error::AppError::Internal("expected a revision".into()))?;
        assert_eq!(new.parameters[0].branch, None);
        let rev = Revision::record(&conn, new)?;
        assert_eq!(
            Revision::get(&conn, rev.id)?.map(|r| r.parameters[0].branch.clone()),
            Some(None)
        );
        Ok(())
    }
}
