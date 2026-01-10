//! OpenAI webhook handler for batch completion events.
//!
//! This module handles webhooks from OpenAI following the Standard Webhooks spec.
//! When a batch completes, OpenAI sends a webhook that triggers immediate
//! processing instead of waiting for the polling loop.

use axum::{
    extract::{Extension, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{Json, Response},
    routing::post,
    Router,
};
use base64::prelude::*;

/// Webhook ID extracted from the Standard Webhooks header.
/// Passed to handler so it can mark the webhook as processed on success.
#[derive(Clone)]
struct WebhookId(String);
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::batch_processor::process_single_batch;
use crate::state_machine::repository::WebhookClaimResult;
use crate::state_machine::state::BatchId;
use crate::AppState;

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
///
/// Note: `id` is optional because non-batch events or verification events may not
/// include it. We validate its presence after filtering to batch events only.
#[derive(Debug, Deserialize)]
pub struct OpenAIWebhookData {
    /// The batch ID, e.g., "batch_abc123". Optional because non-batch events
    /// may not include this field.
    pub id: Option<String>,
}

/// Response returned by the webhook handler.
#[derive(Serialize)]
pub struct WebhookResponse {
    pub message: String,
}

/// Maximum webhook body size (1MB).
///
/// Webhook payloads are typically small (under 64KB). This limit prevents
/// memory exhaustion attacks while being generous enough for any legitimate payload.
const MAX_WEBHOOK_BODY_SIZE: usize = 1024 * 1024;

/// Maximum age of webhook timestamp (5 minutes per Standard Webhooks spec).
///
/// Webhooks with timestamps older than this are rejected to prevent replay attacks.
/// The tolerance also applies to future timestamps to handle clock skew.
const TIMESTAMP_TOLERANCE_SECONDS: i64 = 300;

/// Check if a webhook timestamp is within the acceptable tolerance window.
///
/// Returns true if the timestamp is within `TIMESTAMP_TOLERANCE_SECONDS` of `now`.
/// Both old timestamps (replay attacks) and future timestamps (clock skew) are checked.
fn is_timestamp_within_tolerance(timestamp_secs: i64, now_secs: i64) -> bool {
    (now_secs - timestamp_secs).abs() <= TIMESTAMP_TOLERANCE_SECONDS
}

/// Verify a Standard Webhooks signature using constant-time comparison.
///
/// Standard Webhooks format:
/// - Secret: Base64-encoded, may have "whsec_" prefix
/// - Headers: webhook-id, webhook-timestamp, webhook-signature
/// - Signature format: "v1,<base64>" (may have multiple space-separated versions)
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

    // Build the signed payload as raw bytes: {webhook-id}.{webhook-timestamp}.{body}
    // We must use raw bytes, not String::from_utf8_lossy, because:
    // 1. The Standard Webhooks spec signs the raw byte stream
    // 2. from_utf8_lossy replaces invalid UTF-8 with U+FFFD, corrupting the signature input
    let mut signed_payload =
        Vec::with_capacity(webhook_id.len() + 1 + timestamp.len() + 1 + body.len());
    signed_payload.extend_from_slice(webhook_id.as_bytes());
    signed_payload.push(b'.');
    signed_payload.extend_from_slice(timestamp.as_bytes());
    signed_payload.push(b'.');
    signed_payload.extend_from_slice(body);

    // Parse the signature header - format is "v1,<base64>" or multiple space-separated
    // versions like "v1,<sig1> v1a,<sig2>"
    // We look for any v1 signature that matches using constant-time comparison
    for part in signature_header.split(' ') {
        if let Some(sig_b64) = part.strip_prefix("v1,") {
            // Decode the provided signature from base64
            let sig_bytes = match BASE64_STANDARD.decode(sig_b64) {
                Ok(bytes) => bytes,
                Err(_) => continue, // Skip malformed signatures
            };

            // Create fresh HMAC for each signature attempt and use verify_slice
            // for constant-time comparison
            let mut mac = match HmacSha256::new_from_slice(&secret_bytes) {
                Ok(mac) => mac,
                Err(_) => {
                    error!("Failed to create HMAC from secret");
                    return false;
                }
            };
            mac.update(&signed_payload);

            // verify_slice performs constant-time comparison
            if mac.verify_slice(&sig_bytes).is_ok() {
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

    // Extract headers and body with size limit to prevent DoS
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_WEBHOOK_BODY_SIZE)
        .await
        .map_err(|_| {
            error!("Webhook body too large or read error");
            StatusCode::PAYLOAD_TOO_LARGE
        })?;

    // Extract Standard Webhooks headers
    let webhook_id = parts
        .headers
        .get("webhook-id")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing webhook-id header");
            StatusCode::UNAUTHORIZED
        })?
        .to_string();

    let timestamp = parts
        .headers
        .get("webhook-timestamp")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing webhook-timestamp header");
            StatusCode::UNAUTHORIZED
        })?;

    // Validate timestamp is within tolerance window (replay protection)
    let timestamp_secs: i64 = timestamp.parse().map_err(|_| {
        error!("Invalid webhook-timestamp format: {}", timestamp);
        StatusCode::UNAUTHORIZED
    })?;

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .as_secs() as i64;

    if !is_timestamp_within_tolerance(timestamp_secs, now_secs) {
        error!(
            "Webhook timestamp {} outside tolerance (current: {}, tolerance: {}s)",
            timestamp_secs, now_secs, TIMESTAMP_TOLERANCE_SECONDS
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    let signature = parts
        .headers
        .get("webhook-signature")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error!("Missing webhook-signature header");
            StatusCode::UNAUTHORIZED
        })?;

    // Verify signature
    if !verify_standard_webhook_signature(&secret, &webhook_id, timestamp, &bytes, signature) {
        error!("Invalid OpenAI webhook signature");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Atomically claim this webhook ID for processing.
    // This prevents the race condition where concurrent requests both pass
    // an is_webhook_seen check before either records, causing duplicate processing.
    //
    // Three possible outcomes:
    // - Claimed → we own processing, continue to handler
    // - InProgress → another request is processing, return 409 so OpenAI retries later
    // - Completed → already successfully processed, return 200 (no-op)
    //
    // If processing fails, the handler releases the claim so OpenAI can retry.
    match state.state_store.try_claim_webhook_id(&webhook_id).await {
        WebhookClaimResult::Claimed => {
            // We own this webhook - pass through to handler
            let mut new_request = Request::from_parts(parts, axum::body::Body::from(bytes));
            new_request.extensions_mut().insert(WebhookId(webhook_id));
            Ok(next.run(new_request).await)
        }
        WebhookClaimResult::InProgress => {
            // Another request is currently processing this webhook.
            // Return 409 Conflict so OpenAI will retry later.
            // This prevents the bug where returning 200 for in-progress requests
            // causes dropped batch completions if the first request later fails.
            info!(
                "Webhook {} in progress by another request, returning 409 for retry",
                webhook_id
            );
            Ok(Response::builder()
                .status(StatusCode::CONFLICT)
                .body(axum::body::Body::from(
                    "{\"message\":\"Processing in progress, please retry\"}",
                ))
                .unwrap())
        }
        WebhookClaimResult::Completed => {
            // Already successfully processed - return 200 to acknowledge
            info!("Webhook {} already completed, returning 200", webhook_id);
            Ok(Response::builder()
                .status(StatusCode::OK)
                .body(axum::body::Body::from(
                    "{\"message\":\"Already processed\"}",
                ))
                .unwrap())
        }
    }
}

