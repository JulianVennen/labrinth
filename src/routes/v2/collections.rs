use crate::auth::checks::{filter_authorized_collections, is_authorized_collection};
use crate::auth::get_user_from_headers;
use crate::database;
use crate::database::models::{collection_item, generate_collection_id, project_item};
use crate::file_hosting::FileHost;
use crate::models::collections::{Collection, CollectionStatus};
use crate::models::ids::base62_impl::parse_base62;
use crate::models::ids::{CollectionId, ProjectId};
use crate::models::pats::Scopes;
use crate::queue::session::AuthQueue;
use crate::routes::ApiError;
use crate::util::routes::read_from_payload;
use crate::util::validate::validation_errors_to_string;
use actix_web::web::Data;
use actix_web::{delete, get, patch, post, web, HttpRequest, HttpResponse};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use validator::Validate;

use super::project_creation::CreateError;

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(collections_get);
    cfg.service(collection_create);
    cfg.service(
        web::scope("collection")
            .service(collection_get)
            .service(collection_delete)
            .service(collection_edit)
            .service(collection_icon_edit)
            .service(delete_collection_icon),
    );
}

#[derive(Serialize, Deserialize, Validate, Clone)]
pub struct CollectionCreateData {
    #[validate(
        length(min = 3, max = 64),
        custom(function = "crate::util::validate::validate_name")
    )]
    /// The title or name of the project.
    pub title: String,
    #[validate(length(min = 3, max = 255))]
    /// A short description of the collection.
    pub description: String,
    #[validate(length(max = 32))]
    #[serde(default = "Vec::new")]
    /// A list of initial projects to use with the created collection
    pub projects: Vec<String>,
}

#[post("collection")]
pub async fn collection_create(
    req: HttpRequest,
    collection_create_data: web::Json<CollectionCreateData>,
    client: Data<PgPool>,
    redis: Data<deadpool_redis::Pool>,
    session_queue: Data<AuthQueue>,
) -> Result<HttpResponse, CreateError> {
    let collection_create_data = collection_create_data.into_inner();

    // The currently logged in user
    let current_user = get_user_from_headers(
        &req,
        &**client,
        &redis,
        &session_queue,
        Some(&[Scopes::COLLECTION_CREATE]),
    )
    .await?
    .1;

    collection_create_data
        .validate()
        .map_err(|err| CreateError::InvalidInput(validation_errors_to_string(err, None)))?;

    let mut transaction = client.begin().await?;

    let collection_id: CollectionId = generate_collection_id(&mut transaction).await?.into();

    let initial_project_ids =
        project_item::Project::get_many(&collection_create_data.projects, &mut transaction, &redis)
            .await?
            .into_iter()
            .map(|x| x.inner.id.into())
            .collect::<Vec<ProjectId>>();

    let collection_builder_actual = collection_item::CollectionBuilder {
        collection_id: collection_id.into(),
        user_id: current_user.id.into(),
        title: collection_create_data.title,
        description: collection_create_data.description,
        status: CollectionStatus::Listed,
        projects: initial_project_ids
            .iter()
            .copied()
            .map(|x| x.into())
            .collect(),
    };
    let collection_builder = collection_builder_actual.clone();

    let now = Utc::now();
    collection_builder_actual.insert(&mut transaction).await?;

    let response = crate::models::collections::Collection {
        id: collection_id,
        user: collection_builder.user_id.into(),
        title: collection_builder.title.clone(),
        description: collection_builder.description.clone(),
        created: now,
        updated: now,
        icon_url: None,
        color: None,
        status: collection_builder.status,
        projects: initial_project_ids,
    };
    transaction.commit().await?;

    Ok(HttpResponse::Ok().json(response))
}

