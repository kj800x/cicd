use crate::prelude::*;
use indoc::indoc;

/// Every migration, in order. Append only; never edit an existing entry.
pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(indoc! { r#"
          CREATE TABLE git_repo (
              id INTEGER PRIMARY KEY NOT NULL,
              owner_name TEXT NOT NULL,
              name TEXT NOT NULL,
              default_branch TEXT NOT NULL,
              private BOOLEAN NOT NULL,
              language TEXT
          );

          CREATE TABLE git_branch (
              id INTEGER PRIMARY KEY NOT NULL,
              name TEXT NOT NULL,
              head_commit_sha TEXT NOT NULL,
              repo_id INTEGER NOT NULL,
              active BOOLEAN NOT NULL DEFAULT TRUE,
              FOREIGN KEY(repo_id) REFERENCES git_repo(id)
          );

          CREATE TABLE git_commit (
              id INTEGER PRIMARY KEY NOT NULL,
              sha TEXT NOT NULL,
              repo_id INTEGER NOT NULL,
              message TEXT NOT NULL,
              author TEXT NOT NULL,
              committer TEXT NOT NULL,
              timestamp INTEGER NOT NULL,
              UNIQUE(sha, repo_id),
              FOREIGN KEY(repo_id) REFERENCES git_repo(id)
          );

          CREATE TABLE git_commit_parent (
              commit_id INTEGER NOT NULL,
              parent_sha TEXT NOT NULL,
              PRIMARY KEY(commit_id, parent_sha),
              FOREIGN KEY(commit_id) REFERENCES git_commit(id)
          );

          CREATE TABLE git_commit_branch (
              commit_id INTEGER NOT NULL,
              branch_id INTEGER NOT NULL,
              PRIMARY KEY(commit_id, branch_id),
              FOREIGN KEY(commit_id) REFERENCES git_commit(id),
              FOREIGN KEY(branch_id) REFERENCES git_branch(id)
          );

          CREATE TABLE git_commit_build (
              repo_id INTEGER NOT NULL,
              commit_id INTEGER NOT NULL,
              check_name TEXT NOT NULL,
              status TEXT NOT NULL,
              url TEXT NOT NULL,
              start_time INTEGER,
              settle_time INTEGER,
              PRIMARY KEY(repo_id, commit_id, check_name),
              FOREIGN KEY(repo_id) REFERENCES git_repo(id),
              FOREIGN KEY(commit_id) REFERENCES git_commit(id)
          );

          CREATE TABLE deploy_config (
              name TEXT NOT NULL,
              team TEXT NOT NULL,
              kind TEXT NOT NULL,
              config_repo_id INTEGER NOT NULL,
              artifact_repo_id INTEGER,
              active BOOLEAN NOT NULL DEFAULT TRUE,
              PRIMARY KEY(name),
              FOREIGN KEY(config_repo_id) REFERENCES git_repo(id),
              FOREIGN KEY(artifact_repo_id) REFERENCES git_repo(id)
          );

          CREATE TABLE deploy_config_version (
              name TEXT NOT NULL,
              config_repo_id INTEGER NOT NULL,
              config_commit_sha TEXT NOT NULL,
              hash TEXT NOT NULL,
              PRIMARY KEY(name, config_repo_id, config_commit_sha),
              FOREIGN KEY(name) REFERENCES deploy_config(name),
              FOREIGN KEY(config_repo_id) REFERENCES git_repo(id)
          );

          CREATE TABLE deploy_event (
              name TEXT NOT NULL,
              timestamp INTEGER NOT NULL,
              initiator TEXT NOT NULL,
              config_sha TEXT,
              artifact_sha TEXT,
              artifact_branch TEXT,
              config_branch TEXT,
              prev_artifact_sha TEXT,
              prev_config_sha TEXT,
              artifact_repo_id INTEGER,
              config_repo_id INTEGER,
              config_version_hash TEXT,
              prev_config_version_hash TEXT
          );
          CREATE INDEX IF NOT EXISTS idx_deploy_event_name_ts ON deploy_event(name, timestamp);
      "#}),
        // Track the GitHub App that produced each check run, so deploy configs
        // can eventually depend on specific checks (keyed by app + name) rather
        // than on all checks. Nullable: legacy rows and the REST scan (whose
        // typed model omits the app) leave this empty.
        M::up(indoc! { r#"
          ALTER TABLE git_commit_build ADD COLUMN app_id INTEGER;
        "#}),
        // Blockers: a per-config hold with a reason. While a config has an
        // active blocker (cleared_at IS NULL), manual deploys are refused and
        // autodeploy, once it exists, is suspended. Rows are never deleted;
        // clearing records who and when, so the history stays readable.
        // Rollback and deploy freezes both create blockers.
        M::up(indoc! { r#"
          CREATE TABLE blocker (
              id INTEGER PRIMARY KEY NOT NULL,
              config_name TEXT NOT NULL,
              reason TEXT NOT NULL,
              created_by TEXT NOT NULL,
              created_at INTEGER NOT NULL,
              cleared_by TEXT,
              cleared_at INTEGER
          );
          CREATE INDEX IF NOT EXISTS idx_blocker_config_active ON blocker(config_name, cleared_at);
        "#}),
        // Revisions: the immutable record of each deploy or undeploy, one row
        // plus one revision_parameter row per parameter with the value that
        // was deployed and the channel (branch) it was resolved from, NULL
        // when pinned. `actor` is where the action came from ("web", "mcp")
        // until the app has user identity. `reason` and `patches` are
        // reserved for later changes (rollback notes, manifest patches) and
        // are NULL for now. deploy_event keeps being written alongside; the
        // history page moves to revisions in a later change.
        M::up(indoc! { r#"
          CREATE TABLE revision (
              id INTEGER PRIMARY KEY NOT NULL,
              config_name TEXT NOT NULL,
              created_at INTEGER NOT NULL,
              actor TEXT NOT NULL,
              action TEXT NOT NULL,
              reason TEXT,
              config_sha TEXT,
              config_branch TEXT,
              config_version_hash TEXT,
              patches TEXT
          );
          CREATE INDEX IF NOT EXISTS idx_revision_config_created ON revision(config_name, created_at);

          CREATE TABLE revision_parameter (
              revision_id INTEGER NOT NULL,
              name TEXT NOT NULL,
              type TEXT NOT NULL,
              value TEXT NOT NULL,
              branch TEXT,
              PRIMARY KEY(revision_id, name),
              FOREIGN KEY(revision_id) REFERENCES revision(id)
          );
        "#}),
        // Backfill revisions from the deploy events recorded before revisions
        // existed, so the history page can read revisions alone. Only events
        // older than the first real revision are copied; anything after that
        // was dual-written. Legacy events have no actor beyond "USER", so
        // they get actor "user". An event with an artifact becomes a
        // revision with one SHA parameter; the config-only and undeploy
        // shapes carry over as they are. deploy_event itself is untouched.
        M::up(indoc! { r#"
          INSERT INTO revision (config_name, created_at, actor, action, config_sha, config_branch, config_version_hash)
          SELECT de.name, de.timestamp, 'user',
                 CASE WHEN de.config_sha IS NULL THEN 'undeploy' ELSE 'deploy' END,
                 de.config_sha, de.config_branch, de.config_version_hash
          FROM deploy_event de
          WHERE de.timestamp < COALESCE((SELECT MIN(created_at) FROM revision), 9223372036854775807)
          ORDER BY de.timestamp;

          INSERT OR IGNORE INTO revision_parameter (revision_id, name, type, value, branch)
          SELECT r.id, 'SHA', 'commit', de.artifact_sha, de.artifact_branch
          FROM deploy_event de
          JOIN revision r ON r.config_name = de.name AND r.created_at = de.timestamp AND r.actor = 'user'
          WHERE de.artifact_sha IS NOT NULL;
        "#}),
        // Where cicd is in watchtower's event feed. See db/watchtower_cursor.rs.
        M::up(indoc! { r#"
          CREATE TABLE watchtower_cursor (
              id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
              after INTEGER NOT NULL
          );
        "#}),
    ])
}

pub fn migrate(mut conn: PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
    migrations()
        .to_latest(&mut conn)
        .map_err(|e| AppError::DatabaseMigration(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::revision::Revision;
    use crate::db::test_support::memory_pool_at_version;
    use rusqlite::params;

    #[test]
    fn revisions_are_backfilled_from_deploy_events() -> AppResult<()> {
        let pool = memory_pool_at_version(4);
        let conn = pool.get()?;
        let insert = "INSERT INTO deploy_event (name, timestamp, initiator, config_sha, artifact_sha, artifact_branch, config_branch, config_version_hash) VALUES (?1, ?2, 'USER', ?3, ?4, ?5, ?6, ?7)";
        // Artifact deploy, config-only deploy, undeploy, oldest first.
        conn.execute(
            insert,
            params![
                "site",
                1000,
                Some("c1"),
                Some("a1"),
                Some("master"),
                Some("master"),
                Some("h1")
            ],
        )?;
        conn.execute(
            insert,
            params![
                "cfgonly",
                2000,
                Some("c2"),
                None::<String>,
                None::<String>,
                Some("master"),
                None::<String>
            ],
        )?;
        conn.execute(
            insert,
            params![
                "site",
                3000,
                None::<String>,
                None::<String>,
                None::<String>,
                None::<String>,
                None::<String>
            ],
        )?;
        // A real revision that was dual-written after revisions existed; the
        // matching event must not be copied again.
        conn.execute(
            insert,
            params![
                "site",
                5000,
                Some("c3"),
                Some("a3"),
                Some("master"),
                Some("master"),
                None::<String>
            ],
        )?;
        conn.execute(
            "INSERT INTO revision (config_name, created_at, actor, action, config_sha, config_branch) VALUES ('site', 5000, 'mcp', 'deploy', 'c3', 'master')",
            [],
        )?;
        drop(conn);

        migrate(pool.get()?)?;
        let conn = pool.get()?;

        let site = Revision::list_for(&conn, "site", 10)?;
        assert_eq!(
            site.iter()
                .map(|r| (r.created_at, r.actor.as_str(), r.action.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (5000, "mcp", "deploy"),
                (3000, "user", "undeploy"),
                (1000, "user", "deploy")
            ]
        );
        let first = &site[2];
        assert_eq!(first.config_sha.as_deref(), Some("c1"));
        assert_eq!(first.config_version_hash.as_deref(), Some("h1"));
        assert_eq!(first.parameters.len(), 1);
        assert_eq!(first.parameters[0].value, "a1");
        assert_eq!(first.parameters[0].branch.as_deref(), Some("master"));
        assert!(site[1].parameters.is_empty(), "undeploy has no parameters");
        // The dual-written revision was left alone and got no parameter rows
        // from the backfill.
        assert!(site[0].parameters.is_empty());

        let cfgonly = Revision::list_for(&conn, "cfgonly", 10)?;
        assert_eq!(cfgonly.len(), 1);
        assert!(cfgonly[0].parameters.is_empty());
        Ok(())
    }
}
