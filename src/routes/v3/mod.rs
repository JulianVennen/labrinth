pub use super::ApiError;
use crate::util::cors::default_cors;
use actix_web::{web, HttpResponse};
use serde_json::json;

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("v3")
            .wrap(default_cors())
            .route("", web::get().to(hello_world)),
    );
}

pub async fn hello_world() -> Result<HttpResponse, ApiError> {
    Ok(HttpResponse::Ok().json(json!({
        "hello": "world",
    })))
}
