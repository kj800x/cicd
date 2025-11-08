#[macro_export]
macro_rules! serve_static_file {
    ($file:expr) => {{
        use actix_web::{web, HttpRequest, HttpResponse};
        use std::io::Read;
        use std::sync::Arc;
        let path = std::path::Path::new("src/res").join($file);

        if path.exists() && path.is_file() {
            let mut file = std::fs::File::open(path).unwrap();
            let mut contents = String::new();
            file.read_to_string(&mut contents).unwrap();

            web::resource(concat!("res/", $file)).route(web::get().to(|| async move {
                let path = std::path::Path::new("src/res").join($file);
                let mut file = std::fs::File::open(path).unwrap();
                let mut contents = String::new();
                file.read_to_string(&mut contents).unwrap();
                HttpResponse::Ok()
                    .append_header(("x-resource-source", "disk"))
                    .body(contents)
            }))
        } else {
            let contents = Arc::new(include_str!(concat!("res/", $file)).to_string());
            let hash = md5::compute(contents.as_str().as_bytes());
            let hash_str = Arc::new(format!("{:x}", hash));

            let c = contents.clone();
            let h = hash_str.clone();

            web::resource(concat!("res/", $file)).route(web::get().to(move |req: HttpRequest| {
                let contents = c.clone();
                let hash_str = h.clone();
                async move {
                    if let Some(if_none_match) = req.headers().get("If-None-Match") {
                        if if_none_match.to_str().unwrap_or("") == hash_str.as_str() {
                            return HttpResponse::NotModified().finish();
                        }
                    }
                    HttpResponse::Ok()
                        .append_header(("x-resource-source", "embedded"))
                        .append_header(("ETag", hash_str.as_str()))
                        .body(contents.as_str().to_string())
                }
            }))
        }
    }};
}
