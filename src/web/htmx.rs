use crate::prelude::*;

pub async fn htmx_js() -> impl Responder {
    HttpResponse::Ok()
        .content_type("application/javascript; charset=utf-8")
        .insert_header(("Cache-Control", "public, max-age=31536000"))
        .body(include_str!("htmx.min.js"))
}

pub async fn idiomorph_js() -> impl Responder {
    HttpResponse::Ok()
        .content_type("application/javascript; charset=utf-8")
        .insert_header(("Cache-Control", "public, max-age=31536000"))
        .body(include_str!("idiomorph.min.js"))
}

pub async fn idiomorph_ext_js() -> impl Responder {
    HttpResponse::Ok()
        .content_type("application/javascript; charset=utf-8")
        .insert_header(("Cache-Control", "public, max-age=31536000"))
        .body(include_str!("idiomorph-ext.min.js"))
}
