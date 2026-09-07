use crate::prelude::*;
use indoc::indoc;

pub fn migrate(mut conn: PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
    let migrations: Migrations = Migrations::new(vec![
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
    ]);

    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
    migrations
        .to_latest(&mut conn)
        .map_err(|e| AppError::DatabaseMigration(e.to_string()))?;
    Ok(())
}
