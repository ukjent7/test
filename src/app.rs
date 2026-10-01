mod trace;

use std::{
    env,
    hash::RandomState,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) use trace::{Change, Trace};

use crate::{
    error::ApiError,
    forwarding, history,
    settings::{Settings, messages_url, network_clients, program_directory},
};

#[derive(Default, Serialize)]
struct Stats {
    requests: u64,
    active: u64,
    repairs: usize,
    errors: u64,
}

struct Control {
    settings: Settings,
    clients: [reqwest::Client; 2],
    messages_url: reqwest::Url,
    opencode_url: reqwest::Url,
    stats: Stats,
    history: history::History,
    next_id: i64,
    history_error: Option<String>,
}

pub struct Gateway {
    control: Mutex<Control>,
    settings_path: PathBuf,
    listen: SocketAddr,
    routing_hasher: RandomState,
}

pub(crate) struct Upstream {
    pub url: reqwest::Url,
    pub key: String,
    pub client: reqwest::Client,
}

impl Gateway {
    pub(crate) fn upstreams(&self, endpoint: &str) -> Result<[Upstream; 2], ApiError> {
        let control = self.control.lock().unwrap();
        if !control.settings.enabled {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "网关转发已停止".into(),
            ));
        }
        Ok([
            (
                &control.messages_url,
                &control.settings.stepfun_api_key,
                control.settings.stepfun_use_proxy,
            ),
            (
                &control.opencode_url,
                &control.settings.opencode_api_key,
                control.settings.opencode_use_proxy,
            ),
        ]
        .map(|(url, key, use_proxy)| {
            let mut url = url.clone();
            url.set_path(&format!(
                "{}/{endpoint}",
                url.path().trim_end_matches("/messages")
            ));
            let loopback = url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host.ends_with(".localhost")
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            let client = &control.clients[usize::from(!use_proxy || loopback)];
            Upstream {
                url,
                key: key.clone(),
                client: client.clone(),
            }
        }))
    }
}

pub async fn prepare() -> Result<(tokio::net::TcpListener, Router), Box<dyn std::error::Error>> {
    let listen: SocketAddr = env::var("GATEWAY_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8789".into())
        .parse()?;
    let directory = program_directory()?;
    let settings_path = env::var_os("GATEWAY_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| directory.join("settings.json"));
    let settings = Settings::load(&settings_path)?;
    let opencode_url = messages_url(&settings.opencode_base_url)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.1))?;
    let messages_url = messages_url(&settings.upstream_base_url)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.1))?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let listen = listener.local_addr()?;
    let history_path = env::var_os("GATEWAY_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| settings_path.with_file_name("requests.sqlite3"));
    let history = history::History::open(&history_path)?;
    let next_id = history.last_id()?;
    let clients = network_clients(&settings.proxy).map_err(|error| io::Error::other(error.1))?;
    let gateway = Arc::new(Gateway {
        control: Mutex::new(Control {
            settings,
            clients,
            messages_url,
            opencode_url,
            stats: Stats::default(),
            history,
            next_id,
            history_error: None,
        }),
        settings_path,
        listen,
        routing_hasher: RandomState::new(),
    });
    let ui = Router::new()
        .route("/ui/status", get(status))
        .route("/ui/usage", get(usage))
        .route("/ui/calls/{id}", get(call_diff))
        .route("/ui/settings", post(update_settings))
        .route("/ui/enabled", post(set_enabled))
        .layer(middleware::from_fn(local_control));
    let mut assets = Router::new();
    for (path, source) in [
        ("/app.css", include_str!("../ui/app.css")),
        ("/app.js", include_str!("../ui/app.js")),
        ("/core.js", include_str!("../ui/core.js")),
        ("/gateway.js", include_str!("../ui/gateway.js")),
        ("/settings.js", include_str!("../ui/settings.js")),
        ("/activity.js", include_str!("../ui/activity.js")),
        ("/details.js", include_str!("../ui/details.js")),
        ("/usage.js", include_str!("../ui/usage.js")),
        ("/byte-diff.js", include_str!("../ui/byte-diff.js")),
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
    let app = Router::new()
        .route("/v1/messages", post(forwarding::messages))
        .route("/messages", post(forwarding::messages))
        .route("/v1/chat/completions", post(forwarding::messages))
        .route("/chat/completions", post(forwarding::messages))
        .route("/v1/responses", post(forwarding::messages))
        .route("/responses", post(forwarding::messages))
        .route("/v1/models", get(forwarding::models))
        .route("/models", get(forwarding::models))
        .route("/health", get(|| async { "ok" }))
        .route(
            "/",
            get(|| async { Html(include_str!("../ui/index.html")) }),
        )
        .merge(assets)
        .merge(ui)
        .layer(DefaultBodyLimit::disable())
        .with_state(gateway);
    Ok((listener, app))
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
        "stepfun_key_configured": !control.settings.stepfun_api_key.is_empty(),
        "opencode_key_configured": !control.settings.opencode_api_key.is_empty(),
        "proxy": control.settings.proxy,
        "stepfun_use_proxy": control.settings.stepfun_use_proxy,
        "opencode_use_proxy": control.settings.opencode_use_proxy,
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
    stepfun_api_key: Option<String>,
    opencode_api_key: Option<String>,
    proxy: Option<String>,
    stepfun_use_proxy: Option<bool>,
    opencode_use_proxy: Option<bool>,
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
    if let Some(key) = input.stepfun_api_key {
        settings.stepfun_api_key = key.trim().to_owned();
    }
    if let Some(key) = input.opencode_api_key {
        settings.opencode_api_key = key.trim().to_owned();
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
    let clients = network_clients(&settings.proxy)?;
    settings.save(&gateway.settings_path)?;
    control.settings = settings;
    control.clients = clients;
    control.messages_url = url;
    control.opencode_url = opencode_url;
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
