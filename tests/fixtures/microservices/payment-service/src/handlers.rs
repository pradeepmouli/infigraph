use actix_web::{get, post, web, HttpResponse, Responder};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::client::OrderServiceClient;
use crate::models::{
    CreatePaymentRequest, ListPaymentsQuery, Payment, PaymentStatus,
};

pub struct AppState {
    pub payments: Mutex<HashMap<String, Payment>>,
    pub next_id: Mutex<u64>,
    pub order_client: OrderServiceClient,
}

fn generate_payment_id(next_id: &Mutex<u64>) -> String {
    let mut id = next_id.lock().unwrap();
    let payment_id = format!("PAY-{:08}", *id);
    *id += 1;
    payment_id
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[get("/api/payments")]
pub async fn list_payments(
    state: web::Data<AppState>,
    query: web::Query<ListPaymentsQuery>,
) -> impl Responder {
    let payments = state.payments.lock().unwrap();
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(20);

    let mut filtered: Vec<&Payment> = payments.values().collect();

    if let Some(ref order_id) = query.order_id {
        filtered.retain(|p| &p.order_id == order_id);
    }
    if let Some(ref status_str) = query.status {
        if let Some(status) = PaymentStatus::from_str(status_str) {
            let target = status.as_str().to_string();
            filtered.retain(|p| p.status.as_str() == target);
        }
    }

    let total = filtered.len();
    let start = (page - 1) * per_page;
    let paged: Vec<_> = filtered
        .into_iter()
        .skip(start)
        .take(per_page)
        .map(|p| p.to_response())
        .collect();

    HttpResponse::Ok().json(serde_json::json!({
        "payments": paged,
        "page": page,
        "per_page": per_page,
        "total": total,
    }))
}

#[post("/api/payments")]
pub async fn create_payment(
    state: web::Data<AppState>,
    body: web::Json<CreatePaymentRequest>,
) -> impl Responder {
    let order_exists = state
        .order_client
        .verify_order(&body.order_id)
        .await;

    if !order_exists {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": "Order not found"
        }));
    }

    let payment_id = generate_payment_id(&state.next_id);
    let currency = body.currency.clone().unwrap_or_else(|| "USD".to_string());

    let (status, provider_tx_id) = process_with_provider(body.amount, &currency);

    let payment = Payment {
        id: payment_id.clone(),
        order_id: body.order_id.clone(),
        amount: body.amount,
        currency,
        status,
        provider_tx_id,
        created_at: now_iso(),
        updated_at: now_iso(),
    };

    let response = payment.to_response();
    state.payments.lock().unwrap().insert(payment_id, payment);

    HttpResponse::Created().json(response)
}

#[get("/api/payments/{id}")]
pub async fn get_payment(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> impl Responder {
    let payment_id = path.into_inner();
    let payments = state.payments.lock().unwrap();

    match payments.get(&payment_id) {
        Some(payment) => HttpResponse::Ok().json(payment.to_response()),
        None => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Payment not found"
        })),
    }
}

#[post("/api/payments/{id}/refund")]
pub async fn refund_payment(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> impl Responder {
    let payment_id = path.into_inner();
    let mut payments = state.payments.lock().unwrap();

    match payments.get_mut(&payment_id) {
        Some(payment) => {
            match payment.status {
                PaymentStatus::Completed => {
                    payment.status = PaymentStatus::Refunded;
                    payment.updated_at = now_iso();
                    HttpResponse::Ok().json(payment.to_response())
                }
                PaymentStatus::Refunded => {
                    HttpResponse::Conflict().json(serde_json::json!({
                        "error": "Payment already refunded"
                    }))
                }
                _ => {
                    HttpResponse::BadRequest().json(serde_json::json!({
                        "error": "Only completed payments can be refunded"
                    }))
                }
            }
        }
        None => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Payment not found"
        })),
    }
}

pub async fn health_check() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({ "status": "healthy" }))
}

fn process_with_provider(amount: f64, _currency: &str) -> (PaymentStatus, Option<String>) {
    if amount <= 0.0 {
        return (PaymentStatus::Failed, None);
    }
    let tx_id = format!("PROV-{}", uuid_simple());
    (PaymentStatus::Completed, Some(tx_id))
}

fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", nanos)
}
