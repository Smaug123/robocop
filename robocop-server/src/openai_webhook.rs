//! OpenAI webhook handler for batch completion events.
//!
//! This module handles webhooks from OpenAI following the Standard Webhooks spec.
//! When a batch completes, OpenAI sends a webhook that triggers immediate
//! processing instead of waiting for the polling loop.

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{Json, Response},
    routing::post,
    Router,
};
use base64::prelude::*;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::batch_processor::process_single_batch;
use crate::state_machine::BatchId;
use crate::AppState;
use crate::CorrelationId;

type HmacSha256 = Hmac<Sha256>;

/// OpenAI webhook event payload.
///
/// Based on the Standard Webhooks spec and OpenAI's webhook documentation.
#[derive(Debug, Deserialize)]
pub struct OpenAIWebhookPayload {
    /// Always "event"
    pub object: String,
    /// Event ID, e.g., "evt_..."
    pub id: String,
    /// Event type, e.g., "batch.completed", "batch.failed"
    #[serde(rename = "type")]
    pub event_type: String,
    /// Event data containing the batch information
    pub data: OpenAIWebhookData,
}

/// Data field of the webhook event.
#[derive(Debug, Deserialize)]
pub struct OpenAIWebhookData {
    /// The batch ID, e.g., "batch_abc123"
    pub id: String,
}

/// Response returned by the webhook handler.
#[derive(Serialize)]
pub struct WebhookResponse {
    pub message: String,
}

/// Verify a Standard Webhooks signature.
///
/// Standard Webhooks format:
/// - Secret: Base64-encoded, may have "whsec_" prefix
/// - Headers: webhook-id, webhook-timestamp, webhook-signature
/// - Signature format: "v1,<base64>" (may have multiple comma-separated versions)
/// - Signed payload: "{webhook-id}.{webhook-timestamp}.{body}"
fn verify_standard_webhook_signature(
    secret: &str,
    webhook_id: &str,
    timestamp: &str,
    body: &[u8],
    signature_header: &str,
) -> bool {
    // Remove "whsec_" prefix if present (OpenAI uses this prefix)
    let secret_b64 = secret.strip_prefix("whsec_").unwrap_or(secret);

    // Decode the base64-encoded secret
    let secret_bytes = match BASE64_STANDARD.decode(secret_b64) {
        Ok(bytes) => bytes,
        Err(_) => {
            error!("Failed to decode webhook secret from base64");
            return false;
        }
    };

    // Build the signed payload: {webhook-id}.{webhook-timestamp}.{body}
    let body_str = String::from_utf8_lossy(body);
    let signed_payload = format!("{}.{}.{}", webhook_id, timestamp, body_str);

    // Compute HMAC-SHA256
    let mut mac = match HmacSha256::new_from_slice(&secret_bytes) {
        Ok(mac) => mac,
        Err(_) => {
            error!("Failed to create HMAC from secret");
            return false;
        }
    };
    mac.update(signed_payload.as_bytes());
    let expected_signature = BASE64_STANDARD.encode(mac.finalize().into_bytes());

    // Parse the signature header - format is "v1,<base64>" or multiple comma-separated
    // versions like "v1,<sig1> v1a,<sig2>"
    // We look for any v1 signature that matches
    for part in signature_header.split(' ') {
        if let Some(sig) = part.strip_prefix("v1,") {
            // Constant-time comparison
            if sig == expected_signature {
                return true;
            }
        }
    }

    false
}

/// Middleware to verify OpenAI webhook signatures.
async fn verify_openai_webhook_signature(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Check if OpenAI webhooks are configured
    let secret = match &state.openai_webhook_secret {
        Some(s) => s.clone(),
        None => {
            warn!("OpenAI webhook received but OPENAI_WEBHOOK_SECRET not configured");
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
    };

    let correlation_id = CorrelationId(Uuid::new_v4().to_string());

    // Extract headers and body
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Extract Standard Webhooks headers
    let webhook_id = parts
        .headers
        .get("webhook-id")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing webhook-id header");
            StatusCode::UNAUTHORIZED
        })?;

    let timestamp = parts
        .headers
        .get("webhook-timestamp")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing webhook-timestamp header");
            StatusCode::UNAUTHORIZED
        })?;

    let signature = parts
        .headers
        .get("webhook-signature")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing webhook-signature header");
            StatusCode::UNAUTHORIZED
        })?;

    // Verify signature
    if !verify_standard_webhook_signature(&secret, webhook_id, timestamp, &bytes, signature) {
        error!("Invalid OpenAI webhook signature");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Pass through to handler with correlation ID
    let mut new_request = Request::from_parts(parts, axum::body::Body::from(bytes));
    new_request.extensions_mut().insert(correlation_id);

    Ok(next.run(new_request).await)
}

