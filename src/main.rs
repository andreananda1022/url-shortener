use axum::{
    Json, Router,
    extract::{ConnectInfo, FromRequestParts, Path, State},
    http::{HeaderMap, StatusCode, request::Parts},
    response::Redirect,
    routing::{get, patch, post},
};
use jsonwebtoken::Validation;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use uuid::Uuid;

const RATE_LIMIT_MAX_REQUESTS: i64 = 10;
const RATE_LIMIT_WINDOW_SECONDS: i64 = 60;

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

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
}

struct AuthenticatedUser {
    user_id: String,
}

impl<S> FromRequestParts<S> for AuthenticatedUser
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let header_parts = parts.headers.get("Authorization");
        match header_parts {
            Some(header_value) => {
                let header_str = header_value.to_str().unwrap_or("");
                let token = match header_str.strip_prefix("Bearer ") {
                    Some(t) => t,
                    None => return Err(StatusCode::UNAUTHORIZED),
                };
                let secret_key =
                    std::env::var("JWT_SECRET").expect("JWT_SECRET harus di-set di .env");
                let decoding_key = jsonwebtoken::DecodingKey::from_secret(secret_key.as_bytes());
                let result =
                    jsonwebtoken::decode::<Claims>(token, &decoding_key, &Validation::default());
                match result {
                    Ok(token_data) => {
                        return Ok(AuthenticatedUser {
                            user_id: token_data.claims.sub,
                        });
                    }
                    Err(_) => Err(StatusCode::UNAUTHORIZED),
                }
            }
            None => return Err(StatusCode::UNAUTHORIZED),
        }
    }
}

struct RateLimit {
    user_id: String,
}

impl FromRequestParts<Arc<AppState>> for RateLimit {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let auth_user = AuthenticatedUser::from_request_parts(parts, state).await?;
        let key = format!("rate_limit:{}", auth_user.user_id);
        let count: i64 = state.redis_client.clone().incr(&key, 1).await.unwrap();
        if count == 1 {
            let _: () = state
                .redis_client
                .clone()
                .expire(&key, RATE_LIMIT_WINDOW_SECONDS)
                .await
                .unwrap();
        }

        if count > RATE_LIMIT_MAX_REQUESTS {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }

        Ok(RateLimit {
            user_id: auth_user.user_id,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct ClickEvent {
    short_code: String,
    user_agent: String,
    ip_address: String,
    clicked_at: chrono::DateTime<chrono::Utc>,
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
    rate_limit: RateLimit,
    Json(payload): Json<ShortenRequest>,
) -> Result<Json<ShortenResponse>, StatusCode> {
    let max_retries = 5;
    let user_id = Uuid::parse_str(&rate_limit.user_id).unwrap();

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
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<(StatusCode, Redirect), StatusCode> {
    let click_event = ClickEvent {
        short_code: short_code.clone(),
        user_agent: headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string(),
        ip_address: addr.to_string(),
        clicked_at: chrono::Utc::now(),
    };

    let redis_client_clone = state.redis_client.clone();
    tokio::spawn(async move {
        if let Ok(json_str) = serde_json::to_string(&click_event) {
            let _: Result<i64, _> = redis_client_clone
                .clone()
                .rpush("click_events", json_str)
                .await;
        }
    });

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

fn generate_jwt(user_id: &str) -> String {
    let secret_key = std::env::var("JWT_SECRET").expect("JWT_SECRET harus di-set di .env");
    let expiration = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::hours(24))
        .expect("gagal menghitung waktu kadaluarsa")
        .timestamp() as usize;
    let claims = Claims {
        sub: user_id.to_owned(),
        exp: expiration,
    };
    let encoding_key = jsonwebtoken::EncodingKey::from_secret(secret_key.as_bytes());
    let header = jsonwebtoken::Header::default();
    jsonwebtoken::encode(&header, &claims, &encoding_key).unwrap()
}

async fn login_user(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, StatusCode> {
    let user = sqlx::query!(
        "SELECT id, password_hash FROM users WHERE username = $1",
        payload.username
    )
    .fetch_optional(&state.db_pool)
    .await
    .unwrap();

    match user {
        Some(row) => {
            let result = bcrypt::verify(payload.password, &row.password_hash).unwrap();
            if result {
                let token = generate_jwt(&row.id.to_string());
                return Ok(Json(LoginResponse { token }));
            }
            return Err(StatusCode::UNAUTHORIZED);
        }
        None => return Err(StatusCode::UNAUTHORIZED),
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
        .route("/login", post(login_user))
        .with_state(app_state);

    let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}