/// Handler for OpenAI webhook events.
async fn openai_webhook_handler(
    State(state): State<Arc<AppState>>,
    Extension(webhook_id): Extension<WebhookId>,
    request: Request,
) -> Result<Json<WebhookResponse>, StatusCode> {
    info!("Received OpenAI webhook");

    // Parse body (size already validated by middleware, but apply limit for safety)
    let (_parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_WEBHOOK_BODY_SIZE).await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read OpenAI webhook body: {}", e);
            // Release claim so OpenAI can retry with the same webhook-id
            state.state_store.release_webhook_claim(&webhook_id.0).await;
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let payload: OpenAIWebhookPayload = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to parse OpenAI webhook payload: {}", e);
            // Release claim so OpenAI can retry with the same webhook-id
            state.state_store.release_webhook_claim(&webhook_id.0).await;
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    info!(
        "OpenAI webhook: type={}, batch_id={:?}",
        payload.event_type, payload.data.id
    );

    // Only process batch.* events. Non-batch events (if the subscription isn't filtered)
    // should be early-acked to avoid wasted API calls and potential error loops.
    if !payload.event_type.starts_with("batch.") {
        info!("Ignoring non-batch event type: {}", payload.event_type);
        state
            .state_store
            .complete_webhook_claim(&webhook_id.0)
            .await;
        return Ok(Json(WebhookResponse {
            message: format!("Ignored non-batch event: {}", payload.event_type),
        }));
    }

    // For batch events, data.id is required. If missing, acknowledge to prevent retries
    // but log as unexpected (all batch events should have a batch ID).
    let batch_id = match payload.data.id {
        Some(id) => id,
        None => {
            warn!(
                "Batch event {} missing data.id field, acknowledging without processing",
                payload.event_type
            );
            state
                .state_store
                .complete_webhook_claim(&webhook_id.0)
                .await;
            return Ok(Json(WebhookResponse {
                message: "Batch event missing data.id".to_string(),
            }));
        }
    };

    let lookup_result = state.state_store.get_pr_by_batch_id(&batch_id).await;

    let (pr_id, installation_id) = match lookup_result {
        Ok(Some(result)) => result,
        Ok(None) => {
            // Batch not found - may be from CLI or already completed.
            // Mark the claim as completed so future retries are no-ops.
            info!(
                "Batch {} not found in state store, ignoring webhook",
                batch_id
            );
            state
                .state_store
                .complete_webhook_claim(&webhook_id.0)
                .await;
            return Ok(Json(WebhookResponse {
                message: "Batch not tracked".to_string(),
            }));
        }
        Err(e) => {
            // Transient repository error - release the claim and return 500 so OpenAI retries.
            // This is critical: if we kept the claim and returned 200, the batch completion
            // would be lost and OpenAI wouldn't retry.
            error!(
                "Repository error looking up batch {}: {} - returning 500 to trigger retry",
                batch_id, e
            );
            state.state_store.release_webhook_claim(&webhook_id.0).await;
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    info!(
        "Found PR #{} for batch {} (installation {})",
        pr_id.pr_number, batch_id, installation_id
    );

    // Process the batch result synchronously.
    // We must NOT return 200 until processing succeeds, otherwise OpenAI won't retry
    // and transient failures (DB errors, API hiccups) will silently drop batch results.
    // The polling loop provides a fallback, but if polling frequency is reduced (since
    // webhooks are the primary notification), batches could be stuck for a long time.
    let batch_id_typed = BatchId::from(batch_id.clone());

    if let Err(e) = process_single_batch(&state, &pr_id, &batch_id_typed, installation_id).await {
        error!(
            "Failed to process OpenAI webhook for batch {}: {}",
            batch_id, e
        );
        // Release the claim so OpenAI can retry with the same webhook-id.
        // Per Standard Webhooks spec, retries reuse the same webhook-id.
        state.state_store.release_webhook_claim(&webhook_id.0).await;
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Success! Mark the claim as completed so future retries see Completed instead
    // of InProgress, and return 200 immediately (no reprocessing needed).
    state
        .state_store
        .complete_webhook_claim(&webhook_id.0)
        .await;
    Ok(Json(WebhookResponse {
        message: "Webhook processed".to_string(),
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

    // Official test vector from Standard Webhooks / Svix documentation:
    // https://docs.svix.com/receiving/verifying-payloads/how-manual
    //
    // These values are hard-coded from the spec to ensure our implementation
    // matches the standard, rather than computing the signature ourselves.
    const TEST_SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
    const TEST_WEBHOOK_ID: &str = "msg_p5jXN8AQM9LWM0D4loKWxJek";
    const TEST_TIMESTAMP: &str = "1614265330";
    const TEST_BODY: &[u8] = br#"{"test": 2432232314}"#;
    // Expected signature from the Standard Webhooks spec
    const TEST_EXPECTED_SIGNATURE: &str = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";

    #[test]
    fn test_verify_signature_with_spec_test_vector() {
        // Verify against the official Standard Webhooks test vector
        // This ensures our implementation matches the spec exactly
        assert!(
            verify_standard_webhook_signature(
                TEST_SECRET,
                TEST_WEBHOOK_ID,
                TEST_TIMESTAMP,
                TEST_BODY,
                TEST_EXPECTED_SIGNATURE
            ),
            "Failed to verify official Standard Webhooks test vector"
        );
    }

    #[test]
    fn test_verify_signature_invalid() {
        // Wrong signature should fail
        let wrong_signature = "v1,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

        assert!(!verify_standard_webhook_signature(
            TEST_SECRET,
            TEST_WEBHOOK_ID,
            TEST_TIMESTAMP,
            TEST_BODY,
            wrong_signature
        ));
    }

    #[test]
    fn test_verify_signature_malformed_base64() {
        // Malformed base64 in signature should fail gracefully
        let malformed_signature = "v1,not-valid-base64!!!";

        assert!(!verify_standard_webhook_signature(
            TEST_SECRET,
            TEST_WEBHOOK_ID,
            TEST_TIMESTAMP,
            TEST_BODY,
            malformed_signature
        ));
    }

    #[test]
    fn test_verify_signature_wrong_body() {
        // Valid signature for original body should fail with tampered body
        let tampered_body = br#"{"test": 9999999999}"#;

        assert!(!verify_standard_webhook_signature(
            TEST_SECRET,
            TEST_WEBHOOK_ID,
            TEST_TIMESTAMP,
            tampered_body,
            TEST_EXPECTED_SIGNATURE
        ));
    }

    #[test]
    fn test_verify_signature_wrong_timestamp() {
        // Valid signature should fail with different timestamp
        let wrong_timestamp = "1614265331"; // Off by one second

        assert!(!verify_standard_webhook_signature(
            TEST_SECRET,
            TEST_WEBHOOK_ID,
            wrong_timestamp,
            TEST_BODY,
            TEST_EXPECTED_SIGNATURE
        ));
    }

    #[test]
    fn test_verify_signature_wrong_webhook_id() {
        // Valid signature should fail with different webhook ID
        let wrong_webhook_id = "msg_different";

        assert!(!verify_standard_webhook_signature(
            TEST_SECRET,
            wrong_webhook_id,
            TEST_TIMESTAMP,
            TEST_BODY,
            TEST_EXPECTED_SIGNATURE
        ));
    }

    #[test]
    fn test_verify_signature_multiple_versions() {
        // Header with multiple signature versions - v1 should still match
        let signature_header_with_multiple = format!("v1a,fakesig {}", TEST_EXPECTED_SIGNATURE);

        assert!(verify_standard_webhook_signature(
            TEST_SECRET,
            TEST_WEBHOOK_ID,
            TEST_TIMESTAMP,
            TEST_BODY,
            &signature_header_with_multiple
        ));
    }

    #[test]
    fn test_verify_signature_secret_without_prefix() {
        // Secret without whsec_ prefix should also work
        let secret_without_prefix = "MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

        assert!(verify_standard_webhook_signature(
            secret_without_prefix,
            TEST_WEBHOOK_ID,
            TEST_TIMESTAMP,
            TEST_BODY,
            TEST_EXPECTED_SIGNATURE
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
        assert_eq!(payload.data.id, Some("batch_abc123".to_string()));
    }

    #[test]
    fn test_parse_webhook_payload_without_data_id() {
        // Non-batch events or verification events may not include data.id
        let json = r#"{
            "object": "event",
            "id": "evt_456",
            "type": "fine_tuning.job.succeeded",
            "data": {}
        }"#;

        let payload: OpenAIWebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.object, "event");
        assert_eq!(payload.id, "evt_456");
        assert_eq!(payload.event_type, "fine_tuning.job.succeeded");
        assert_eq!(payload.data.id, None);
    }

    #[test]
    fn test_timestamp_within_tolerance_current() {
        let now = 1700000000i64;
        // Timestamp at exactly now should be valid
        assert!(is_timestamp_within_tolerance(now, now));
    }

    #[test]
    fn test_timestamp_within_tolerance_slightly_old() {
        let now = 1700000000i64;
        // Timestamp 60 seconds ago should be valid
        assert!(is_timestamp_within_tolerance(now - 60, now));
    }

    #[test]
    fn test_timestamp_within_tolerance_at_boundary() {
        let now = 1700000000i64;
        // Timestamp exactly at tolerance boundary should be valid
        assert!(is_timestamp_within_tolerance(
            now - TIMESTAMP_TOLERANCE_SECONDS,
            now
        ));
        assert!(is_timestamp_within_tolerance(
            now + TIMESTAMP_TOLERANCE_SECONDS,
            now
        ));
    }

    #[test]
    fn test_timestamp_outside_tolerance_too_old() {
        let now = 1700000000i64;
        // Timestamp just past tolerance should be rejected
        assert!(!is_timestamp_within_tolerance(
            now - TIMESTAMP_TOLERANCE_SECONDS - 1,
            now
        ));
        // Timestamp way in the past should be rejected
        assert!(!is_timestamp_within_tolerance(now - 3600, now));
    }

    #[test]
    fn test_timestamp_outside_tolerance_future() {
        let now = 1700000000i64;
        // Timestamp just past tolerance in future should be rejected
        assert!(!is_timestamp_within_tolerance(
            now + TIMESTAMP_TOLERANCE_SECONDS + 1,
            now
        ));
        // Timestamp way in the future should be rejected
        assert!(!is_timestamp_within_tolerance(now + 3600, now));
    }

    // =========================================================================
    // Webhook replay protection tests
    // =========================================================================

    /// Test that duplicate webhook IDs are rejected (replay protection).
    ///
    /// This test verifies that a captured webhook cannot be replayed within
    /// the 5-minute timestamp window.
    ///
    /// Regression test for: webhook-id is parsed but never stored/deduped
    #[tokio::test]
    async fn test_duplicate_webhook_id_rejected() {
        use crate::state_machine::repository::SqliteRepository;
        use crate::state_machine::store::StateStore;
        use std::sync::Arc;

        // Create a repository with webhook dedup support
        let repo = Arc::new(SqliteRepository::new_in_memory().unwrap());
        let state_store = StateStore::with_repository(repo.clone());

        let webhook_id = "msg_test123";

        // First request should succeed (not seen before)
        let first_seen = state_store.is_webhook_seen(webhook_id).await;
        assert!(
            !first_seen,
            "First request for webhook ID should not be seen"
        );

        // Record the webhook ID
        state_store.record_webhook_id(webhook_id).await.unwrap();

        // Second request with same webhook ID should be detected as duplicate
        let second_seen = state_store.is_webhook_seen(webhook_id).await;
        assert!(
            second_seen,
            "Second request with same webhook ID should be detected as duplicate"
        );
    }

    /// Test that webhook IDs expire after cleanup with TTL.
    ///
    /// Property: After `cleanup_expired_webhooks` removes old entries,
    /// the webhook ID should no longer be seen.
    #[tokio::test]
    async fn test_webhook_id_expires_after_cleanup() {
        use crate::state_machine::repository::SqliteRepository;
        use crate::state_machine::store::StateStore;
        use rusqlite::params;
        use std::sync::Arc;

        let repo = Arc::new(SqliteRepository::new_in_memory().unwrap());
        let state_store = StateStore::with_repository(repo.clone());

        let webhook_id = "msg_expire_test";

        // Record the webhook ID (marks as completed)
        state_store.record_webhook_id(webhook_id).await.unwrap();
        assert!(
            state_store.is_webhook_seen(webhook_id).await,
            "Webhook ID should be seen immediately after recording"
        );

        // Make it old by manipulating the DB timestamp
        {
            let conn = repo.conn.lock().unwrap();
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // Set timestamp to well past the tolerance window
            let old_timestamp = now_secs - (TIMESTAMP_TOLERANCE_SECONDS + 60);
            conn.execute(
                "UPDATE seen_webhook_ids SET recorded_at = ?1 WHERE webhook_id = ?2",
                params![old_timestamp, webhook_id],
            )
            .unwrap();
        }

        // Cleanup with TTL matching timestamp tolerance
        let cleaned = state_store
            .cleanup_expired_webhooks(TIMESTAMP_TOLERANCE_SECONDS)
            .await;
        assert!(cleaned > 0, "Should have cleaned up expired webhook");

        // Should no longer be seen
        assert!(
            !state_store.is_webhook_seen(webhook_id).await,
            "Webhook should be gone after cleanup"
        );
    }

    /// Test that concurrent webhook claims are properly serialized.
    ///
    /// Property: When multiple concurrent callers try to claim the same webhook ID,
    /// exactly one succeeds (returns true) and all others fail (return false).
    ///
    /// This is a regression test for the race condition where the non-atomic
    /// is_webhook_seen + record_webhook_id pattern allowed concurrent requests
    /// to both pass the check before either recorded.
    #[tokio::test]
    async fn test_concurrent_webhook_claims_only_one_succeeds() {
        use crate::state_machine::repository::SqliteRepository;
        use crate::state_machine::store::StateStore;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let repo = Arc::new(SqliteRepository::new_in_memory().unwrap());
        let state_store = Arc::new(StateStore::with_repository(repo));

        let webhook_id = "msg_concurrent_test";
        let successful_claims = Arc::new(AtomicUsize::new(0));

        // Spawn 20 concurrent tasks all trying to claim the same webhook ID
        let mut handles = vec![];
        for _ in 0..20 {
            let store = state_store.clone();
            let claims = successful_claims.clone();
            let id = webhook_id.to_string();
            handles.push(tokio::spawn(async move {
                if store.try_claim_webhook_id(&id).await == WebhookClaimResult::Claimed {
                    claims.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }

        // Wait for all tasks to complete
        for handle in handles {
            handle.await.unwrap();
        }

        // Property: Exactly one caller should have succeeded
        let claims = successful_claims.load(Ordering::SeqCst);
        assert_eq!(
            claims, 1,
            "Exactly one concurrent caller should succeed in claiming the webhook ID.\n\
             Got {} successful claims, indicating a race condition.",
            claims
        );
    }

    // =========================================================================
    // Property-based tests for webhook deduplication semantics
    // =========================================================================

    use proptest::prelude::*;

    prop_compose! {
        /// Generate a random webhook ID.
        fn arb_webhook_id()(id in "[a-z]{3,10}") -> String {
            format!("msg_{}", id)
        }
    }

    proptest! {
        /// Property: is_webhook_seen returns true iff the webhook was successfully processed.
        ///
        /// This is the core semantic invariant for webhook deduplication:
        /// - After successful processing → is_webhook_seen returns true
        /// - After failed processing → is_webhook_seen returns false (retry allowed)
        ///
        /// The original bug violated this: webhook IDs were marked "seen" before
        /// processing, so failures left them marked, blocking retries.
        ///
        /// This test models the actual handler flow:
        /// 1. try_claim_webhook_id() → Claimed/InProgress/Completed
        /// 2. If Claimed + success → complete_webhook_claim()
        /// 3. If Claimed + failure → release_webhook_claim()
        #[test]
        fn webhook_seen_iff_successfully_processed(
            webhook_id in arb_webhook_id(),
            outcomes in prop::collection::vec(prop::bool::ANY, 1..10)
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                use crate::state_machine::repository::{SqliteRepository, WebhookClaimResult};
                use crate::state_machine::store::StateStore;
                use std::sync::Arc;

                let repo = Arc::new(SqliteRepository::new_in_memory().unwrap());
                let state_store = StateStore::with_repository(repo);

                let mut any_success = false;

                // Simulate a sequence of processing attempts using the claim-based flow
                for success in &outcomes {
                    // This is what the handler middleware does
                    let claim_result = state_store.try_claim_webhook_id(&webhook_id).await;

                    match claim_result {
                        WebhookClaimResult::Claimed => {
                            // We claimed it, now process
                            if *success {
                                // Success: mark as completed (this is what the handler does)
                                state_store.complete_webhook_claim(&webhook_id).await;
                                any_success = true;
                            } else {
                                // Failure: release the claim to allow retry
                                state_store.release_webhook_claim(&webhook_id).await;
                            }
                        }
                        WebhookClaimResult::InProgress => {
                            // Another request is processing - handler returns 409
                            // No state change
                        }
                        WebhookClaimResult::Completed => {
                            // Already completed - handler returns 200
                            // No state change
                        }
                    }
                }

                // Verify the invariant
                let is_seen = state_store.is_webhook_seen(&webhook_id).await;
                assert_eq!(
                    is_seen, any_success,
                    "Webhook {} should be seen={} but was seen={}.\n\
                     Outcomes: {:?}",
                    webhook_id, any_success, is_seen, outcomes
                );
            });
        }

        /// Property: Failed processing attempts don't block retries.
        ///
        /// If a webhook fails N times and then succeeds, the final state should
        /// be "seen". This tests the retry path that the original bug broke.
        ///
        /// This test models the actual handler flow:
        /// - Failed attempt: try_claim → Claimed → release_webhook_claim
        /// - Successful attempt: try_claim → Claimed → complete_webhook_claim
        #[test]
        fn failed_attempts_allow_retry(
            webhook_id in arb_webhook_id(),
            num_failures in 0usize..10,
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                use crate::state_machine::repository::{SqliteRepository, WebhookClaimResult};
                use crate::state_machine::store::StateStore;
                use std::sync::Arc;

                let repo = Arc::new(SqliteRepository::new_in_memory().unwrap());
                let state_store = StateStore::with_repository(repo);

                // Simulate N failed attempts using the claim-based flow
                for i in 0..num_failures {
                    // Claim the webhook (simulating handler middleware)
                    let claim_result = state_store.try_claim_webhook_id(&webhook_id).await;
                    assert_eq!(
                        claim_result,
                        WebhookClaimResult::Claimed,
                        "Attempt {} should successfully claim",
                        i
                    );

                    // Each failure should NOT mark the webhook as seen
                    let seen = state_store.is_webhook_seen(&webhook_id).await;
                    assert!(
                        !seen,
                        "Webhook should not be seen while in-progress (attempt {})",
                        i
                    );

                    // Simulate failure: release the claim to allow retry
                    state_store.release_webhook_claim(&webhook_id).await;

                    // After release, still not seen (allows retry)
                    let seen_after_release = state_store.is_webhook_seen(&webhook_id).await;
                    assert!(
                        !seen_after_release,
                        "Webhook should not be seen after release (attempt {})",
                        i
                    );
                }

                // Now simulate a successful attempt
                let seen_before_success = state_store.is_webhook_seen(&webhook_id).await;
                assert!(
                    !seen_before_success,
                    "Webhook should not be seen before successful processing"
                );

                // Claim for the successful attempt
                let claim_result = state_store.try_claim_webhook_id(&webhook_id).await;
                assert_eq!(
                    claim_result,
                    WebhookClaimResult::Claimed,
                    "Should be able to claim for successful attempt after {} failures",
                    num_failures
                );

                // Success: complete the claim
                state_store.complete_webhook_claim(&webhook_id).await;

                // Now it should be seen
                let seen_after_success = state_store.is_webhook_seen(&webhook_id).await;
                assert!(
                    seen_after_success,
                    "Webhook should be seen after successful processing"
                );

                // Verify we can't claim again (already completed)
                let final_claim = state_store.try_claim_webhook_id(&webhook_id).await;
                assert_eq!(
                    final_claim,
                    WebhookClaimResult::Completed,
                    "Should return Completed for already-processed webhook"
                );
            });
        }
    }
}
