//! Provider-enforced structured output with independent local schema validation.
use anyhow::{ensure, Context, Result};
use serde_json::Value;

fn check_local_references(value: &Value) -> Result<()> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                    ensure!(
                        value
                            .as_str()
                            .is_some_and(|reference| reference.starts_with('#')),
                        "Structured-output schemas support only local fragment references"
                    );
                }
                check_local_references(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                check_local_references(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn validate_schema(schema: &Value) -> Result<()> {
    ensure!(schema.is_object(), "JSON schema must be an object");
    ensure!(
        serde_json::to_vec(schema)?.len() <= 1_048_576,
        "JSON schema exceeds 1 MiB"
    );
    check_local_references(schema)?;
    jsonschema::validator_for(schema)
        .map_err(|error| anyhow::anyhow!("Invalid JSON schema: {error}"))?;
    Ok(())
}

pub fn validate_result(schema: &Value, text: &str) -> Result<Value> {
    validate_schema(schema)?;
    let value: Value = serde_json::from_str(text).context("Structured output is not valid JSON")?;
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| anyhow::anyhow!("Invalid JSON schema: {error}"))?;
    let errors: Vec<_> = validator
        .iter_errors(&value)
        .take(3)
        .map(|error| error.to_string())
        .collect();
    ensure!(
        errors.is_empty(),
        "Structured output does not match JSON schema: {}",
        errors.join("; ")
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn validates_required_types_enum_and_additional_properties() {
        let schema = json!({"type":"object","properties":{"answer":{"type":"integer","minimum":1},"status":{"enum":["done"]}},"required":["answer","status"],"additionalProperties":false});
        assert!(validate_result(&schema, r#"{"answer":2,"status":"done"}"#).is_ok());
        for text in [
            r#"{"answer":"2","status":"done"}"#,
            r#"{"answer":0,"status":"done"}"#,
            r#"{"answer":2}"#,
            r#"{"answer":2,"status":"done","extra":1}"#,
            "not json",
        ] {
            assert!(validate_result(&schema, text).is_err());
        }
    }
    #[test]
    fn local_refs_work_and_remote_refs_or_invalid_schema_fail() {
        let schema = json!({"$defs":{"value":{"type":"integer"}},"type":"object","properties":{"value":{"$ref":"#/$defs/value"}},"required":["value"]});
        assert!(validate_result(&schema, r#"{"value":4}"#).is_ok());
        assert!(validate_result(&schema, r#"{"value":"four"}"#).is_err());
        assert!(validate_schema(&json!({"$ref":"https://example.invalid/schema"})).is_err());
        assert!(validate_schema(&json!({"type":42})).is_err());
    }
}
