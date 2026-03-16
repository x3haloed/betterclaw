use serde_json::{Value, json};

/// Normalize a JSON schema for strict provider validation.
///
/// Rules:
/// - Object schemas always declare `additionalProperties: false`
/// - Every property appears in `required`
/// - Originally-optional properties become nullable
/// - Nested objects, array items, combinators, and defs are normalized recursively
pub fn normalize_schema_strict(schema: &Value) -> Value {
    let mut normalized = schema.clone();
    normalize_schema_recursive(&mut normalized);
    normalized
}

fn normalize_schema_recursive(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(entries)) = object.get_mut(keyword) {
            for entry in entries {
                normalize_schema_recursive(entry);
            }
        }
    }

    if let Some(items) = object.get_mut("items") {
        normalize_schema_recursive(items);
    }

    for keyword in ["not", "if", "then", "else"] {
        if let Some(entry) = object.get_mut(keyword) {
            normalize_schema_recursive(entry);
        }
    }

    for keyword in ["$defs", "definitions"] {
        if let Some(Value::Object(entries)) = object.get_mut(keyword) {
            for entry in entries.values_mut() {
                normalize_schema_recursive(entry);
            }
        }
    }

    let is_object = object
        .get("type")
        .map(schema_type_includes_object)
        .unwrap_or(false);
    let has_properties = object.contains_key("properties");
    if !is_object && !has_properties {
        return;
    }

    if !object.contains_key("type") && has_properties {
        object.insert("type".to_string(), Value::String("object".to_string()));
    }

    object.insert("additionalProperties".to_string(), Value::Bool(false));
    if !object.contains_key("properties") {
        object.insert(
            "properties".to_string(),
            Value::Object(serde_json::Map::new()),
        );
    }

    let current_required = object
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();

    let all_keys = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            let mut keys = properties.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys
        })
        .unwrap_or_default();

    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for key in &all_keys {
            if let Some(property_schema) = properties.get_mut(key) {
                normalize_schema_recursive(property_schema);
            }
            if !current_required.contains(key)
                && let Some(property_schema) = properties.get_mut(key)
            {
                make_schema_nullable(property_schema);
            }
        }
    }

    object.insert(
        "required".to_string(),
        Value::Array(all_keys.into_iter().map(Value::String).collect()),
    );
}

fn schema_type_includes_object(value: &Value) -> bool {
    match value {
        Value::String(kind) => kind == "object",
        Value::Array(items) => items.iter().any(|item| item.as_str() == Some("object")),
        _ => false,
    }
}

fn make_schema_nullable(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    if let Some(type_value) = object.get_mut("type") {
        match type_value {
            Value::String(kind) if kind != "null" => {
                *type_value = Value::Array(vec![
                    Value::String(kind.clone()),
                    Value::String("null".to_string()),
                ]);
                return;
            }
            Value::Array(items) if !items.iter().any(|item| item.as_str() == Some("null")) => {
                items.push(Value::String("null".to_string()));
                return;
            }
            _ => return,
        }
    }

    if let Some(Value::Array(any_of)) = object.get_mut("anyOf")
        && !any_of
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("null"))
    {
        any_of.push(json!({ "type": "null" }));
    }
}

pub fn validate_strict_schema(schema: &Value, name: &str) -> Result<(), Vec<String>> {
    let errors = validate_object_schema(schema, name);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_object_schema(schema: &Value, path: &str) -> Vec<String> {
    let mut errors = Vec::new();

    match schema.get("type") {
        Some(Value::String(kind)) if kind == "object" => {}
        Some(Value::Array(items)) if items.iter().any(|item| item.as_str() == Some("object")) => {}
        Some(other) => {
            errors.push(format!("{path}: expected object type, got {other}"));
            return errors;
        }
        None => {
            errors.push(format!("{path}: missing object type"));
            return errors;
        }
    }

    let properties = match schema.get("properties").and_then(Value::as_object) {
        Some(properties) => properties,
        None => {
            errors.push(format!("{path}: missing or non-object properties"));
            return errors;
        }
    };

    let required = schema.get("required").and_then(Value::as_array);
    if required.is_none() {
        errors.push(format!("{path}: missing required array"));
    }
    let required_names = required
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    for key in properties.keys() {
        if !required_names.contains(key.as_str()) {
            errors.push(format!("{path}: required missing property key '{key}'"));
        }
    }

    if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
        errors.push(format!("{path}: additionalProperties must be false"));
    }

    for (key, property) in properties {
        let property_path = format!("{path}.{key}");
        if property.get("type").is_none()
            && property.get("anyOf").is_none()
            && property.get("$ref").is_none()
        {
            errors.push(format!("{property_path}: property missing type/anyOf/$ref"));
            continue;
        }

        if let Some(items) = property.get("items")
            && items.get("type").is_some()
            && items
                .get("type")
                .map(schema_type_includes_object)
                .unwrap_or(false)
        {
            errors.extend(validate_object_schema(
                items,
                &format!("{property_path}.items"),
            ));
        }

        let property_is_object = property
            .get("type")
            .map(schema_type_includes_object)
            .unwrap_or_else(|| property.get("properties").is_some());
        if property_is_object && property.get("$ref").is_none() {
            errors.extend(validate_object_schema(property, &property_path));
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{normalize_schema_strict, validate_strict_schema};

    fn required_names(value: &serde_json::Value) -> Vec<String> {
        let mut names = value
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn normalizes_optional_fields_to_required_nullable() {
        let normalized = normalize_schema_strict(&json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" },
                "context": { "type": "string" }
            },
            "required": ["question"]
        }));

        assert_eq!(
            required_names(&normalized["required"]),
            vec!["context".to_string(), "question".to_string()]
        );
        assert_eq!(
            normalized["properties"]["context"]["type"],
            json!(["string", "null"])
        );
        validate_strict_schema(&normalized, "test").expect("normalized schema should validate");
    }

    #[test]
    fn normalizes_nested_object_fields() {
        let normalized = normalize_schema_strict(&json!({
            "type": "object",
            "properties": {
                "payload": {
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean" },
                        "label": { "type": "string" }
                    },
                    "required": ["enabled"]
                }
            },
            "required": []
        }));

        assert_eq!(
            normalized["properties"]["payload"]["type"],
            json!(["object", "null"])
        );
        assert_eq!(
            normalized["properties"]["payload"]["properties"]["label"]["type"],
            json!(["string", "null"])
        );
        validate_strict_schema(&normalized["properties"]["payload"], "payload")
            .expect("nested payload schema should validate");
    }
}
