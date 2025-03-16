use crate::prelude::*;

pub async fn manual_hello() -> impl Responder {
    HttpResponse::Ok().body("Hey there!")
}