#[derive(Serialize, Deserialize)]
pub struct CollectionIds {
    pub ids: String,
}
#[get("collections")]
pub async fn collections_get(
    req: HttpRequest,
    web::Query(ids): web::Query<CollectionIds>,
    pool: web::Data<PgPool>,
    redis: web::Data<deadpool_redis::Pool>,
    session_queue: web::Data<AuthQueue>,
) -> Result<HttpResponse, ApiError> {
    let ids = serde_json::from_str::<Vec<&str>>(&ids.ids)?;
    let ids = ids
        .into_iter()
        .map(|x| parse_base62(x).map(|x| database::models::CollectionId(x as i64)))
        .collect::<Result<Vec<_>, _>>()?;

    let collections_data = database::models::Collection::get_many(&ids, &**pool, &redis).await?;

    let user_option = get_user_from_headers(
        &req,
        &**pool,
        &redis,
        &session_queue,
        Some(&[Scopes::COLLECTION_READ]),
    )
    .await
    .map(|x| x.1)
    .ok();

    let collections = filter_authorized_collections(collections_data, &user_option, &pool).await?;

    Ok(HttpResponse::Ok().json(collections))
}

#[get("{id}")]
pub async fn collection_get(
    req: HttpRequest,
    info: web::Path<(String,)>,
    pool: web::Data<PgPool>,
    redis: web::Data<deadpool_redis::Pool>,
    session_queue: web::Data<AuthQueue>,
) -> Result<HttpResponse, ApiError> {
    let string = info.into_inner().0;

    let id = database::models::CollectionId(parse_base62(&string)? as i64);
    let collection_data = database::models::Collection::get(id, &**pool, &redis).await?;
    let user_option = get_user_from_headers(
        &req,
        &**pool,
        &redis,
        &session_queue,
        Some(&[Scopes::COLLECTION_READ]),
    )
    .await
    .map(|x| x.1)
    .ok();

    if let Some(data) = collection_data {
        if is_authorized_collection(&data, &user_option).await? {
            return Ok(HttpResponse::Ok().json(Collection::from(data)));
        }
    }
    Ok(HttpResponse::NotFound().body(""))
}

#[derive(Deserialize, Validate)]
pub struct EditCollection {
    #[validate(
        length(min = 3, max = 64),
        custom(function = "crate::util::validate::validate_name")
    )]
    pub title: Option<String>,
    #[validate(length(min = 3, max = 256))]
    pub description: Option<String>,
    pub status: Option<CollectionStatus>,
    #[validate(length(max = 64))]
    pub new_projects: Option<Vec<String>>,
}

