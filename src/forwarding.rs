use std::{io, sync::Arc};

use axum::{
    Json,
    body::{Body, Bytes, to_bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::{
    app::{Change, Gateway, Trace, Upstream},
    cache,
    error::ApiError,
    history,
    protocol::{normalize_history, normalize_response, normalize_sse},
    zen,
};

pub(crate) async fn messages(
    State(gateway): State<Arc<Gateway>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request = serde_json::from_slice::<Value>(&body);
    let mut trace = Trace::new(
        &gateway,
        request
            .as_ref()
            .ok()
            .and_then(|value| value["model"].as_str())
            .unwrap_or("未知模型")
            .to_owned(),
    );
    trace.exchange.request = trace.snapshot(uri.to_string(), None, &headers, body.to_vec(), true);
    let mut trace = Some(trace);
    let response = match request {
        Ok(request) => forward(&gateway, uri, headers, request, &mut trace).await,
        Err(error) => Err(ApiError(StatusCode::BAD_REQUEST, error.to_string())),
    };
    let response = response.unwrap_or_else(|error| {
        let trace = trace.as_mut().unwrap();
        trace.status = error.0.as_u16();
        trace.error(error.1.clone());
        error.into_response()
    });
    if let Some(mut trace) = trace {
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.map_err(|error| {
            trace.error(error.to_string());
            ApiError(StatusCode::BAD_GATEWAY, error.to_string())
        })?;
        trace.exchange.response = Some(trace.snapshot(
            String::new(),
            Some(parts.status.as_u16()),
            &parts.headers,
            bytes.to_vec(),
            true,
        ));
        return Ok(Response::from_parts(parts, Body::from(bytes)));
    }
    Ok(response)
}

async fn forward(
    gateway: &Arc<Gateway>,
    uri: axum::http::Uri,
    mut headers: HeaderMap,
    mut request: Value,
    trace_slot: &mut Option<Trace>,
) -> Result<Response, ApiError> {
    let trace = trace_slot.as_mut().unwrap();
    let endpoint = uri
        .path()
        .trim_start_matches("/v1")
        .trim_start_matches('/')
        .to_owned();
    let [stepfun, opencode] = gateway.upstreams(&endpoint).inspect_err(|error| {
        trace.status = error.0.as_u16();
    })?;
    let model = request["model"].as_str().unwrap_or("").to_owned();
    let (upstream, wire_model) = if let Some(model) = model.strip_prefix("opencode/") {
        (opencode, model)
    } else {
        (stepfun, model.strip_prefix("stepfun/").unwrap_or(&model))
    };
    let Upstream {
        mut url,
        key,
        client,
    } = upstream;
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
        let (source, session) = zen::prepare_headers(&mut headers, &request, anthropic);
        trace.record_routing(source, &session, &headers);
    } else if let Some((source, identity)) = cache::identity(&headers, &request) {
        trace.record_routing(source, identity, &headers);
    }
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::ACCEPT_ENCODING, "identity".parse().unwrap());
    url.set_query(uri.query());
    let client_streaming = request["stream"].as_bool().unwrap_or(false);
    let mut collapse = false;
    let mut upstream = send_upstream(&client, &url, &headers, &request, trace).await?;
    if !anthropic && zen::is_zen(&url) && upstream.status() == StatusCode::FORBIDDEN {
        trace.status = 403;
        let mut original_headers = upstream.headers().clone();
        strip_hop_headers(&mut original_headers);
        let bytes = read_upstream(upstream, trace).await?;
        let free_error = serde_json::from_slice::<Value>(&bytes)
            .is_ok_and(|error| error["error"]["type"] == "FreeTierError");
        if free_error && let Some(change) = zen::prepare_free_body(&mut request, &endpoint) {
            trace.diff.request.push(change);
            collapse = !client_streaming;
            upstream = send_upstream(&client, &url, &headers, &request, trace).await?;
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
        let bytes = read_upstream(upstream, trace).await?;
        let value = zen::collapse(&bytes, &endpoint)?;
        trace.cache.observe(&endpoint, &value);
        headers.remove(header::CONTENT_LENGTH);
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        trace.result = "完成";
        return Ok((status, headers, serde_json::to_vec(&value).unwrap()).into_response());
    }
    if !status.is_success() {
        let mut trace = trace_slot.take().unwrap();
        trace.exchange.response = Some(trace.snapshot(
            String::new(),
            Some(status.as_u16()),
            &headers,
            Vec::new(),
            false,
        ));
        let stream = async_stream::stream! {
            let mut stream = upstream.bytes_stream();
            while let Some(chunk) = stream.next().await {
                match &chunk {
                    Ok(bytes) => {
                        trace.upstream_response().body.extend_from_slice(bytes);
                        trace.exchange.response.as_mut().unwrap().body.extend_from_slice(bytes);
                    }
                    Err(error) => {
                        trace.error(error.to_string());
                        yield chunk;
                        return;
                    }
                }
                yield chunk;
            }
            trace.upstream_response().complete = true;
            trace.exchange.response.as_mut().unwrap().complete = true;
        };
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        return Ok(response);
    }
    headers.remove(header::CONTENT_LENGTH);
    let body = if is_sse {
        let mut trace = trace_slot.take().unwrap();
        trace.result = "取消";
        trace.exchange.response = Some(trace.snapshot(
            String::new(),
            Some(status.as_u16()),
            &headers,
            Vec::new(),
            false,
        ));
        // Buffer one event only. Byte-level framing keeps split UTF-8 intact.
        let stream = async_stream::stream! {
            let mut upstream = upstream.bytes_stream();
            let mut frame = Vec::new();
            let mut line_length = 0;
            let mut frame_number = 0;
            while let Some(chunk) = upstream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        trace.error(error.to_string());
                        yield Err::<Bytes, io::Error>(io::Error::other(error));
                        return;
                    }
                };
                trace.upstream_response().body.extend_from_slice(&chunk);
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
                        let (output, changes) = normalize_sse(&frame, frame_number, &endpoint, &mut trace.cache);
                        trace.repairs += changes.len();
                        trace.diff.response.extend(changes);
                        trace.exchange.response.as_mut().unwrap().body.extend_from_slice(&output);
                        yield Ok(Bytes::from(output));
                        frame.clear();
                    }
                }
            }
            if !frame.is_empty() {
                let (output, changes) = normalize_sse(&frame, frame_number + 1, &endpoint, &mut trace.cache);
                trace.repairs += changes.len();
                trace.diff.response.extend(changes);
                trace.exchange.response.as_mut().unwrap().body.extend_from_slice(&output);
                yield Ok(Bytes::from(output));
            }
            trace.result = "完成";
            trace.upstream_response().complete = true;
            trace.exchange.response.as_mut().unwrap().complete = true;
        };
        Body::from_stream(stream)
    } else {
        let mut bytes = read_upstream(upstream, trace).await?;
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(mut value) => {
                trace.cache.observe(&endpoint, &value);
                if anthropic {
                    trace.diff.response = normalize_response(&mut value);
                    trace.repairs = trace.diff.response.len();
                    if trace.repairs > 0 {
                        bytes = Bytes::from(serde_json::to_vec(&value).unwrap());
                    }
                }
            }
            Err(error) if anthropic => {
                return Err(ApiError(StatusCode::BAD_GATEWAY, error.to_string()));
            }
            Err(_) => {}
        }
        trace.result = "完成";
        Body::from(bytes)
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

