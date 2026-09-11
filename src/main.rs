use axum::{Router, extract::State, routing::get};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    db_pool: sqlx::PgPool,
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

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL harus di-set di .env");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .unwrap();

    let app_state = Arc::new(AppState { db_pool: pool });

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/health", get(health_check))
        .with_state(app_state);

    let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
