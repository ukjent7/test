use std::{net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Gateway;
use crate::{
    error::ApiError,
    settings::{anonymous_base_url, messages_url, network_clients},
};

pub(super) fn router() -> Router<Arc<Gateway>> {
    Router::new()
        .route("/ui/status", get(status))
        .route("/ui/usage", get(usage))
        .route("/ui/calls/{id}", get(call_diff))
        .route("/ui/settings", post(update_settings))
        .route("/ui/enabled", post(set_enabled))
        .layer(middleware::from_fn(local_control))
}

pub(super) fn assets() -> Router<Arc<Gateway>> {
    let mut assets = Router::new();
    for (path, source) in [
        ("/app.css", include_str!("../../ui/app.css")),
        ("/app.js", include_str!("../../ui/app.js")),
        ("/core.js", include_str!("../../ui/core.js")),
        ("/gateway.js", include_str!("../../ui/gateway.js")),
        ("/settings.js", include_str!("../../ui/settings.js")),
        ("/activity.js", include_str!("../../ui/activity.js")),
        ("/details.js", include_str!("../../ui/details.js")),
        ("/usage.js", include_str!("../../ui/usage.js")),
        ("/byte-diff.js", include_str!("../../ui/byte-diff.js")),
    ] {
        let content_type = if path.ends_with(".css") {
            "text/css; charset=utf-8"
        } else {
            "text/javascript; charset=utf-8"
        };
        assets = assets.route(
            path,
            get(move || async move { ([(header::CONTENT_TYPE, content_type)], source) }),
        );
    }
    assets
}

async fn local_control(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let local_host = reqwest::Url::parse(&format!("http://{host}"))
        .ok()
        .is_some_and(|url| matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")));
    let same_origin = request
        .headers()
        .get(header::ORIGIN)
        .is_none_or(|origin| origin.as_bytes() == format!("http://{host}").as_bytes());
    if !peer.ip().is_loopback() || !local_host || !same_origin {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn status(State(gateway): State<Arc<Gateway>>) -> Result<Json<Value>, ApiError> {
    let control = gateway.control.lock().unwrap();
    let calls = control.history.list().map_err(history_error)?;
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"), "enabled": control.settings.enabled,
        "upstream_base_url": control.settings.upstream_base_url,
        "opencode_base_url": control.settings.opencode_base_url,
        "lunaroute_base_url": control.settings.lunaroute_base_url,
        "stepfun_key_configured": !control.settings.stepfun_api_key.is_empty(),
        "opencode_key_configured": !control.settings.opencode_api_key.is_empty(),
        "lunaroute_key_configured": !control.settings.lunaroute_api_key.is_empty(),
        "proxy": control.settings.proxy,
        "stepfun_use_proxy": control.settings.stepfun_use_proxy,
        "opencode_use_proxy": control.settings.opencode_use_proxy,
        "lunaroute_use_proxy": control.settings.lunaroute_use_proxy,
        "endpoint": format!("http://{}/v1", gateway.listen),
        "stats": control.stats, "calls": calls, "history_error": control.history_error,
    })))
}

async fn call_diff(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let control = gateway.control.lock().unwrap();
    let detail = control
        .history
        .detail(id)
        .map_err(history_error)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "记录已过期，请选择最近的请求".into()))?;
    Ok(Json(detail))
}

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default)]
    since: i64,
}

async fn usage(
    State(gateway): State<Arc<Gateway>>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Value>, ApiError> {
    let control = gateway.control.lock().unwrap();
    Ok(Json(
        control.history.usage(query.since).map_err(history_error)?,
    ))
}

fn history_error(error: rusqlite::Error) -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("无法读取请求历史：{error}"),
    )
}

#[derive(Deserialize)]
struct UpstreamInput {
    upstream_base_url: String,
    opencode_base_url: Option<String>,
    lunaroute_base_url: Option<String>,
    stepfun_api_key: Option<String>,
    opencode_api_key: Option<String>,
    lunaroute_api_key: Option<String>,
    proxy: Option<String>,
    stepfun_use_proxy: Option<bool>,
    opencode_use_proxy: Option<bool>,
    lunaroute_use_proxy: Option<bool>,
}

async fn update_settings(
    State(gateway): State<Arc<Gateway>>,
    Json(input): Json<UpstreamInput>,
) -> Result<StatusCode, ApiError> {
    let base = input
        .upstream_base_url
        .trim()
        .trim_end_matches('/')
        .to_owned();
    let url = messages_url(&base)?;
    let mut control = gateway.control.lock().unwrap();
    let mut settings = control.settings.clone();
    settings.upstream_base_url = base;
    if let Some(base) = input.opencode_base_url {
        settings.opencode_base_url = base.trim().trim_end_matches('/').to_owned();
    }
    let opencode_url = messages_url(&settings.opencode_base_url)?;
    let opencode_anonymous_url = messages_url(&anonymous_base_url(&settings.opencode_base_url))?;
    if let Some(base) = input.lunaroute_base_url {
        settings.lunaroute_base_url = base.trim().trim_end_matches('/').to_owned();
    }
    let lunaroute_url = messages_url(&settings.lunaroute_base_url)?;
    if let Some(key) = input.stepfun_api_key {
        settings.stepfun_api_key = key.trim().to_owned();
    }
    if let Some(key) = input.opencode_api_key {
        settings.opencode_api_key = key.trim().to_owned();
    }
    if let Some(key) = input.lunaroute_api_key {
        settings.lunaroute_api_key = key.trim().to_owned();
    }
    if let Some(proxy) = input.proxy {
        settings.proxy = proxy.trim().to_owned();
    }
    if let Some(enabled) = input.stepfun_use_proxy {
        settings.stepfun_use_proxy = enabled;
    }
    if let Some(enabled) = input.opencode_use_proxy {
        settings.opencode_use_proxy = enabled;
    }
    if let Some(enabled) = input.lunaroute_use_proxy {
        settings.lunaroute_use_proxy = enabled;
    }
    let clients = network_clients(&settings.proxy)?;
    settings.save(&gateway.settings_path)?;
    control.settings = settings;
    control.clients = clients;
    control.messages_url = url;
    control.opencode_url = opencode_url;
    control.opencode_anonymous_url = opencode_anonymous_url;
    control.lunaroute_url = lunaroute_url;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct EnabledInput {
    enabled: bool,
}

async fn set_enabled(
    State(gateway): State<Arc<Gateway>>,
    Json(input): Json<EnabledInput>,
) -> Result<StatusCode, ApiError> {
    let mut control = gateway.control.lock().unwrap();
    let mut settings = control.settings.clone();
    settings.enabled = input.enabled;
    settings.save(&gateway.settings_path)?;
    control.settings = settings;
    Ok(StatusCode::NO_CONTENT)
}
