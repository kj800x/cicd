use crate::prelude::*;

pub async fn manual_hello() -> impl Responder {
    HttpResponse::Ok().body("Hey there!")
}

pub async fn sync_deploy_configs() -> impl Responder {
    HttpResponse::Ok().body("Syncing deploy configs...")
}

#[get("/api/sync-repo/{owner}/{repo}")]
pub async fn sync_repo_deploy_configs(path: web::Path<(String, String)>) -> impl Responder {
    let (owner, repo_name) = path.into_inner();
    log::info!(
        "Request to sync deploy configs for {}/{}...",
        owner,
        repo_name
    );

    // todo!();
    HttpResponse::Ok().body(format!("Synced deploy configs for {}/{}", owner, repo_name))
}

#[get("/api/sync-all")]
pub async fn sync_all_deploy_configs() -> impl Responder {
    log::info!("Request to sync all deploy configs...");

    // todo!();
    HttpResponse::Ok().body("Synced all deploy configs")
}
