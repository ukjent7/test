use std::collections::BTreeMap;

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::error::ApiError;

const INVALID_STREAM: &str = "上游免费模型的流式响应不完整或无效";

/// 把免费模型的流式回复聚合成客户端要的单条 chat.completion / response。
pub fn collapse(bytes: &[u8], endpoint: &str) -> Result<Value, ApiError> {
    let (events, done) = parse_events(bytes)?;
    if endpoint == "responses" {
        return events
            .into_iter()
            .rev()
            .find(|event| {
                matches!(
                    event["type"].as_str(),
                    Some("response.completed" | "response.incomplete" | "response.failed")
                )
            })
            .and_then(|mut event| {
                event
                    .get_mut("response")
                    .filter(|response| response.is_object())
                    .map(Value::take)
            })
            .ok_or_else(|| ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()));
    }
    if !done {
        return Err(ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()));
    }
    let mut response = json!({"object": "chat.completion"});
    let mut choices = BTreeMap::new();
    for event in events {
        let Some(fields) = event.as_object() else {
            return Err(ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()));
        };
        for (key, value) in fields {
            if !matches!(key.as_str(), "object" | "choices") && !value.is_null() {
                response[key] = value.clone();
            }
        }
        for choice in event["choices"].as_array().into_iter().flatten() {
            let index = choice["index"].as_u64().unwrap_or(0);
            let output = choices.entry(index).or_insert_with(|| {
                json!({"index": index,
                "message": {"role": "assistant", "content": ""}, "finish_reason": null})
            });
            merge_delta(&mut output["message"], &choice["delta"]);
            if !choice["finish_reason"].is_null() {
                output["finish_reason"] = choice["finish_reason"].clone();
            }
            if !choice["logprobs"].is_null() {
                merge_delta(&mut output["logprobs"], &choice["logprobs"]);
            }
        }
    }
    if choices.is_empty() {
        return Err(ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()));
    }
    response["choices"] = Value::Array(choices.into_values().collect());
    Ok(response)
}

/// 把 Anthropic 形态的流式回复聚合成客户端要的单条 message JSON。
pub fn collapse_messages(bytes: &[u8]) -> Result<Value, ApiError> {
    let (events, _) = parse_events(bytes)?;
    let mut message = json!({"type": "message", "role": "assistant", "content": [],
        "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": null, "output_tokens": null}});
    for event in events {
        match event["type"].as_str() {
            Some("message_start") => {
                for field in ["id", "model", "role"] {
                    if !event["message"][field].is_null() {
                        message[field] = event["message"][field].clone();
                    }
                }
                merge_usage(&mut message["usage"], &event["message"]["usage"]);
            }
            Some("content_block_start") => {
                let index = event["index"].as_u64().unwrap_or(
                    message["content"]
                        .as_array()
                        .map_or(0, |blocks| blocks.len() as u64),
                ) as usize;
                let Some(blocks) = message["content"].as_array_mut() else {
                    continue;
                };
                while blocks.len() <= index {
                    blocks.push(Value::Null);
                }
                blocks[index] = event["content_block"].clone();
            }
            Some("content_block_delta") => {
                let Some(blocks) = message["content"].as_array_mut() else {
                    continue;
                };
                let Some(block) =
                    blocks.get_mut(event["index"].as_u64().unwrap_or(u64::MAX) as usize)
                else {
                    continue;
                };
                match event["delta"]["type"].as_str() {
                    Some("text_delta") => append_str(&mut block["text"], &event["delta"]["text"]),
                    Some("thinking_delta") => {
                        append_str(&mut block["thinking"], &event["delta"]["thinking"]);
                    }
                    Some("signature_delta") => {
                        append_str(&mut block["signature"], &event["delta"]["signature"]);
                    }
                    Some("input_json_delta") => {
                        append_str(&mut block["partial_json"], &event["delta"]["partial_json"]);
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let Some(blocks) = message["content"].as_array_mut() else {
                    continue;
                };
                let Some(block) =
                    blocks.get_mut(event["index"].as_u64().unwrap_or(u64::MAX) as usize)
                else {
                    continue;
                };
                if let Some(partial) = block["partial_json"].as_str() {
                    let input = if partial.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(partial)
                            .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()))?
                    };
                    block["input"] = input;
                    if let Some(object) = block.as_object_mut() {
                        object.remove("partial_json");
                    }
                }
            }
            Some("message_delta") => {
                for field in ["stop_reason", "stop_sequence"] {
                    if !event["delta"][field].is_null() {
                        message[field] = event["delta"][field].clone();
                    }
                }
                merge_usage(&mut message["usage"], &event["usage"]);
            }
            Some("message_stop") => break,
            _ => {}
        }
    }
    Ok(message)
}

fn parse_events(bytes: &[u8]) -> Result<(Vec<Value>, bool), ApiError> {
    let invalid = || ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into());
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid())?
        .replace("\r\n", "\n");
    let mut events = Vec::new();
    let mut done = false;
    for frame in text.split("\n\n") {
        let data = frame
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data:")
                    .map(|data| data.strip_prefix(' ').unwrap_or(data))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            done = true;
            continue;
        }
        let event: Value = serde_json::from_str(&data).map_err(|_| invalid())?;
        if event["error"].is_object() || event["type"] == "error" {
            return Err(invalid());
        }
        events.push(event);
    }
    Ok((events, done))
}

fn append_str(target: &mut Value, text: &Value) {
    if let Some(text) = text.as_str() {
        *target = format!("{}{text}", target.as_str().unwrap_or("")).into();
    }
}

fn merge_usage(target: &mut Value, usage: &Value) {
    if let (Some(target), Some(fields)) = (target.as_object_mut(), usage.as_object()) {
        for (key, value) in fields {
            if !value.is_null() {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn merge_delta(target: &mut Value, delta: &Value) {
    match delta {
        Value::Null => {}
        Value::Object(fields) => {
            if !target.is_object() {
                *target = json!({});
            }
            for (key, value) in fields {
                if value.is_null() {
                    continue;
                }
                if matches!(key.as_str(), "id" | "type" | "role" | "format" | "index") {
                    target[key] = value.clone();
                } else {
                    merge_delta(&mut target[key], value);
                }
            }
        }
        Value::Array(items) => {
            if !target.is_array() {
                *target = json!([]);
            }
            let output = target.as_array_mut().unwrap();
            for item in items {
                if let Some(index) = item.get("index")
                    && let Some(existing) = output
                        .iter_mut()
                        .find(|existing| existing["index"] == *index)
                {
                    merge_delta(existing, item);
                } else {
                    output.push(item.clone());
                }
            }
        }
        Value::String(text) => *target = format!("{}{text}", target.as_str().unwrap_or("")).into(),
        value => *target = value.clone(),
    }
}
