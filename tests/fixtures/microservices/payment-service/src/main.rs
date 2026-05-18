use actix_web::{web, App, HttpServer, middleware};

mod client;
mod handlers;
mod models;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let order_client = client::OrderServiceClient::new(
        "http://order-service:3000".to_string(),
        std::time::Duration::from_secs(5),
    );

    let app_state = web::Data::new(handlers::AppState {
        payments: std::sync::Mutex::new(std::collections::HashMap::new()),
        next_id: std::sync::Mutex::new(1),
        order_client,
    });

    log::info!("Starting payment service on 0.0.0.0:8080");

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .wrap(middleware::Logger::default())
            .service(handlers::list_payments)
            .service(handlers::create_payment)
            .service(handlers::get_payment)
            .service(handlers::refund_payment)
            .route("/health", web::get().to(handlers::health_check))
    })
    .bind("0.0.0.0:8080")?
    .run()
    .await
}
