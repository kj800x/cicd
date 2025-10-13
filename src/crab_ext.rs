use crate::kubernetes::repo::IRepo;
use octocrab::Octocrab;

pub type Octocrabs = Vec<Octocrab>;

pub trait OctocrabExt {
    async fn crab_for<T: IRepo>(&self, repo: &T) -> Option<&Octocrab>;
}

impl OctocrabExt for Octocrabs {
    async fn crab_for<T: IRepo>(&self, repo: &T) -> Option<&Octocrab> {
        if self.len() == 1 {
            return Some(&self[0]);
        }

        for crab in self {
            let result = crab.repos(repo.owner(), repo.repo()).get().await;

            if result.is_ok() {
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
        .map(|pat| {
            #[allow(clippy::expect_used)]
            Octocrab::builder()
                .personal_token(pat)
                .build()
                .expect("Failed to build Octocrab client - invalid GitHub PAT")
        })
        .collect()
}
