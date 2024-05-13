use crate::{prelude::*, PortainerConfig};

// #[get("/api/class")]
// async fn event_class_listing(
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     let classes = get_classes(&pool, user_id.into_inner()?).await.unwrap();
//     Ok(web::Json(classes))
// }

// #[get("/api/ui/homepage")]
// async fn home_page_omnibus(
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     let uid = user_id.into_inner()?;

//     let classes = get_classes(&pool, uid).await.unwrap();

//     let hydrated_classes: Vec<HydratedClass> = join_all(classes.iter().map(|x| async {
//         HydratedClass {
//             id: x.id,
//             name: x.name.clone(),
//             latest: get_latest_event(&pool, x.id, uid).await.unwrap(),
//         }
//     }))
//     .await;

//     Ok(web::Json(hydrated_classes))
// }

// #[derive(Debug, Serialize, Deserialize)]
// struct StatsResponse {
//     class: ClassResult,
//     events: Vec<EventResult>,
// }

// #[get("/api/ui/stats/{id}")]
// async fn stats_page_omnibus(
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
//     id: web::Path<i64>,
// ) -> Result<impl Responder, actix_web::Error> {
//     let class_id = id.into_inner();
//     let uid = user_id.into_inner()?;

//     return match get_class(&pool, class_id, uid).await.unwrap() {
//         Some(class) => {
//             let events = get_events(&pool, class_id, uid).await.unwrap();
//             Ok(web::Json(StatsResponse { class, events }))
//         }
//         None => Err(error::ErrorNotFound("Event class not found")),
//     };
// }

// #[post("/api/class")]
// async fn event_class_create(
//     create_class: Json<CreateClass>,
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     let class = insert_class(&pool, create_class.into_inner(), user_id.into_inner()?)
//         .await
//         .unwrap();
//     Ok(web::Json(class))
// }

// #[put("/api/class/{id}")]
// async fn event_class_update(
//     create_class: Json<CreateClass>,
//     id: web::Path<i64>,
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     let class = update_class(
//         &pool,
//         id.into_inner(),
//         create_class.into_inner(),
//         user_id.into_inner()?,
//     )
//     .await
//     .unwrap();
//     Ok(web::Json(class))
// }

// #[delete("/api/class/{id}")]
// async fn event_class_delete(
//     id: web::Path<i64>,
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     delete_class(&pool, id.into_inner(), user_id.into_inner()?)
//         .await
//         .unwrap();

//     Ok(HttpResponse::NoContent())
// }

// #[get("/api/class/{class_id}/events")]
// async fn event_class_events(
//     class_id: web::Path<i64>,
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     let events = get_events(&pool, class_id.into_inner(), user_id.into_inner()?)
//         .await
//         .unwrap();
//     Ok(web::Json(events))
// }

// #[post("/api/class/{class_id}/events")]
// async fn record_event(
//     create_event: Json<CreateEvent>,
//     class_id: web::Path<i64>,
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     let event = insert_event(
//         &pool,
//         class_id.into_inner(),
//         create_event.into_inner(),
//         user_id.into_inner()?,
//     )
//     .await
//     .unwrap();
//     Ok(web::Json(event))
// }

// #[delete("/api/class/{class_id}/event/{event_id}")]
// async fn delete_event(
//     path_params: web::Path<(i64, i64)>,
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     let (class_id, event_id) = path_params.into_inner();

//     db_delete_event(&pool, class_id, event_id, user_id.into_inner()?)
//         .await
//         .unwrap();

//     Ok(HttpResponse::NoContent())
// }

// #[get("/api/class/{class_id}/events/latest")]
// async fn event_class_latest_event(
//     class_id: web::Path<i64>,
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     let event = get_latest_event(&pool, class_id.into_inner(), user_id.into_inner()?)
//         .await
//         .unwrap();
//     Ok(web::Json(event))
// }

// async fn manual_hello() -> impl Responder {
//     HttpResponse::Ok().body("Hey there!")
// }

// #[get("/api/auth")]
// async fn profile(
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     user_id: UserId,
// ) -> Result<impl Responder, actix_web::Error> {
//     let profile = fetch_profile(&pool, user_id.into_inner()?).await?;

//     Ok(web::Json(profile))
// }

// #[post("/api/auth")]
// async fn login(
//     login: Json<Login>,
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     session: Session,
// ) -> Result<impl Responder, actix_web::Error> {
//     let uid = authenticate(&pool, login.username.clone(), &login.password).await?;

//     session.insert("user_id", uid)?;

//     Ok(HttpResponse::Ok().body("Login success!"))
// }

// #[post("/api/auth/register")]
// async fn register(
//     registration: Json<Registration>,
//     pool: web::Data<Pool<SqliteConnectionManager>>,
//     session: Session,
// ) -> Result<impl Responder, actix_web::Error> {
//     if std::env::var("ALLOW_REGISTRATION").unwrap_or("false".to_string()) == "false" {
//         return Ok(HttpResponse::Forbidden().body("Registration is disabled"));
//     }

//     let reg = registration.into_inner();

//     if reg.username.trim().is_empty() {
//         return Err(error::ErrorBadRequest("username cannot be empty"));
//     }
//     if reg.password.trim().is_empty() {
//         return Err(error::ErrorBadRequest("password cannot be empty"));
//     }
//     if reg.name.trim().is_empty() {
//         return Err(error::ErrorBadRequest("name cannot be empty"));
//     }

//     let uid = sign_up(&pool, reg).await?;

//     session.insert("user_id", uid)?;

//     Ok(HttpResponse::Ok().body("Registration success!"))
// }

#[get("/api/portainer/endpoints")]
async fn portainer_endpoints(portainer_config: web::Data<PortainerConfig>) -> impl Responder {
    let result = crate::portainer::get_endpoints((**portainer_config).clone())
        .await
        .unwrap();

    web::Json(result)
}

#[get("/api/portainer/endpoints/{id}")]
async fn portainer_endpoint(
    id: web::Path<u64>,
    portainer_config: web::Data<PortainerConfig>,
) -> impl Responder {
    let result = crate::portainer::get_endpoint(id.into_inner(), (**portainer_config).clone())
        .await
        .unwrap();

    web::Json(result)
}

pub async fn manual_hello() -> impl Responder {
    HttpResponse::Ok().body("Hey there!")
}