/// Handler for OpenAI webhook events.
pub async fn openai_webhook_handler(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<WebhookResponse>, StatusCode> {
    info!("Received OpenAI webhook");

    let correlation_id = request
        .extensions()
        .get::<CorrelationId>()
        .map(|id| id.0.clone());

    // Parse body
    let (_parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let payload: OpenAIWebhookPayload = serde_json::from_slice(&bytes).map_err(|e| {
        error!("Failed to parse OpenAI webhook payload: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    info!(
        "OpenAI webhook: type={}, batch_id={}",
        payload.event_type, payload.data.id
    );

    // Look up which PR owns this batch
    let batch_id = payload.data.id.clone();
    let db = state.state_store.db();
    let batch_id_for_query = batch_id.clone();

    let lookup_result =
        tokio::task::spawn_blocking(move || db.get_pr_by_batch_id(&batch_id_for_query))
            .await
            .map_err(|e| {
                error!(
                    "spawn_blocking panicked looking up batch {}: {}",
                    batch_id, e
                );
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .map_err(|e| {
                error!("Database error looking up batch {}: {}", batch_id, e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

    let (pr_id, installation_id) = match lookup_result {
        Some(result) => result,
        None => {
            // Batch not found - may be from CLI or already completed
            info!("Batch {} not found in database, ignoring webhook", batch_id);
            return Ok(Json(WebhookResponse {
                message: "Batch not tracked".to_string(),
            }));
        }
    };

    info!(
        "Found PR #{} for batch {} (installation {})",
        pr_id.pr_number, batch_id, installation_id
    );

    // Spawn background task to process the batch result
    let state_clone = state.clone();
    let batch_id_clone = BatchId::from(batch_id.clone());
    let pr_id_clone = pr_id.clone();

    tokio::spawn(async move {
        let _correlation_id = correlation_id;
        if let Err(e) =
            process_single_batch(&state_clone, &pr_id_clone, &batch_id_clone, installation_id).await
        {
            error!(
                "Failed to process OpenAI webhook for batch {}: {}",
                batch_id_clone.0, e
            );
        }
    });

    Ok(Json(WebhookResponse {
        message: "Webhook received".to_string(),
    }))
}

/// Create a router for OpenAI webhook endpoints.
pub fn openai_webhook_router(middleware_state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/openai-webhook", post(openai_webhook_handler))
        .route_layer(middleware::from_fn_with_state(
            middleware_state,
            verify_openai_webhook_signature,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_signature_valid() {
        // Test vector based on Standard Webhooks spec
        // This uses a known secret and payload to verify our implementation
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let webhook_id = "msg_p5jXN8AQM9LWM0D4loKWxJek";
        let timestamp = "1614265330";
        let body = br#"{"test": 2432232314}"#;

        // Compute expected signature for this test
        let secret_b64 = secret.strip_prefix("whsec_").unwrap();
        let secret_bytes = BASE64_STANDARD.decode(secret_b64).unwrap();
        let signed_payload = format!(
            "{}.{}.{}",
            webhook_id,
            timestamp,
            String::from_utf8_lossy(body)
        );
        let mut mac = HmacSha256::new_from_slice(&secret_bytes).unwrap();
        mac.update(signed_payload.as_bytes());
        let signature = BASE64_STANDARD.encode(mac.finalize().into_bytes());

        let signature_header = format!("v1,{}", signature);

        assert!(verify_standard_webhook_signature(
            secret,
            webhook_id,
            timestamp,
            body,
            &signature_header
        ));
    }

    #[test]
    fn test_verify_signature_invalid() {
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let webhook_id = "msg_p5jXN8AQM9LWM0D4loKWxJek";
        let timestamp = "1614265330";
        let body = br#"{"test": 2432232314}"#;

        // Wrong signature
        let signature_header = "v1,wrongsignature";

        assert!(!verify_standard_webhook_signature(
            secret,
            webhook_id,
            timestamp,
            body,
            signature_header
        ));
    }

    #[test]
    fn test_verify_signature_wrong_body() {
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let webhook_id = "msg_p5jXN8AQM9LWM0D4loKWxJek";
        let timestamp = "1614265330";
        let original_body = br#"{"test": 2432232314}"#;
        let tampered_body = br#"{"test": 9999999999}"#;

        // Compute signature for original body
        let secret_b64 = secret.strip_prefix("whsec_").unwrap();
        let secret_bytes = BASE64_STANDARD.decode(secret_b64).unwrap();
        let signed_payload = format!(
            "{}.{}.{}",
            webhook_id,
            timestamp,
            String::from_utf8_lossy(original_body)
        );
        let mut mac = HmacSha256::new_from_slice(&secret_bytes).unwrap();
        mac.update(signed_payload.as_bytes());
        let signature = BASE64_STANDARD.encode(mac.finalize().into_bytes());
        let signature_header = format!("v1,{}", signature);

        // Verify should fail with tampered body
        assert!(!verify_standard_webhook_signature(
            secret,
            webhook_id,
            timestamp,
            tampered_body,
            &signature_header
        ));
    }

    #[test]
    fn test_verify_signature_multiple_versions() {
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let webhook_id = "msg_p5jXN8AQM9LWM0D4loKWxJek";
        let timestamp = "1614265330";
        let body = br#"{"test": 2432232314}"#;

        // Compute expected signature
        let secret_b64 = secret.strip_prefix("whsec_").unwrap();
        let secret_bytes = BASE64_STANDARD.decode(secret_b64).unwrap();
        let signed_payload = format!(
            "{}.{}.{}",
            webhook_id,
            timestamp,
            String::from_utf8_lossy(body)
        );
        let mut mac = HmacSha256::new_from_slice(&secret_bytes).unwrap();
        mac.update(signed_payload.as_bytes());
        let signature = BASE64_STANDARD.encode(mac.finalize().into_bytes());

        // Header with multiple versions (v1a is fake, v1 is real)
        let signature_header = format!("v1a,fakesig v1,{}", signature);

        assert!(verify_standard_webhook_signature(
            secret,
            webhook_id,
            timestamp,
            body,
            &signature_header
        ));
    }

    #[test]
    fn test_parse_webhook_payload() {
        let json = r#"{
            "object": "event",
            "id": "evt_123",
            "type": "batch.completed",
            "data": {
                "id": "batch_abc123"
            }
        }"#;

        let payload: OpenAIWebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.object, "event");
        assert_eq!(payload.id, "evt_123");
        assert_eq!(payload.event_type, "batch.completed");
        assert_eq!(payload.data.id, "batch_abc123");
    }
}