#[patch("{id}")]
pub async fn collection_edit(
    req: HttpRequest,
    info: web::Path<(String,)>,
    pool: web::Data<PgPool>,
    new_collection: web::Json<EditCollection>,
    redis: web::Data<deadpool_redis::Pool>,
    session_queue: web::Data<AuthQueue>,
) -> Result<HttpResponse, ApiError> {
    let user_option = get_user_from_headers(
        &req,
        &**pool,
        &redis,
        &session_queue,
        Some(&[Scopes::COLLECTION_WRITE]),
    )
    .await
    .map(|x| x.1)
    .ok();

    new_collection
        .validate()
        .map_err(|err| ApiError::Validation(validation_errors_to_string(err, None)))?;

    let string = info.into_inner().0;
    let id = database::models::CollectionId(parse_base62(&string)? as i64);
    let result = database::models::Collection::get(id, &**pool, &redis).await?;

    if let Some(collection_item) = result {
        if !is_authorized_collection(&collection_item, &user_option).await? {
            return Ok(HttpResponse::Unauthorized().body(""));
        }

        let id = collection_item.id;

        let mut transaction = pool.begin().await?;

        if let Some(title) = &new_collection.title {
            sqlx::query!(
                "
                UPDATE collections
                SET title = $1
                WHERE (id = $2)
                ",
                title.trim(),
                id as database::models::ids::CollectionId,
            )
            .execute(&mut *transaction)
            .await?;
        }

        if let Some(description) = &new_collection.description {
            sqlx::query!(
                "
                UPDATE collections
                SET description = $1
                WHERE (id = $2)
                ",
                description,
                id as database::models::ids::CollectionId,
            )
            .execute(&mut *transaction)
            .await?;
        }

        if let Some(status) = &new_collection.status {
            if let Some(user) = user_option {
                if !(user.role.is_mod()
                    || collection_item.status.is_approved() && status.can_be_requested())
                {
                    return Err(ApiError::CustomAuthentication(
                        "You don't have permission to set this status!".to_string(),
                    ));
                }

                sqlx::query!(
                    "
                    UPDATE collections
                    SET status = $1
                    WHERE (id = $2)
                    ",
                    status.to_string(),
                    id as database::models::ids::CollectionId,
                )
                .execute(&mut *transaction)
                .await?;
            }
        }

        if let Some(new_project_ids) = &new_collection.new_projects {
            // Delete all existing projects
            sqlx::query!(
                "
                DELETE FROM collections_mods
                WHERE collection_id = $1
                ",
                collection_item.id as database::models::ids::CollectionId,
            )
            .execute(&mut *transaction)
            .await?;

            for project_id in new_project_ids {
                let project = database::models::Project::get(project_id, &**pool, &redis)
                    .await?
                    .ok_or_else(|| {
                        ApiError::InvalidInput(format!(
                            "The specified project {project_id} does not exist!"
                        ))
                    })?;

                // Insert- don't throw an error if it already exists
                sqlx::query!(
                    "
                            INSERT INTO collections_mods (collection_id, mod_id)
                            VALUES ($1, $2)
                            ON CONFLICT DO NOTHING
                            ",
                    collection_item.id as database::models::ids::CollectionId,
                    project.inner.id as database::models::ids::ProjectId,
                )
                .execute(&mut *transaction)
                .await?;
            }
        }

        database::models::Collection::clear_cache(collection_item.id, &redis).await?;

        transaction.commit().await?;
        Ok(HttpResponse::NoContent().body(""))
    } else {
        Ok(HttpResponse::NotFound().body(""))
    }
}

#[derive(Serialize, Deserialize)]
pub struct Extension {
    pub ext: String,
}

#[patch("{id}/icon")]
#[allow(clippy::too_many_arguments)]
pub async fn collection_icon_edit(
    web::Query(ext): web::Query<Extension>,
    req: HttpRequest,
    info: web::Path<(String,)>,
    pool: web::Data<PgPool>,
    redis: web::Data<deadpool_redis::Pool>,
    file_host: web::Data<Arc<dyn FileHost + Send + Sync>>,
    mut payload: web::Payload,
    session_queue: web::Data<AuthQueue>,
) -> Result<HttpResponse, ApiError> {
    if let Some(content_type) = crate::util::ext::get_image_content_type(&ext.ext) {
        let cdn_url = dotenvy::var("CDN_URL")?;
        let user_option = get_user_from_headers(
            &req,
            &**pool,
            &redis,
            &session_queue,
            Some(&[Scopes::COLLECTION_WRITE]),
        )
        .await
        .map(|x| x.1)
        .ok();

        let string = info.into_inner().0;
        let id = database::models::CollectionId(parse_base62(&string)? as i64);
        let collection_item = database::models::Collection::get(id, &**pool, &redis)
            .await?
            .ok_or_else(|| {
                ApiError::InvalidInput("The specified collection does not exist!".to_string())
            })?;

        if !is_authorized_collection(&collection_item, &user_option).await? {
            return Ok(HttpResponse::Unauthorized().body(""));
        }

        if let Some(icon) = collection_item.icon_url {
            let name = icon.split(&format!("{cdn_url}/")).nth(1);

            if let Some(icon_path) = name {
                file_host.delete_file_version("", icon_path).await?;
            }
        }

        let bytes =
            read_from_payload(&mut payload, 262144, "Icons must be smaller than 256KiB").await?;

        let color = crate::util::img::get_color_from_img(&bytes)?;

        let hash = sha1::Sha1::from(&bytes).hexdigest();
        let collection_id: CollectionId = collection_item.id.into();
        let upload_data = file_host
            .upload_file(
                content_type,
                &format!("data/{}/{}.{}", collection_id, hash, ext.ext),
                bytes.freeze(),
            )
            .await?;

        let mut transaction = pool.begin().await?;

        sqlx::query!(
            "
            UPDATE collections
            SET icon_url = $1, color = $2
            WHERE (id = $3)
            ",
            format!("{}/{}", cdn_url, upload_data.file_name),
            color.map(|x| x as i32),
            collection_item.id as database::models::ids::CollectionId,
        )
        .execute(&mut *transaction)
        .await?;

        database::models::Collection::clear_cache(collection_item.id, &redis).await?;

        transaction.commit().await?;

        Ok(HttpResponse::NoContent().body(""))
    } else {
        Err(ApiError::InvalidInput(format!(
            "Invalid format for collection icon: {}",
            ext.ext
        )))
    }
}

