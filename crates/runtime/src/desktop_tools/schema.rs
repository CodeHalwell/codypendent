use super::*;
use agent_framework_core::tools::{ApprovalMode, ToolKind};

pub(super) fn sanitize_context(context: Value) -> Result<Value, String> {
    let object = context
        .as_object()
        .ok_or("desktop.invalid_context: expected an object")?;
    let allowed = [
        "view",
        "appearance",
        "repository",
        "sessionId",
        "runId",
        "activeFile",
        "openFiles",
        "selectedModel",
        "mode",
        "draftLength",
        "title",
    ];
    let filtered: serde_json::Map<String, Value> = object
        .iter()
        .filter(|(key, value)| {
            allowed.contains(&key.as_str())
                && match key.as_str() {
                    "draftLength" => value.is_u64(),
                    "openFiles" => value.as_array().is_some_and(|files| {
                        files.len() <= 32 && files.iter().all(Value::is_string)
                    }),
                    _ => value.is_string() || value.is_null(),
                }
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let value = Value::Object(filtered);
    if value.to_string().len() > 16 * 1024 {
        return Err("desktop.context_limit: context exceeds 16 KiB".into());
    }
    Ok(value)
}

pub(super) fn validate_definition(tool: &DesktopTool, custom: bool) -> Result<(), String> {
    if tool.name.len() > 96
        || !tool
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || (custom
            && (!tool.name.starts_with("desktop.custom.")
                || tool.name.len() == "desktop.custom.".len()))
        || (!custom
            && (!tool.name.starts_with("desktop.")
                || tool.name.starts_with("desktop.custom.")
                || [DEFINE, MANAGE, EXECUTE].contains(&tool.name.as_str())))
    {
        return Err("desktop.invalid_name: use desktop.<name> for host tools or desktop.custom.<name> for generated tools".into());
    }
    if tool.description.trim().is_empty()
        || tool.description.len() > 2048
        || tool.capabilities.len() > MAX_TOOLS
    {
        return Err("desktop.invalid_definition".into());
    }
    if custom != tool.function_body.is_some()
        || tool
            .function_body
            .as_ref()
            .is_some_and(|body| body.len() > 64 * 1024 || body.trim().is_empty())
    {
        return Err("desktop.invalid_function: generated tools need a sandbox function body of at most 64 KiB".into());
    }
    if !custom && !tool.capabilities.is_empty() {
        return Err("desktop.invalid_host_capabilities".into());
    }
    if tool
        .language
        .as_deref()
        .is_some_and(|language| !custom || !matches!(language, "javascript" | "python"))
    {
        return Err(
            "desktop.invalid_language: use python or javascript for generated helpers".into(),
        );
    }
    validate_schema(&tool.parameters, 0)?;
    if tool.parameters.get("type").and_then(Value::as_str) != Some("object") {
        return Err("desktop.invalid_schema: root parameters must be an object".into());
    }
    bounded(&serde_json::to_value(tool).map_err(|error| error.to_string())?)
}

fn validate_schema(schema: &Value, depth: usize) -> Result<(), String> {
    let raw = schema;
    let schema = schema
        .as_object()
        .ok_or("desktop.invalid_schema: expected an object")?;
    let allowed = [
        "type",
        "description",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "title",
        "default",
    ];
    if depth > 12 || schema.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("desktop.invalid_schema: unsupported keyword or excessive nesting".into());
    }
    if !matches!(
        schema.get("type").and_then(Value::as_str),
        Some("object" | "array" | "string" | "number" | "integer" | "boolean" | "null")
    ) {
        return Err("desktop.invalid_schema: a concrete type is required".into());
    }
    if let Some(properties) = schema.get("properties") {
        for child in properties
            .as_object()
            .ok_or("desktop.invalid_properties")?
            .values()
        {
            validate_schema(child, depth + 1)?;
        }
    }
    if let Some(items) = schema.get("items") {
        validate_schema(items, depth + 1)?;
    }
    if schema
        .get("additionalProperties")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("desktop.invalid_schema: additionalProperties must be boolean".into());
    }
    let kind = schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    for key in ["description", "title"] {
        if schema.get(key).is_some_and(|value| !value.is_string()) {
            return Err(format!("desktop.invalid_schema: {key} must be a string"));
        }
    }
    if kind != "object"
        && ["properties", "required", "additionalProperties"]
            .iter()
            .any(|key| schema.contains_key(*key))
    {
        return Err("desktop.invalid_schema: object keywords need object type".into());
    }
    if kind != "array"
        && ["items", "minItems", "maxItems"]
            .iter()
            .any(|key| schema.contains_key(*key))
    {
        return Err("desktop.invalid_schema: array keywords need array type".into());
    }
    if kind != "string"
        && ["minLength", "maxLength"]
            .iter()
            .any(|key| schema.contains_key(*key))
    {
        return Err("desktop.invalid_schema: string bounds need string type".into());
    }
    if !["integer", "number"].contains(&kind)
        && ["minimum", "maximum"]
            .iter()
            .any(|key| schema.contains_key(*key))
    {
        return Err("desktop.invalid_schema: numeric bounds need number type".into());
    }
    if let Some(required) = schema.get("required") {
        let required = required
            .as_array()
            .ok_or("desktop.invalid_schema: required must be a string array")?;
        let mut seen = HashSet::new();
        for key in required {
            let key = key
                .as_str()
                .ok_or("desktop.invalid_schema: required must be a string array")?;
            if !seen.insert(key)
                || !schema
                    .get("properties")
                    .is_some_and(|properties| properties.get(key).is_some())
            {
                return Err(
                    "desktop.invalid_schema: required must contain unique declared property names"
                        .into(),
                );
            }
        }
    }
    for (minimum, maximum) in [("minLength", "maxLength"), ("minItems", "maxItems")] {
        for key in [minimum, maximum] {
            if schema
                .get(key)
                .is_some_and(|value| value.as_u64().is_none())
            {
                return Err(format!(
                    "desktop.invalid_schema: {key} must be a nonnegative integer"
                ));
            }
        }
        if let (Some(min), Some(max)) = (
            schema.get(minimum).and_then(Value::as_u64),
            schema.get(maximum).and_then(Value::as_u64),
        ) {
            if min > max {
                return Err("desktop.invalid_schema: minimum length exceeds maximum".into());
            }
        }
    }
    for key in ["minimum", "maximum"] {
        if schema
            .get(key)
            .is_some_and(|value| value.as_f64().is_none())
        {
            return Err(format!("desktop.invalid_schema: {key} must be numeric"));
        }
    }
    if let (Some(min), Some(max)) = (
        schema.get("minimum").and_then(Value::as_f64),
        schema.get("maximum").and_then(Value::as_f64),
    ) {
        if min > max {
            return Err("desktop.invalid_schema: minimum exceeds maximum".into());
        }
    }
    if let Some(variants) = schema.get("enum") {
        let variants = variants
            .as_array()
            .filter(|variants| !variants.is_empty())
            .ok_or("desktop.invalid_schema: enum must be a nonempty array")?;
        for (index, variant) in variants.iter().enumerate() {
            if variants[..index].contains(variant) || validate_value(raw, variant, depth).is_err() {
                return Err(
                    "desktop.invalid_schema: enum values must be unique and satisfy the schema"
                        .into(),
                );
            }
        }
    }
    if let Some(default) = schema.get("default") {
        validate_value(raw, default, depth)
            .map_err(|_| "desktop.invalid_schema: default must satisfy the schema")?;
    }
    Ok(())
}

