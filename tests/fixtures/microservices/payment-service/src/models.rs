use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaymentStatus {
    Pending,
    Completed,
    Failed,
    Refunded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payment {
    pub id: String,
    pub order_id: String,
    pub amount: f64,
    pub currency: String,
    pub status: PaymentStatus,
    pub provider_tx_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreatePaymentRequest {
    pub order_id: String,
    pub amount: f64,
    pub currency: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PaymentResponse {
    pub id: String,
    pub order_id: String,
    pub amount: f64,
    pub currency: String,
    pub status: PaymentStatus,
}

#[derive(Debug, Deserialize)]
pub struct ListPaymentsQuery {
    pub order_id: Option<String>,
    pub status: Option<String>,
    pub page: Option<usize>,
    pub per_page: Option<usize>,
}

impl Payment {
    pub fn to_response(&self) -> PaymentResponse {
        PaymentResponse {
            id: self.id.clone(),
            order_id: self.order_id.clone(),
            amount: self.amount,
            currency: self.currency.clone(),
            status: self.status.clone(),
        }
    }
}

impl PaymentStatus {
    pub fn as_str(&self) -> &str {
        match self {
            PaymentStatus::Pending => "pending",
            PaymentStatus::Completed => "completed",
            PaymentStatus::Failed => "failed",
            PaymentStatus::Refunded => "refunded",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(PaymentStatus::Pending),
            "completed" => Some(PaymentStatus::Completed),
            "failed" => Some(PaymentStatus::Failed),
            "refunded" => Some(PaymentStatus::Refunded),
            _ => None,
        }
    }
}
