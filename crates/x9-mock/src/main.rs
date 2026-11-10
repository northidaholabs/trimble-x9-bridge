//! x9-mock: serves the mock X9 API (see the library's docs for what it does
//! and the environment it reads).

#[tokio::main]
async fn main() {
    let addr = std::env::var("X9_MOCK_ADDR").unwrap_or_else(|_| "127.0.0.1:34567".to_string());
    let app = x9_mock::app(x9_mock::Config::from_env());
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    eprintln!("x9-mock scanner API listening on http://{addr}/api/v1");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .unwrap();
}
