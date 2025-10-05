use crate::kubernetes::deployconfig::DefiningRepo;
use octocrab::Octocrab;

pub type Octocrabs = Vec<Octocrab>;

pub trait OctocrabExt {
    async fn crab_for(&self, repo: &DefiningRepo) -> Option<&Octocrab>;
}

impl OctocrabExt for Octocrabs {
    async fn crab_for(&self, repo: &DefiningRepo) -> Option<&Octocrab> {
        if self.len() == 1 {
            return Some(&self[0]);
        }

        for crab in self {
            let result = crab
                .repos(repo.owner.clone(), repo.repo.clone())
                .get()
                .await;

            if let Ok(_) = result {
                return Some(crab);
            }
        }

        None
    }
}

pub fn initialize_octocrabs() -> Octocrabs {
    let github_pats = std::env::var("GITHUB_PATS").ok();
    let Some(github_pats) = github_pats else {
        return vec![];
    };

    github_pats
        .split(',')
        .map(|pat| Octocrab::builder().personal_token(pat).build().unwrap())
        .collect()
}