pub(super) fn validate_value(schema: &Value, value: &Value, depth: usize) -> Result<(), String> {
    if depth > 12 {
        return Err("desktop.invalid_args: excessive nesting".into());
    }
    let matches = match schema.get("type").and_then(Value::as_str) {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("number") => value.is_number(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("boolean") => value.is_boolean(),
        Some("null") => value.is_null(),
        _ => false,
    };
    if !matches
        || schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|allowed| !allowed.contains(value))
    {
        return Err("desktop.invalid_args: type or enum mismatch".into());
    }
    if let Some(object) = value.as_object() {
        if schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| {
                required
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|key| !object.contains_key(key))
            })
        {
            return Err("desktop.invalid_args: missing required property".into());
        }
        for (key, value) in object {
            if let Some(child) = schema
                .get("properties")
                .and_then(|properties| properties.get(key))
            {
                validate_value(child, value, depth + 1)?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(format!("desktop.invalid_args: unexpected property {key}"));
            }
        }
    }
    if let Some(array) = value.as_array() {
        if let Some(items) = schema.get("items") {
            for item in array {
                validate_value(items, item, depth + 1)?;
            }
        }
    }
    let length = value
        .as_str()
        .map(|text| text.chars().count())
        .or_else(|| value.as_array().map(Vec::len));
    if let Some(length) = length {
        for (key, minimum) in [
            ("minLength", true),
            ("maxLength", false),
            ("minItems", true),
            ("maxItems", false),
        ] {
            if schema
                .get(key)
                .and_then(Value::as_u64)
                .is_some_and(|bound| {
                    if minimum {
                        length < bound as usize
                    } else {
                        length > bound as usize
                    }
                })
            {
                return Err("desktop.invalid_args: length outside schema bounds".into());
            }
        }
    }
    if let Some(number) = value.as_f64() {
        if schema
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|min| number < min)
            || schema
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|max| number > max)
        {
            return Err("desktop.invalid_args: number outside schema bounds".into());
        }
    }
    Ok(())
}

