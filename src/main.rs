#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod desktop;
mod zen;

use std::{env, io, net::SocketAddr, sync::Arc};

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};

use app::{Change, Gateway, Trace};

pub(crate) struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let kind = if self.0 == StatusCode::BAD_REQUEST {
            "invalid_request_error"
        } else {
            "api_error"
        };
        (
            self.0,
            Json(json!({"type": "error", "error": {"type": kind, "message": self.1}})),
        )
            .into_response()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let headless = env::args().any(|argument| argument == "--headless");
    let runtime = tokio::runtime::Runtime::new()?;
    let (listener, router) = match runtime.block_on(app::prepare()) {
        Ok(server) => server,
        Err(error) if !headless => return desktop::run(Err(error.to_string())),
        Err(error) => return Err(error),
    };
    let address = listener.local_addr()?;
    eprintln!("messages-gateway listening on {address}");
    let server = async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    };
    if headless {
        runtime.block_on(server)?;
    } else {
        runtime.spawn(server);
        desktop::run(Ok(format!("http://{address}/")))?;
    }
    Ok(())
}

async fn messages(
    State(gateway): State<Arc<Gateway>>,
    OriginalUri(uri): OriginalUri,
    mut headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let mut request: Value = serde_json::from_slice(&body)
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let mut trace = Trace::new(
        &gateway,
        request["model"].as_str().unwrap_or("未知模型").to_owned(),
    );
    let endpoint = uri.path().trim_start_matches("/v1").trim_start_matches('/');
    let [stepfun, opencode] = gateway.upstreams(endpoint).inspect_err(|error| {
        trace.status = error.0.as_u16();
    })?;
    let model = request["model"].as_str().unwrap_or("").to_owned();
    let (upstream, wire_model) = if let Some(model) = model.strip_prefix("opencode/") {
        (opencode, model)
    } else {
        (stepfun, model.strip_prefix("stepfun/").unwrap_or(&model))
    };
    let (mut url, key) = upstream;
    let anthropic = endpoint == "messages";
    if anthropic {
        trace.diff.request = normalize_history(&mut request);
    }
    if wire_model != model {
        trace.diff.request.push(Change::new(
            String::new(),
            Some(json!({"model": model})),
            Some(json!({"model": wire_model})),
            "模型前缀路由",
        ));
        request["model"] = wire_model.into();
    }
    strip_hop_headers(&mut headers);
    configured_key(&mut headers, &key)?;
    if zen::is_zen(&url) {
        zen::prepare_headers(&mut headers, &request, anthropic);
    }
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::ACCEPT_ENCODING, "identity".parse().unwrap());
    url.set_query(uri.query());
    let client_streaming = request["stream"].as_bool().unwrap_or(false);
    let mut collapse = false;
    let mut upstream = gateway
        .client
        .post(url.clone())
        .headers(headers.clone())
        .body(serde_json::to_vec(&request).unwrap())
        .send()
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
    if !anthropic && zen::is_zen(&url) && upstream.status() == StatusCode::FORBIDDEN {
        trace.status = 403;
        let mut original_headers = upstream.headers().clone();
        strip_hop_headers(&mut original_headers);
        let bytes = upstream
            .bytes()
            .await
            .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
        let free_error = serde_json::from_slice::<Value>(&bytes)
            .is_ok_and(|error| error["error"]["type"] == "FreeTierError");
        if free_error && let Some(change) = zen::prepare_free_body(&mut request, endpoint) {
            trace.diff.request.push(change);
            collapse = !client_streaming;
            upstream = gateway
                .client
                .post(url)
                .headers(headers)
                .body(serde_json::to_vec(&request).unwrap())
                .send()
                .await
                .map_err(|error| {
                    ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string())
                })?;
        } else {
            return Ok((StatusCode::FORBIDDEN, original_headers, bytes).into_response());
        }
    }
    let status = upstream.status();
    trace.status = status.as_u16();
    let mut headers = upstream.headers().clone();
    strip_hop_headers(&mut headers);
    let is_sse = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next().unwrap_or("").trim() == "text/event-stream");
    if collapse && status.is_success() && is_sse {
        let bytes = upstream
            .bytes()
            .await
            .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
        let value = zen::collapse(&bytes, endpoint)?;
        headers.remove(header::CONTENT_LENGTH);
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        trace.result = "完成";
        return Ok((status, headers, serde_json::to_vec(&value).unwrap()).into_response());
    }
    if !status.is_success() || !anthropic {
        let successful = status.is_success();
        if successful {
            trace.result = "取消";
        }
        let stream = async_stream::stream! {
            let mut trace = trace;
            let mut stream = upstream.bytes_stream();
            while let Some(chunk) = stream.next().await {
                if chunk.is_err() {
                    trace.result = "失败";
                    yield chunk;
                    return;
                }
                yield chunk;
            }
            if successful {
                trace.result = "完成";
            }
        };
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        return Ok(response);
    }
    headers.remove(header::CONTENT_LENGTH);
    let body = if is_sse {
        trace.result = "取消";
        // Buffer one event only. Byte-level framing keeps split UTF-8 intact.
        let stream = async_stream::stream! {
            let mut trace = trace;
            let mut upstream = upstream.bytes_stream();
            let mut frame = Vec::new();
            let mut line_length = 0;
            let mut frame_number = 0;
            while let Some(chunk) = upstream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        trace.result = "失败";
                        yield Err::<Bytes, io::Error>(io::Error::other(error));
                        return;
                    }
                };
                for byte in chunk {
                    let blank = byte == b'\n'
                        && (line_length == 0
                            || (line_length == 1 && frame.last() == Some(&b'\r')));
                    frame.push(byte);
                    if byte == b'\n' {
                        line_length = 0;
                    } else {
                        line_length += 1;
                    }
                    if blank {
                        frame_number += 1;
                        let (output, changes) = normalize_sse(&frame, frame_number);
                        trace.repairs += changes.len();
                        trace.diff.response.extend(changes);
                        yield Ok(Bytes::from(output));
                        frame.clear();
                    }
                }
            }
            if !frame.is_empty() {
                let (output, changes) = normalize_sse(&frame, frame_number + 1);
                trace.repairs += changes.len();
                trace.diff.response.extend(changes);
                yield Ok(Bytes::from(output));
            }
            trace.result = "完成";
        };
        Body::from_stream(stream)
    } else {
        let bytes = upstream
            .bytes()
            .await
            .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
        let mut value: Value = serde_json::from_slice(&bytes)
            .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.to_string()))?;
        trace.diff.response = normalize_response(&mut value);
        trace.repairs = trace.diff.response.len();
        trace.result = "完成";
        if trace.repairs > 0 {
            Body::from(serde_json::to_vec(&value).unwrap())
        } else {
            Body::from(bytes)
        }
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

