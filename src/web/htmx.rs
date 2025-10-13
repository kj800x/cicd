use actix_web::Scope;

use crate::prelude::*;

#[get("/htmx.min.js")]
pub async fn htmx_js() -> impl Responder {
    HttpResponse::Ok()
        .content_type("application/javascript; charset=utf-8")
        .insert_header(("Cache-Control", "public, max-age=31536000"))
        .body(include_str!("htmx.min.js"))
}

#[get("/idiomorph.min.js")]
pub async fn idiomorph_js() -> impl Responder {
    HttpResponse::Ok()
        .content_type("application/javascript; charset=utf-8")
        .insert_header(("Cache-Control", "public, max-age=31536000"))
        .body(include_str!("idiomorph.min.js"))
}

#[get("/idiomorph-ext.min.js")]
pub async fn idiomorph_ext_js() -> impl Responder {
    HttpResponse::Ok()
        .content_type("application/javascript; charset=utf-8")
        .insert_header(("Cache-Control", "public, max-age=31536000"))
        .body(include_str!("idiomorph-ext.min.js"))
}

#[get("/styles.css")]
pub async fn styles_css() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/css; charset=utf-8")
        .insert_header(("Cache-Control", "public, max-age=31536000"))
        .body(include_str!("styles.css"))
}

pub fn assets() -> Scope {
    web::scope("/assets")
        .service(htmx_js)
        .service(idiomorph_js)
        .service(idiomorph_ext_js)
        .service(styles_css)
}
