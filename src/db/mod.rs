use std::ops::Deref;

pub mod autodeploy_state;
pub mod deploy_config;
pub mod deploy_config_version;
pub mod deploy_event;
pub mod functions;
pub mod git_branch;
pub mod git_commit;
pub mod git_commit_branch;
pub mod git_commit_build;
pub mod git_commit_parent;
pub mod git_repo;
pub mod migrations;

pub struct ExistenceResult {
    id: u64,
}

impl Deref for ExistenceResult {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.id
    }
}