fn configured_key(headers: &mut HeaderMap, key: &str) -> Result<(), ApiError> {
    if !key.is_empty() {
        headers.remove("x-api-key");
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {key}").parse().map_err(|_| {
                ApiError(
                    StatusCode::BAD_REQUEST,
                    "上游密钥不能包含非法请求头字符".into(),
                )
            })?,
        );
    }
    Ok(())
}

async fn models(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let [stepfun, opencode] = gateway.upstreams("models")?;
    let (stepfun, opencode) = futures_util::future::join(
        fetch_models(&gateway.client, stepfun, headers.clone()),
        fetch_models(&gateway.client, opencode, headers),
    )
    .await;
    let mut data = Vec::new();
    let mut errors = Vec::new();
    let mut successes = 0;
    for (prefix, result) in [("stepfun", stepfun), ("opencode", opencode)] {
        match result {
            Ok(models) => {
                successes += 1;
                for mut model in models {
                    if let Some(id) = model["id"].as_str().filter(|id| !id.is_empty()) {
                        model["id"] = format!("{prefix}/{id}").into();
                        data.push(model);
                    }
                }
            }
            Err(error) => errors.push(format!("{prefix}: {}", error.1)),
        }
    }
    if successes == 0 {
        return Err(ApiError(StatusCode::BAD_GATEWAY, errors.join("；")));
    }
    Ok(Json(
        json!({"object": "list", "data": data, "upstream_errors": errors}),
    ))
}

