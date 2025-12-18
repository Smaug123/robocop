use serde_json::Value;
use std::collections::HashMap;

/// Headers that contain security-sensitive values and must be redacted.
pub const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "openai-api-key",
    "set-cookie",
    "x-github-token",
    "x-hub-signature",
    "x-hub-signature-256",
];

pub struct Sanitizer;

impl Sanitizer {
    /// Check if a header name is sensitive and should be redacted.
    pub fn is_sensitive_header(header_name: &str) -> bool {
        let lower = header_name.to_lowercase();
        SENSITIVE_HEADERS.contains(&lower.as_str())
    }

    /// Remove sensitive data from headers
    pub fn sanitize_headers(headers: &HashMap<String, String>) -> HashMap<String, String> {
        let mut sanitized = HashMap::new();

        for (key, value) in headers {
            let sanitized_value = if Self::is_sensitive_header(key) {
                "[REDACTED]".to_string()
            } else {
                value.clone()
            };
            sanitized.insert(key.clone(), sanitized_value);
        }

        sanitized
    }

    /// Remove sensitive data from JSON payloads
    pub fn sanitize_json(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut sanitized = serde_json::Map::new();
                for (key, val) in map {
                    let sanitized_val = match key.as_str() {
                        "token" | "private_key" | "secret" | "password" => {
                            Value::String("[REDACTED]".to_string())
                        }
                        _ => Self::sanitize_json(val),
                    };
                    sanitized.insert(key.clone(), sanitized_val);
                }
                Value::Object(sanitized)
            }
            Value::Array(arr) => Value::Array(arr.iter().map(Self::sanitize_json).collect()),
            _ => value.clone(),
        }
    }
}
