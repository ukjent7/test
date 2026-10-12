mod collapse;
mod markup;
mod stream;
mod tools;

use std::{
    collections::BTreeSet,
    hash::{DefaultHasher, Hash, Hasher},
    sync::RwLock,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::http::{HeaderMap, header};
use serde_json::Value;

pub use collapse::{collapse, collapse_messages};
pub use stream::StreamFix;
pub use tools::prepare_free_body;

use crate::cache;

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Zen 会拒绝过旧的 OpenCode 版本（426），跟随 OpenCode 的当前发布。
const OPENCODE_VERSION: &str = "1.18.34";

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

static DYNAMIC_FREE_MODELS: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

/// Zen 的免费模型：-free 后缀、已知不带后缀的免费 id、或来自 models.dev 0 成本清单。
pub fn is_free_model(model: &str) -> bool {
    if model.ends_with("-free") || matches!(model, "big-pickle" | "grok-code") {
        return true;
    }
    DYNAMIC_FREE_MODELS
        .read()
        .is_ok_and(|guard| guard.contains(model))
}

/// 尝试从 models.dev 异步同步 OpenCode 的免费模型（输入/输出均为 0.00）。
pub async fn sync_models_dev(client: reqwest::Client) {
    let url = "https://models.dev/api.json";
    let Ok(response) = client
        .get(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    else {
        return;
    };
    let Ok(bytes) = response.bytes().await else {
        return;
    };
    let Ok(data) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    let Some(models) = data
        .get("opencode")
        .and_then(|v| v.get("models"))
        .and_then(Value::as_object)
    else {
        return;
    };
    let mut free = BTreeSet::new();
    for (id, info) in models {
        let cost = &info["cost"];
        let input_zero = cost["input"].as_f64().is_some_and(|v| v == 0.0);
        let output_zero = cost["output"].as_f64().is_some_and(|v| v == 0.0);
        if input_zero && output_zero {
            free.insert(id.clone());
        }
    }
    if !free.is_empty()
        && let Ok(mut guard) = DYNAMIC_FREE_MODELS.write()
    {
        *guard = free;
    }
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
    clean.insert(
        header::USER_AGENT,
        format!("opencode/{OPENCODE_VERSION}").parse().unwrap(),
    );
    clean.insert("x-opencode-session", session.parse().unwrap());
    // OpenCode 还带每轮一个 msg_ id 与客户端、项目标签，照它发送。
    clean.insert("x-opencode-request", turn_id().parse().unwrap());
    clean.insert("x-opencode-client", "cli".parse().unwrap());
    clean.insert("x-opencode-project", "global".parse().unwrap());
    *headers = clean;
    (source, session)
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
    let time = hash.finish();
    "ses".hash(&mut hash);
    (source, opencode_id("ses", time, hash.finish()))
}

/// 每次上游请求一个 msg_ 形态的轮次 id，如同 OpenCode 自己发送。
fn turn_id() -> String {
    let mut hash = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_nanos() as u64)
        .unwrap_or(0)
        .hash(&mut hash);
    let time = hash.finish();
    "msg".hash(&mut hash);
    opencode_id("msg", time, hash.finish())
}

fn opencode_id(prefix: &str, time: u64, random: u64) -> String {
    let mut suffix = [b'0'; 14];
    let mut random = random;
    for byte in suffix.iter_mut().rev() {
        *byte = BASE62[(random % 62) as usize];
        random /= 62;
    }
    format!(
        "{prefix}_{:012x}{}",
        time & 0xffff_ffff_ffff,
        std::str::from_utf8(&suffix).unwrap()
    )
}
