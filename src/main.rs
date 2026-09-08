use axum::{Router, routing::get};
use tokio::net::TcpListener;

async fn root_handler() -> &'static str {
    "Hello, URL Shortener!"
}

#[tokio::main]
async fn main() {
    let app = Router::new().route("/", get(root_handler));
    let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
