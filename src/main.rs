use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::Redirect,
    routing::{get, patch, post},
};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tokio::net::TcpListener;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db_pool: sqlx::PgPool,
    redis_client: redis::aio::ConnectionManager,
}

#[derive(Deserialize)]
struct ShortenRequest {
    original_url: String,
}

#[derive(Serialize)]
struct ShortenResponse {
    short_code: String,
}

#[derive(Deserialize)]
struct UpdateUrlRequest {
    original_url: String,
}

#[derive(Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
}

#[derive(Deserialize)]
struct RegisterRequest {
    username: String,
    password: String,
}

async fn root_handler() -> &'static str {
    "Hello, URL Shortener!"
}

async fn health_check(State(state): State<Arc<AppState>>) -> &'static str {
    sqlx::query!("SELECT 1 as one")
        .fetch_one(&state.db_pool)
        .await
        .unwrap();
    "Database is connected!"
}

async fn redis_check(State(state): State<Arc<AppState>>) -> String {
    let mut client = state.redis_client.clone();
    let _: () = client.set("ping", "pong").await.unwrap();
    client.get("ping").await.unwrap()
}

async fn create_short_url(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ShortenRequest>,
) -> Result<Json<ShortenResponse>, StatusCode> {
    let max_retries = 5;
    let user_id = Uuid::parse_str("1b85a488-2579-4fd1-937c-b87e7cd95f15").unwrap();

    for _ in 0..max_retries {
        let candidate = nanoid::nanoid!(7);

        let query_result = sqlx::query!(
            "INSERT INTO urls (short_code, original_url, user_id) VALUES ($1, $2, $3)",
            candidate,
            payload.original_url,
            user_id
        )
        .execute(&state.db_pool)
        .await;

        match query_result {
            Ok(_) => {
                return Ok(Json(ShortenResponse {
                    short_code: candidate,
                }));
            }
            Err(e) => {
                if let Some(db_err) = e.as_database_error() {
                    if db_err.is_unique_violation() {
                        continue;
                    }
                }

                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    Err(StatusCode::INTERNAL_SERVER_ERROR)
}

async fn redirect_url(
    State(state): State<Arc<AppState>>,
    Path(short_code): Path<String>,
) -> Result<(StatusCode, Redirect), StatusCode> {
    let cached: Option<String> = state.redis_client.clone().get(&short_code).await.unwrap();
    match cached {
        Some(uri) => {
            return Ok((StatusCode::FOUND, Redirect::to(&uri)));
        }
        None => {
            let result = sqlx::query!(
                "SELECT original_url FROM urls WHERE short_code = $1",
                short_code
            )
            .fetch_optional(&state.db_pool)
            .await
            .unwrap();

            match result {
                Some(row) => {
                    let redis_client_clone = state.redis_client.clone();
                    let short_code_clone = short_code.clone();
                    let original_url_clone = row.original_url.clone();

                    tokio::spawn(async move {
                        let _: Result<(), _> = redis_client_clone
                            .clone()
                            .set(short_code_clone, original_url_clone)
                            .await;
                    });

                    Ok((StatusCode::FOUND, Redirect::to(&row.original_url)))
                }
                None => Err(StatusCode::NOT_FOUND),
            }
        }
    }
}

async fn update_url(
    State(state): State<Arc<AppState>>,
    Path(short_code): Path<String>,
    Json(payload): Json<UpdateUrlRequest>,
) -> Result<StatusCode, StatusCode> {
    let result = sqlx::query!(
        "UPDATE urls SET original_url = $1 WHERE short_code = $2",
        payload.original_url,
        short_code
    )
    .execute(&state.db_pool)
    .await
    .unwrap();

    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }

    let _: () = state.redis_client.clone().del(&short_code).await.unwrap();
    Ok(StatusCode::OK)
}

async fn register_user(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<RegisterRequest>,
) -> Result<StatusCode, StatusCode> {
    let hashed_password = bcrypt::hash(payload.password, bcrypt::DEFAULT_COST).unwrap();
    let result = sqlx::query!(
        "INSERT INTO users (username, password_hash) VALUES ($1, $2)",
        payload.username,
        hashed_password
    )
    .execute(&state.db_pool)
    .await;

    match result {
        Ok(_) => Ok(StatusCode::CREATED),
        Err(e) => {
            if let Some(db_err) = e.as_database_error() {
                if db_err.is_unique_violation() {
                    return Err(StatusCode::CONFLICT);
                }
            }
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL harus di-set di .env");
    let redis_url = std::env::var("REDIS_URL").expect("REDIS_URL harus di-set di .env");
    let client = redis::Client::open(redis_url).expect("Gagal menginisialisasi klien!");
    let connection_manager = redis::aio::ConnectionManager::new(client)
        .await
        .expect("Gagal membuat connection manager Redis!");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .unwrap();

    let app_state = Arc::new(AppState {
        db_pool: pool,
        redis_client: connection_manager,
    });

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/health", get(health_check))
        .route("/redis-check", get(redis_check))
        .route("/shorten", post(create_short_url))
        .route("/{short_code}", get(redirect_url))
        .route("/{short_code}", patch(update_url))
        .route("/register", post(register_user))
        .with_state(app_state);

    let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