async fn fetch_models(
    client: &reqwest::Client,
    (url, key): (reqwest::Url, String),
    mut headers: HeaderMap,
) -> Result<Vec<Value>, ApiError> {
    strip_hop_headers(&mut headers);
    configured_key(&mut headers, &key)?;
    if zen::is_zen(&url) {
        zen::prepare_headers(&mut headers, &Value::Null, false);
    }
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::ACCEPT_ENCODING, "identity".parse().unwrap());
    let response = client
        .get(url)
        .headers(headers)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
    if !response.status().is_success() {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            format!("模型列表返回 HTTP {}", response.status()),
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
    let mut payload: Value = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "模型列表不是有效 JSON".into()))?;
    payload
        .get_mut("data")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .ok_or_else(|| ApiError(StatusCode::BAD_GATEWAY, "模型列表缺少 data 数组".into()))
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let named: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(|name| name.trim().to_owned()))
        .collect();
    for name in named {
        headers.remove(name.as_str());
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

fn unsigned_thinking(block: &Value) -> bool {
    block["type"] == "thinking"
        && block["signature"]
            .as_str()
            .is_none_or(|value| value.trim().is_empty())
}

fn normalize_history(request: &mut Value) -> Vec<Change> {
    let mut changes = Vec::new();
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return changes;
    };
    let mut source_message = 0;
    let mut target_message = 0;
    messages.retain_mut(|message| {
        let path = format!("/messages/{source_message}");
        source_message += 1;
        if message["role"] != "assistant" {
            target_message += 1;
            return true;
        }
        let Some(blocks) = message["content"].as_array() else {
            target_message += 1;
            return true;
        };
        if blocks.iter().all(|block| {
            unsigned_thinking(block) && block["thinking"].as_str().unwrap_or("").trim().is_empty()
        }) {
            changes.push(Change::new(
                path,
                Some(message.clone()),
                None,
                "移除空助手消息",
            ));
            return false;
        }
        let target_path = format!("/messages/{target_message}");
        target_message += 1;
        let blocks = message["content"].as_array_mut().unwrap();
        let mut source_block = 0;
        let mut target_block = 0;
        // pi: unsigned thinking is plain text on replay; valid signatures remain opaque.
        blocks.retain_mut(|block| {
            let block_path = format!("{path}/content/{source_block}");
            source_block += 1;
            if !unsigned_thinking(block) {
                target_block += 1;
                return true;
            }
            let before = block.clone();
            let text = block["thinking"].as_str().unwrap_or("").to_owned();
            if text.trim().is_empty() {
                changes.push(Change::new(block_path, Some(before), None, "移除空思考块"));
                return false;
            }
            let object = block.as_object_mut().unwrap();
            object.insert("type".into(), json!("text"));
            object.insert("text".into(), json!(text));
            object.remove("thinking");
            object.remove("signature");
            let mut change = Change::new(
                block_path,
                Some(before),
                Some(block.clone()),
                "无签名思考内容转为文本",
            );
            change.after_path = format!("{target_path}/content/{target_block}");
            changes.push(change);
            target_block += 1;
            true
        });
        true
    });
    changes
}

fn normalize_response(value: &mut Value) -> Vec<Change> {
    let mut changes = Vec::new();
    let (blocks, prefix): (&mut [Value], &str) = match value["type"].as_str() {
        Some("content_block_start") => (
            value
                .get_mut("content_block")
                .map(std::slice::from_mut)
                .unwrap_or(&mut []),
            "/content_block",
        ),
        Some("message_start") => (
            value["message"]["content"]
                .as_array_mut()
                .map(Vec::as_mut_slice)
                .unwrap_or(&mut []),
            "/message/content",
        ),
        Some("message") => (
            value["content"]
                .as_array_mut()
                .map(Vec::as_mut_slice)
                .unwrap_or(&mut []),
            "/content",
        ),
        _ => return changes,
    };
    for (index, block) in blocks.iter_mut().enumerate() {
        if block["type"] != "thinking" {
            continue;
        }
        let mut changed = false;
        let before = block.clone();
        for field in ["thinking", "signature"] {
            if block.get(field).is_none_or(Value::is_null) {
                block[field] = json!("");
                changed = true;
            }
        }
        if changed {
            let path = if prefix == "/content_block" {
                prefix.into()
            } else {
                format!("{prefix}/{index}")
            };
            changes.push(Change::new(
                path,
                Some(before),
                Some(block.clone()),
                "补齐思考字段",
            ));
        }
    }
    changes
}

fn normalize_sse(frame: &[u8], frame_number: usize) -> (Vec<u8>, Vec<Change>) {
    let Ok(text) = std::str::from_utf8(frame) else {
        return (frame.to_vec(), Vec::new());
    };
    let data = text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .map(|value| value.strip_prefix(' ').unwrap_or(value))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let Ok(mut value) = serde_json::from_str::<Value>(&data) else {
        return (frame.to_vec(), Vec::new());
    };
    let mut changes = normalize_response(&mut value);
    if changes.is_empty() {
        return (frame.to_vec(), changes);
    }
    let mut event = format!(
        "SSE #{frame_number} · {}",
        value["type"].as_str().unwrap_or("")
    );
    if let Some(index) = value["index"].as_u64() {
        event.push_str(&format!(" · index {index}"));
    }
    for change in &mut changes {
        change.event = Some(event.clone());
    }
    let mut output = String::new();
    let mut replaced = false;
    for line in text.split_inclusive('\n') {
        if line.starts_with("data:") {
            if !replaced {
                output.push_str("data: ");
                output.push_str(&serde_json::to_string(&value).unwrap());
                if line.ends_with("\r\n") {
                    output.push_str("\r\n");
                } else if line.ends_with('\n') {
                    output.push('\n');
                }
                replaced = true;
            }
        } else {
            output.push_str(line);
        }
    }
    (output.into_bytes(), changes)
}
