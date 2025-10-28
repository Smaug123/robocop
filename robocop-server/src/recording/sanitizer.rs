use serde_json::Value;
use std::collections::HashMap;

pub struct Sanitizer;

impl Sanitizer {
    /// Remove sensitive data from headers
    pub fn sanitize_headers(headers: &HashMap<String, String>) -> HashMap<String, String> {
        let mut sanitized = HashMap::new();

        for (key, value) in headers {
            let key_lower = key.to_lowercase();
            let sanitized_value = match key_lower.as_str() {
                "authorization" => "[REDACTED]".to_string(),
                "x-hub-signature-256" => "[REDACTED]".to_string(),
                "cookie" => "[REDACTED]".to_string(),
                _ => value.clone(),
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
