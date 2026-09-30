#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod desktop;

use std::{env, io, net::SocketAddr, sync::Arc};

use axum::{
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde_json::{json, Value};

use app::{Gateway, Trace};

struct ApiError(StatusCode, String);

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
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
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
    let mut trace = Trace::new(&gateway, request["model"].as_str().unwrap_or("未知模型").to_owned());
    let Some(mut url) = gateway.forwarding_url() else {
        trace.status = 503;
        return Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "网关转发已停止".into()));
    };
    normalize_history(&mut request);
    strip_hop_headers(&mut headers);
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::ACCEPT_ENCODING, "identity".parse().unwrap());
    url.set_query(uri.query());
    let upstream = gateway
        .client
        .post(url)
        .headers(headers)
        .body(serde_json::to_vec(&request).unwrap())
        .send()
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
    let status = upstream.status();
    trace.status = status.as_u16();
    let mut headers = upstream.headers().clone();
    strip_hop_headers(&mut headers);
    if !status.is_success() {
        let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        return Ok(response);
    }
    headers.remove(header::CONTENT_LENGTH);
    let is_sse = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next().unwrap_or("").trim() == "text/event-stream");
    let body = if is_sse {
        trace.result = "取消";
        // Buffer one event only. Byte-level framing keeps split UTF-8 intact.
        let stream = async_stream::stream! {
            let mut trace = trace;
            let mut upstream = upstream.bytes_stream();
            let mut frame = Vec::new();
            let mut line_length = 0;
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
                        let (output, repairs) = normalize_sse(&frame);
                        trace.repairs += repairs;
                        yield Ok(Bytes::from(output));
                        frame.clear();
                    }
                }
            }
            if !frame.is_empty() {
                let (output, repairs) = normalize_sse(&frame);
                trace.repairs += repairs;
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
        trace.repairs = normalize_response(&mut value);
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

fn normalize_history(request: &mut Value) {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    messages.retain_mut(|message| {
        if message["role"] != "assistant" {
            return true;
        }
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            return true;
        };
        // pi: unsigned thinking is plain text on replay; valid signatures remain opaque.
        blocks.retain_mut(|block| {
            if block["type"] != "thinking"
                || block["signature"]
                    .as_str()
                    .is_some_and(|value| !value.trim().is_empty())
            {
                return true;
            }
            let text = block["thinking"].as_str().unwrap_or("").to_owned();
            if text.trim().is_empty() {
                return false;
            }
            let object = block.as_object_mut().unwrap();
            object.insert("type".into(), json!("text"));
            object.insert("text".into(), json!(text));
            object.remove("thinking");
            object.remove("signature");
            true
        });
        !blocks.is_empty()
    });
}

fn normalize_response(value: &mut Value) -> usize {
    let mut repairs = 0;
    let blocks: &mut [Value] = match value["type"].as_str() {
        Some("content_block_start") => value
            .get_mut("content_block")
            .map(std::slice::from_mut)
            .unwrap_or(&mut []),
        Some("message_start") => value["message"]["content"]
            .as_array_mut()
            .map(Vec::as_mut_slice)
            .unwrap_or(&mut []),
        Some("message") => value["content"]
            .as_array_mut()
            .map(Vec::as_mut_slice)
            .unwrap_or(&mut []),
        _ => return 0,
    };
    for block in blocks {
        if block["type"] != "thinking" {
            continue;
        }
        let mut changed = false;
        for field in ["thinking", "signature"] {
            if block.get(field).is_none_or(Value::is_null) {
                block[field] = json!("");
                changed = true;
            }
        }
        repairs += usize::from(changed);
    }
    repairs
}

fn normalize_sse(frame: &[u8]) -> (Vec<u8>, usize) {
    let Ok(text) = std::str::from_utf8(frame) else {
        return (frame.to_vec(), 0);
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
        return (frame.to_vec(), 0);
    };
    let repairs = normalize_response(&mut value);
    if repairs == 0 {
        return (frame.to_vec(), 0);
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
    (output.into_bytes(), repairs)
}