pub(super) fn validate_widget(args: &Value) -> Result<(), String> {
    bounded(args)?;
    if args
        .get("html")
        .and_then(Value::as_str)
        .is_none_or(|html| html.trim().is_empty() || html.len() > 128 * 1024)
    {
        return Err("desktop.invalid_widget: html must contain 1 to 131072 bytes".into());
    }
    Ok(())
}

pub(super) fn definition(name: &str, description: &str, parameters: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
        kind: ToolKind::Function,
        approval_mode: ApprovalMode::NeverRequire,
        executor: None,
    }
}

pub(super) fn builtin_definitions() -> Vec<ToolDefinition> {
    vec![
        definition(RENDER, "Display a useful interactive answer as HTML/CSS/JavaScript in the desktop's isolated sandbox. No native API, filesystem or network access. Stream fields in this order: initialHeight, placeholderMessages, css, html, jsFunctions, jsExpressions.", json!({"type":"object","properties":{"initialHeight":{"type":"integer","minimum":120,"maximum":1200},"placeholderMessages":{"type":"array","items":{"type":"string"},"maxItems":8},"css":{"type":"string","maxLength":65536},"html":{"type":"string","minLength":1,"maxLength":131072},"jsFunctions":{"type":"string","maxLength":65536},"jsExpressions":{"type":"array","items":{"type":"string"},"maxItems":64}},"required":["html"],"additionalProperties":false})),
        definition(DEFINE, "Create or update a reusable helper for this chat. Name starts desktop.custom. Supply an object JSON Schema, functionBody source and declared host capabilities. Prefer language python: source runs in a native Monty subprocess, args is an input dict, host_call(name, args) calls only declared capabilities, and return a JSON value. Example body: return host_call('desktop.getWorkspace', {}). For language javascript (the default for existing helpers), use an async function body with await host.call(name,args) and return JSON. Both have a five-second limit and no direct native, shell, filesystem or network access. Source, language, version and capabilities pass native approval. Supported schema keywords: type,description,properties,required,additionalProperties(boolean),items,enum,minimum,maximum,minLength,maxLength,minItems,maxItems,title,default.", json!({"type":"object","properties":{"name":{"type":"string","maxLength":96},"description":{"type":"string","maxLength":2048},"parameters":{"type":"object"},"functionBody":{"type":"string","maxLength":65536},"language":{"type":"string","enum":["python","javascript"]},"capabilities":{"type":"array","items":{"type":"string"},"maxItems":64}},"required":["name","description","parameters","functionBody","capabilities"],"additionalProperties":false})),
        definition(MANAGE, "List saved generated tools including source, schemas, capabilities and versions, or delete a generated tool. Update a tool with desktop.defineTool using the same name.", json!({"type":"object","properties":{"operation":{"type":"string","enum":["list","delete"]},"name":{"type":"string"}},"required":["operation"],"additionalProperties":false})),
        definition(EXECUTE, "Execute a saved generated tool by its full desktop.custom name and argument object. Use this stable entry point immediately after desktop.defineTool when your client cannot refresh its tool list. The generated source, version and declared host capabilities still pass native policy and approval before execution.", json!({"type":"object","properties":{"name":{"type":"string","maxLength":96},"args":{"type":"object"}},"required":["name","args"],"additionalProperties":false})),
    ]
}
