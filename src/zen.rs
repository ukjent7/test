use std::hash::{DefaultHasher, Hash, Hasher};

use axum::http::{HeaderMap, header};
use serde_json::Value;

pub fn is_zen(url: &reqwest::Url) -> bool {
    url.host_str() == Some("opencode.ai") && url.path() == "/zen/v1/messages"
}

pub fn prepare_headers(headers: &mut HeaderMap, request: &Value) {
    let session = session_id(headers, request);
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
    for name in ["anthropic-version", "anthropic-beta"] {
        if let Some(value) = headers.get(name) {
            clean.insert(name, value.clone());
        }
    }
    if let Some(key) = key {
        clean.insert("x-api-key", key);
    }
    clean
        .entry("anthropic-version")
        .or_insert("2023-06-01".parse().unwrap());
    clean.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    clean.insert(
        header::ACCEPT,
        "application/json, text/event-stream".parse().unwrap(),
    );
    clean.insert(header::USER_AGENT, "opencode/1.18.31".parse().unwrap());
    clean.insert("x-opencode-session", session.parse().unwrap());
    *headers = clean;
}

fn session_id(headers: &HeaderMap, request: &Value) -> String {
    let signal = [
        "x-opencode-session",
        "x-session-affinity",
        "x-session-id",
        "conversation-id",
    ]
    .into_iter()
    .filter_map(|name| headers.get(name)?.to_str().ok())
    .chain(request["metadata"]["session_id"].as_str())
    .chain(request["conversation_id"].as_str())
    .find(|signal| !signal.is_empty());
    if let Some(signal) = signal
        && signal.len() == 30
        && signal.starts_with("ses_")
        && signal.as_bytes()[4..16]
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        && signal.as_bytes()[16..]
            .iter()
            .all(u8::is_ascii_alphanumeric)
    {
        return signal.to_owned();
    }
    let first_turn = request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().find(|message| message["role"] == "user"))
        .map(|message| message["content"].to_string())
        .unwrap_or_default();
    // Routing identity only: deterministic within this build, never an auth token.
    let mut hash = DefaultHasher::new();
    headers
        .get("x-api-key")
        .or_else(|| headers.get(header::AUTHORIZATION))
        .map(|key| key.as_bytes())
        .hash(&mut hash);
    signal.unwrap_or(&first_turn).hash(&mut hash);
    let time = hash.finish() & 0xffff_ffff_ffff;
    "ses".hash(&mut hash);
    let mut random = hash.finish();
    let mut suffix = [b'0'; 14];
    for byte in suffix.iter_mut().rev() {
        *byte = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
            [(random % 62) as usize];
        random /= 62;
    }
    format!("ses_{time:012x}{}", std::str::from_utf8(&suffix).unwrap())
}
