use axum::{Router, extract::State, routing::get};
use redis::AsyncCommands;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    db_pool: sqlx::PgPool,
    redis_client: redis::aio::ConnectionManager,
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
    let _: () = client
        .set("ping", "pong")
        .await
        .unwrap();
    client.get("ping").await.unwrap()
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
        .with_state(app_state);

    let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
