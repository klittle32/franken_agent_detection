//! Shared Pi-family wire helpers.
//!
//! Prime Agent and Pi share content-block, tool-call, image, and usage shapes.
//! This module is `pub(super)` so Prime can reuse those primitives without
//! inheriting Pi identity, roots, titles, or resume semantics.

use serde_json::{Map, Value};

use crate::types::NormalizedInvocation;

/// Flattened Pi-family content used by Prime (and tests).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct FlattenedPiContent {
    pub searchable_text: String,
    pub invocations: Vec<NormalizedInvocation>,
    pub image_mime_types: Vec<String>,
    pub had_reasoning: bool,
}

/// One recognized content block inside a Pi-family message.
#[derive(Debug, Clone)]
pub(super) enum PiContentBlock<'a> {
    Text(&'a str),
    Thinking(&'a str),
    ToolCall {
        name: &'a str,
        id: Option<&'a str>,
        arguments: Option<&'a Value>,
    },
    Image {
        mime: Option<&'a str>,
    },
    Other,
}

/// Recursively sort object keys for deterministic searchable JSON.
#[must_use]
pub(super) fn deterministic_json(value: &Value) -> String {
    serde_json::to_string(&sort_value(value)).unwrap_or_else(|_| value.to_string())
}

fn sort_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                if let Some(child) = map.get(key) {
                    out.insert(key.clone(), sort_value(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// MIME-only image placeholder. Never includes base64 data.
#[must_use]
pub(super) fn image_placeholder(mime: &str) -> String {
    format!("[image: {mime}]")
}

#[must_use]
pub(super) fn mime_from_image_block(block: &Value) -> Option<&str> {
    block
        .get("mimeType")
        .or_else(|| block.get("mime_type"))
        .and_then(Value::as_str)
        .or_else(|| block.get("media_type").and_then(Value::as_str))
}

/// Visit Pi-family content blocks in source order.
pub(super) fn visit_pi_content_blocks(
    content: &Value,
    mut on_block: impl FnMut(PiContentBlock<'_>),
) {
    if let Some(text) = content.as_str() {
        on_block(PiContentBlock::Text(text));
        return;
    }
    let Some(items) = content.as_array() else {
        return;
    };
    for item in items {
        let block_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        let block = match block_type {
            "text" => item
                .get("text")
                .and_then(Value::as_str)
                .map_or(PiContentBlock::Other, PiContentBlock::Text),
            "thinking" => item
                .get("thinking")
                .and_then(Value::as_str)
                .map_or(PiContentBlock::Other, PiContentBlock::Thinking),
            "toolCall" => PiContentBlock::ToolCall {
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                id: item.get("id").and_then(Value::as_str),
                arguments: item.get("arguments"),
            },
            "image" => PiContentBlock::Image {
                mime: mime_from_image_block(item),
            },
            _ => PiContentBlock::Other,
        };
        on_block(block);
    }
}

/// Extract every toolCall block as a structured invocation, in block order.
#[must_use]
pub(super) fn extract_tool_invocations(content: &Value) -> Vec<NormalizedInvocation> {
    let mut invocations = Vec::new();
    visit_pi_content_blocks(content, |block| {
        if let PiContentBlock::ToolCall {
            name,
            id,
            arguments,
        } = block
        {
            invocations.push(NormalizedInvocation {
                kind: "tool".to_string(),
                name: name.to_string(),
                raw_name: None,
                call_id: id.map(str::to_string),
                arguments: arguments.cloned(),
            });
        }
    });
    invocations
}

/// Prime-style searchable flatten: keep block order, all tool args, MIME
/// placeholders, and no image bodies.
#[must_use]
pub(super) fn flatten_pi_family_content(content: &Value) -> FlattenedPiContent {
    if let Some(text) = content.as_str() {
        return FlattenedPiContent {
            searchable_text: text.to_string(),
            ..FlattenedPiContent::default()
        };
    }

    let mut parts = Vec::new();
    let mut invocations = Vec::new();
    let mut image_mime_types = Vec::new();
    let mut had_reasoning = false;

    visit_pi_content_blocks(content, |block| match block {
        PiContentBlock::Text(text) => parts.push(text.to_string()),
        PiContentBlock::Thinking(text) => {
            had_reasoning = true;
            parts.push(format!("[reasoning]\n{text}"));
        }
        PiContentBlock::ToolCall {
            name,
            id,
            arguments,
        } => {
            let args = arguments.cloned().unwrap_or(Value::Null);
            parts.push(format!(
                "[tool call: {name}]\n{}",
                deterministic_json(&args)
            ));
            invocations.push(NormalizedInvocation {
                kind: "tool".to_string(),
                name: name.to_string(),
                raw_name: None,
                call_id: id.map(str::to_string),
                arguments: arguments.cloned(),
            });
        }
        PiContentBlock::Image { mime } => {
            let mime = mime.unwrap_or("image");
            image_mime_types.push(mime.to_string());
            parts.push(image_placeholder(mime));
        }
        PiContentBlock::Other => {}
    });

    FlattenedPiContent {
        searchable_text: parts.join("\n"),
        invocations,
        image_mime_types,
        had_reasoning,
    }
}

pub(super) fn i64_usage_field(usage: &Value, names: &[&str]) -> Option<i64> {
    names
        .iter()
        .find_map(|name| usage.get(*name))
        .and_then(Value::as_i64)
}

/// Compact selected usage fields for Prime/Pi assistant extras.
#[must_use]
pub(super) fn compact_pi_family_usage(message: &Value, service_tier: Option<&str>) -> Value {
    let usage = message.get("usage");
    let mut out = Map::new();
    if let Some(usage) = usage {
        if let Some(input) = i64_usage_field(usage, &["input", "input_tokens"]) {
            out.insert("input".to_string(), Value::from(input));
            out.insert("input_tokens".to_string(), Value::from(input));
        }
        if let Some(output) = i64_usage_field(usage, &["output", "output_tokens"]) {
            out.insert("output".to_string(), Value::from(output));
            out.insert("output_tokens".to_string(), Value::from(output));
        }
        if let Some(cache_read) = i64_usage_field(usage, &["cacheRead", "cache_read_tokens"]) {
            out.insert("cacheRead".to_string(), Value::from(cache_read));
            out.insert("cache_read_tokens".to_string(), Value::from(cache_read));
        }
        if let Some(cache_write) = i64_usage_field(usage, &["cacheWrite", "cache_creation_tokens"])
        {
            out.insert("cacheWrite".to_string(), Value::from(cache_write));
            out.insert(
                "cache_creation_tokens".to_string(),
                Value::from(cache_write),
            );
        }
    }
    if let Some(tier) = service_tier.filter(|text| !text.is_empty()) {
        out.insert("service_tier".to_string(), Value::String(tier.to_string()));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deterministic_json_sorts_nested_keys() {
        let value = json!({"z": 1, "a": {"c": 2, "b": 3}});
        assert_eq!(deterministic_json(&value), r#"{"a":{"b":3,"c":2},"z":1}"#);
    }

    #[test]
    fn flatten_keeps_all_tool_args_and_image_placeholder() {
        let content = json!([
            {"type": "text", "text": "hello"},
            {"type": "thinking", "thinking": "reason"},
            {
                "type": "toolCall",
                "id": "call-1",
                "name": "read",
                "arguments": {"z": "last", "a": "first", "nested": {"b": 1, "a": 2}}
            },
            {"type": "image", "mimeType": "image/png", "data": "PRIME_BASE64_MUST_NOT_SURVIVE"}
        ]);
        let flat = flatten_pi_family_content(&content);
        assert!(flat.searchable_text.contains("hello"));
        assert!(flat.searchable_text.contains("[reasoning]\nreason"));
        assert!(flat.searchable_text.contains("[tool call: read]"));
        assert!(flat.searchable_text.contains(r#""a":"first""#));
        assert!(flat.searchable_text.contains(r#""z":"last""#));
        assert!(flat.searchable_text.contains("[image: image/png]"));
        assert!(
            !flat
                .searchable_text
                .contains("PRIME_BASE64_MUST_NOT_SURVIVE")
        );
        assert_eq!(flat.invocations.len(), 1);
        assert_eq!(flat.invocations[0].call_id.as_deref(), Some("call-1"));
        assert!(flat.had_reasoning);
    }
}
