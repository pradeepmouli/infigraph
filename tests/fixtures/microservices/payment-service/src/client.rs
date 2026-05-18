use std::time::Duration;

pub struct OrderServiceClient {
    base_url: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl OrderServiceClient {
    pub fn new(base_url: String, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url,
            timeout,
            client,
        }
    }

    pub async fn verify_order(&self, order_id: &str) -> bool {
        let url = format!("{}/api/orders/{}", self.base_url, order_id);
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(err) => {
                log::error!("Failed to verify order {}: {}", order_id, err);
                false
            }
        }
    }

    pub async fn get_order(&self, order_id: &str) -> Option<OrderInfo> {
        let url = format!("{}/api/orders/{}", self.base_url, order_id);
        let resp = self.client.get(&url).send().await.ok()?;

        if !resp.status().is_success() {
            return None;
        }

        resp.json::<OrderInfo>().await.ok()
    }

    pub async fn notify_payment_complete(
        &self,
        order_id: &str,
        payment_id: &str,
    ) -> Result<(), ClientError> {
        let url = format!("{}/api/orders/{}/payment", self.base_url, order_id);
        let body = serde_json::json!({
            "payment_id": payment_id,
            "status": "completed",
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ClientError::Network(e.to_string()))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(ClientError::UpstreamError(resp.status().as_u16()))
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct OrderInfo {
    pub id: String,
    pub user_id: String,
    pub total_amount: f64,
    pub status: String,
}

#[derive(Debug)]
pub enum ClientError {
    Network(String),
    UpstreamError(u16),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Network(msg) => write!(f, "Network error: {}", msg),
            ClientError::UpstreamError(code) => write!(f, "Upstream returned {}", code),
        }
    }
}

impl std::error::Error for ClientError {}