#[delete("{id}/icon")]
pub async fn delete_collection_icon(
    req: HttpRequest,
    info: web::Path<(String,)>,
    pool: web::Data<PgPool>,
    redis: web::Data<deadpool_redis::Pool>,
    file_host: web::Data<Arc<dyn FileHost + Send + Sync>>,
    session_queue: web::Data<AuthQueue>,
) -> Result<HttpResponse, ApiError> {
    let user_option = get_user_from_headers(
        &req,
        &**pool,
        &redis,
        &session_queue,
        Some(&[Scopes::COLLECTION_WRITE]),
    )
    .await
    .map(|x| x.1)
    .ok();
    let string = info.into_inner().0;
    let id = database::models::CollectionId(parse_base62(&string)? as i64);
    let collection_item = database::models::Collection::get(id, &**pool, &redis)
        .await?
        .ok_or_else(|| {
            ApiError::InvalidInput("The specified collection does not exist!".to_string())
        })?;
    if !is_authorized_collection(&collection_item, &user_option).await? {
        return Ok(HttpResponse::Unauthorized().body(""));
    }

    let cdn_url = dotenvy::var("CDN_URL")?;
    if let Some(icon) = collection_item.icon_url {
        let name = icon.split(&format!("{cdn_url}/")).nth(1);

        if let Some(icon_path) = name {
            file_host.delete_file_version("", icon_path).await?;
        }
    }

    let mut transaction = pool.begin().await?;

    sqlx::query!(
        "
        UPDATE collections
        SET icon_url = NULL, color = NULL
        WHERE (id = $1)
        ",
        collection_item.id as database::models::ids::CollectionId,
    )
    .execute(&mut *transaction)
    .await?;

    database::models::Collection::clear_cache(collection_item.id, &redis).await?;

    transaction.commit().await?;

    Ok(HttpResponse::NoContent().body(""))
}

#[delete("{id}")]
pub async fn collection_delete(
    req: HttpRequest,
    info: web::Path<(String,)>,
    pool: web::Data<PgPool>,
    redis: web::Data<deadpool_redis::Pool>,
    session_queue: web::Data<AuthQueue>,
) -> Result<HttpResponse, ApiError> {
    let user_option = get_user_from_headers(
        &req,
        &**pool,
        &redis,
        &session_queue,
        Some(&[Scopes::COLLECTION_DELETE]),
    )
    .await
    .map(|x| x.1)
    .ok();

    let string = info.into_inner().0;
    let id = database::models::CollectionId(parse_base62(&string)? as i64);
    let collection = database::models::Collection::get(id, &**pool, &redis)
        .await?
        .ok_or_else(|| {
            ApiError::InvalidInput("The specified collection does not exist!".to_string())
        })?;
    if !is_authorized_collection(&collection, &user_option).await? {
        return Ok(HttpResponse::Unauthorized().body(""));
    }
    let mut transaction = pool.begin().await?;

    let result =
        database::models::Collection::remove(collection.id, &mut transaction, &redis).await?;
    database::models::Collection::clear_cache(collection.id, &redis).await?;

    transaction.commit().await?;

    if result.is_some() {
        Ok(HttpResponse::NoContent().body(""))
    } else {
        Ok(HttpResponse::NotFound().body(""))
    }
}
