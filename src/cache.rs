use axum::http::HeaderMap;
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
pub struct Routing {
    pub source: &'static str,
    pub fingerprint: String,
}

pub fn identity<'a>(headers: &'a HeaderMap, request: &'a Value) -> Option<(&'static str, &'a str)> {
    [
        "x-opencode-session",
        "x-session-affinity",
        "x-session-id",
        "conversation-id",
        "session-id",
        "session_id",
    ]
    .into_iter()
    .map(|name| {
        (
            name,
            headers.get(name).and_then(|value| value.to_str().ok()),
        )
    })
    .chain([
        ("prompt_cache_key", request["prompt_cache_key"].as_str()),
        (
            "x-grok-conv-id",
            headers
                .get("x-grok-conv-id")
                .and_then(|value| value.to_str().ok()),
        ),
        (
            "metadata.session_id",
            request["metadata"]["session_id"].as_str(),
        ),
        ("conversation_id", request["conversation_id"].as_str()),
    ])
    .find_map(|(source, value)| {
        value
            .filter(|value| !value.is_empty())
            .map(|value| (source, value))
    })
}

#[derive(Default, Serialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    #[serde(skip)]
    messages_input_tokens: Option<u64>,
}

impl Usage {
    pub fn observe(&mut self, endpoint: &str, value: &Value) {
        let value = value
            .get("message")
            .or_else(|| value.get("response"))
            .unwrap_or(value);
        let Some(usage) = value.get("usage") else {
            return;
        };
        let input = usage[if endpoint == "chat/completions" {
            "prompt_tokens"
        } else {
            "input_tokens"
        }]
        .as_u64();
        let (read, write) = match endpoint {
            "messages" => (
                usage["cache_read_input_tokens"].as_u64(),
                usage["cache_creation_input_tokens"].as_u64(),
            ),
            "chat/completions" => (
                usage["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .or_else(|| usage["prompt_cache_hit_tokens"].as_u64())
                    .or_else(|| usage["cached_tokens"].as_u64()),
                usage["prompt_tokens_details"]["cache_write_tokens"].as_u64(),
            ),
            _ => (
                usage["input_tokens_details"]["cached_tokens"].as_u64(),
                None,
            ),
        };
        // Streaming usage is cumulative; omitted fields retain their last reported value.
        self.cache_read_tokens = read.or(self.cache_read_tokens);
        self.cache_write_tokens = write.or(self.cache_write_tokens);
        if endpoint == "messages" {
            self.messages_input_tokens = input.or(self.messages_input_tokens);
            self.input_tokens = self.messages_input_tokens.map(|input| {
                input + self.cache_read_tokens.unwrap_or(0) + self.cache_write_tokens.unwrap_or(0)
            });
        } else {
            self.input_tokens = input.or(self.input_tokens);
        }
    }
}