async fn send_upstream(
    client: &reqwest::Client,
    url: &reqwest::Url,
    headers: &HeaderMap,
    body: &Value,
    trace: &mut Trace,
) -> Result<reqwest::Response, ApiError> {
    let bytes = serde_json::to_vec(body).unwrap();
    let mut request = client
        .post(url.clone())
        .headers(headers.clone())
        .body(bytes.clone())
        .build()
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
    // Make protocol defaults explicit so the recorded headers match the actual request.
    let host = url.port().map_or_else(
        || url.host_str().unwrap().to_owned(),
        |port| format!("{}:{port}", url.host_str().unwrap()),
    );
    request
        .headers_mut()
        .insert(header::HOST, host.parse().unwrap());
    request.headers_mut().insert(
        header::CONTENT_LENGTH,
        bytes.len().to_string().parse().unwrap(),
    );
    request
        .headers_mut()
        .entry(header::ACCEPT)
        .or_insert("*/*".parse().unwrap());
    trace.exchange.attempts.push(history::Attempt {
        request: trace.snapshot(url.to_string(), None, request.headers(), bytes, true),
        response: None,
    });
    let response = client
        .execute(request)
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
    trace.exchange.attempts.last_mut().unwrap().response = Some(trace.snapshot(
        String::new(),
        Some(response.status().as_u16()),
        response.headers(),
        Vec::new(),
        false,
    ));
    Ok(response)
}

async fn read_upstream(upstream: reqwest::Response, trace: &mut Trace) -> Result<Bytes, ApiError> {
    let mut stream = upstream.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk
            .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.without_url().to_string()))?;
        trace.upstream_response().body.extend_from_slice(&bytes);
    }
    let response = trace.upstream_response();
    response.complete = true;
    Ok(Bytes::copy_from_slice(&response.body))
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

pub(crate) async fn models(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let [stepfun, opencode] = gateway.upstreams("models")?;
    let (stepfun, opencode) = futures_util::future::join(
        fetch_models(stepfun, headers.clone()),
        fetch_models(opencode, headers),
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
    Upstream { url, key, client }: Upstream,
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
