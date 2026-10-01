use std::{
    collections::BTreeMap,
    hash::{DefaultHasher, Hash, Hasher},
};

use axum::http::{HeaderMap, header};
use serde_json::{Value, json};

use crate::{ApiError, app::Change, cache};

pub fn is_zen(url: &reqwest::Url) -> bool {
    url.host_str() == Some("opencode.ai")
        && matches!(
            url.path(),
            "/zen/v1/messages"
                | "/zen/v1/chat/completions"
                | "/zen/v1/responses"
                | "/zen/v1/models"
        )
}

pub fn prepare_headers(
    headers: &mut HeaderMap,
    request: &Value,
    anthropic: bool,
) -> (&'static str, String) {
    let (source, session) = session_id(headers, request);
    let key = headers.get("x-api-key").cloned().or_else(|| {
        headers
            .get(header::AUTHORIZATION)?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?
            .parse()
            .ok()
    });
    let mut clean = HeaderMap::new();
    if anthropic {
        for name in ["anthropic-version", "anthropic-beta"] {
            if let Some(value) = headers.get(name) {
                clean.insert(name, value.clone());
            }
        }
        clean
            .entry("anthropic-version")
            .or_insert("2023-06-01".parse().unwrap());
    }
    if let Some(key) = key {
        if anthropic {
            clean.insert("x-api-key", key);
        } else {
            clean.insert(
                header::AUTHORIZATION,
                format!("Bearer {}", key.to_str().unwrap()).parse().unwrap(),
            );
        }
    }
    clean.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    clean.insert(
        header::ACCEPT,
        "application/json, text/event-stream".parse().unwrap(),
    );
    clean.insert(header::USER_AGENT, "opencode/1.18.31".parse().unwrap());
    clean.insert("x-opencode-session", session.parse().unwrap());
    *headers = clean;
    (source, session)
}

pub fn prepare_free_body(request: &mut Value, endpoint: &str) -> Option<Change> {
    if request.get("tools").is_some_and(|tools| !tools.is_array()) {
        return None;
    }
    let before = request.clone();
    let had_tools = request["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty());
    request["stream"] = true.into();
    if !had_tools && request.get("tool_choice").is_none() {
        request["tool_choice"] = "none".into();
    }
    if request.get("tools").is_none() {
        request["tools"] = json!([]);
    }
    let tools = request["tools"].as_array_mut().unwrap();
    for name in ["bash", "edit", "glob", "grep", "read"] {
        if tools.iter().any(|tool| {
            if endpoint == "chat/completions" {
                tool["function"]["name"] == name
            } else {
                tool["name"] == name
            }
        }) {
            continue;
        }
        let mut tool = json!({"name": name, "description": "Compatibility marker; do not call.",
                              "parameters": {"type": "object", "properties": {}}});
        if endpoint == "chat/completions" {
            tool = json!({"type": "function", "function": tool});
        } else {
            tool["type"] = "function".into();
        }
        tools.push(tool);
    }
    (before != *request).then(|| {
        Change::new(
            String::new(),
            Some(before),
            Some(request.clone()),
            "Zen 免费层要求流式与基础工具",
        )
    })
}

pub fn collapse(bytes: &[u8], endpoint: &str) -> Result<Value, ApiError> {
    let invalid = || {
        ApiError(
            axum::http::StatusCode::BAD_GATEWAY,
            "上游免费模型的流式响应不完整或无效".into(),
        )
    };
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
            .ok_or_else(invalid);
    }
    if !done {
        return Err(invalid());
    }
    let mut response = json!({"object": "chat.completion"});
    let mut choices = BTreeMap::new();
    for event in events {
        let Some(fields) = event.as_object() else {
            return Err(invalid());
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
        return Err(invalid());
    }
    response["choices"] = Value::Array(choices.into_values().collect());
    Ok(response)
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

fn session_id(headers: &HeaderMap, request: &Value) -> (&'static str, String) {
    let identity = cache::identity(headers, request);
    if let Some((source, signal)) = identity
        && signal.len() == 30
        && signal.starts_with("ses_")
        && signal.as_bytes()[4..16]
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        && signal.as_bytes()[16..]
            .iter()
            .all(u8::is_ascii_alphanumeric)
    {
        return (source, signal.to_owned());
    }
    let first_turn = request
        .get("messages")
        .or_else(|| request.get("input"))
        .and_then(Value::as_array)
        .and_then(|messages| messages.iter().find(|message| message["role"] == "user"))
        .map(|message| message["content"].to_string())
        .unwrap_or_else(|| request["input"].as_str().unwrap_or("").to_owned());
    // Routing identity only: deterministic within this build, never an auth token.
    let mut hash = DefaultHasher::new();
    headers
        .get("x-api-key")
        .or_else(|| headers.get(header::AUTHORIZATION))
        .map(|key| key.as_bytes())
        .hash(&mut hash);
    let (source, signal) = identity.unwrap_or((
        if request
            .get("messages")
            .or_else(|| request.get("input"))
            .is_some_and(Value::is_array)
        {
            "first_user_message"
        } else {
            "input"
        },
        &first_turn,
    ));
    signal.hash(&mut hash);
    let time = hash.finish() & 0xffff_ffff_ffff;
    "ses".hash(&mut hash);
    let mut random = hash.finish();
    let mut suffix = [b'0'; 14];
    for byte in suffix.iter_mut().rev() {
        *byte = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
            [(random % 62) as usize];
        random /= 62;
    }
    (
        source,
        format!("ses_{time:012x}{}", std::str::from_utf8(&suffix).unwrap()),
    )
}
