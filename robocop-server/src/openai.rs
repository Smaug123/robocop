// Re-export all OpenAI types and client from robocop-core
pub use robocop_core::{
    create_openai_client, BatchCreateRequest, BatchRequest, BatchRequestBody, BatchRequestMessage,
    BatchResponse, ExpiresAfter, FileUploadResponse, JsonSchema, OpenAIClient, RequestCounts,
    ResponseFormat, ReviewMetadata, Schema, SchemaProperties, SchemaProperty,
};

#[cfg(test)]
mod tests {
    #[test]
    fn test_response_format_schema_consistency() {
        // Verify that the response format schema is correctly structured
        // This test ensures the property names in the schema match the required array
        let response_format = robocop_core::openai::OpenAIClient::create_response_format();
        let schema = response_format.json_schema.schema;

        // Serialize the schema to JSON to verify the actual property names
        let schema_json = serde_json::to_value(&schema).expect("Failed to serialize schema");
        let properties = schema_json["properties"]
            .as_object()
            .expect("Properties should be an object");

        // Verify all required properties exist in the properties object
        for required_field in &schema.required {
            assert!(
                properties.contains_key(required_field),
                "Required field '{}' not found in properties. Available properties: {:?}",
                required_field,
                properties.keys().collect::<Vec<_>>()
            );
        }

        // Verify the specific property names we expect
        assert!(
            properties.contains_key("reasoning"),
            "Schema should have 'reasoning' property"
        );
        assert!(
            properties.contains_key("substantiveComments"),
            "Schema should have 'substantiveComments' property (camelCase)"
        );
        assert!(
            properties.contains_key("summary"),
            "Schema should have 'summary' property"
        );

        // Verify we don't have the incorrect snake_case version
        assert!(
            !properties.contains_key("substantive_comments"),
            "Schema should not have 'substantive_comments' property (snake_case)"
        );
    }
}
