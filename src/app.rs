use std::{
    env, fs,
    hash::{BuildHasher, RandomState},
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ApiError, cache, history, messages, models};

#[derive(Clone, Deserialize, Serialize)]
pub struct Settings {
    pub upstream_base_url: String,
    #[serde(default = "opencode_base_url")]
    pub opencode_base_url: String,
    #[serde(default)]
    pub stepfun_api_key: String,
    #[serde(default)]
    pub opencode_api_key: String,
    pub enabled: bool,
}

fn opencode_base_url() -> String {
    "https://opencode.ai/zen/v1".into()
}

#[derive(Default, Serialize)]
pub struct Stats {
    requests: u64,
    active: u64,
    repairs: usize,
    errors: u64,
}

#[derive(Serialize)]
struct Call {
    id: u64,
    model: String,
    status: u16,
    result: &'static str,
    repairs: usize,
    duration_ms: u128,
    timestamp: u128,
    request_changes: usize,
    response_changes: usize,
    routing: Option<cache::Routing>,
    cache: cache::Usage,
    #[serde(skip_serializing)]
    diff: Diff,
}

#[derive(Default, Serialize)]
pub struct Diff {
    pub request: Vec<Change>,
    pub response: Vec<Change>,
}

#[derive(Serialize)]
pub struct Change {
    pub path: String,
    pub after_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
    pub reason: &'static str,
    pub event: Option<String>,
}

impl Change {
    pub fn new(
        path: String,
        mut before: Option<Value>,
        mut after: Option<Value>,
        reason: &'static str,
    ) -> Self {
        if let (Some(Value::Object(before)), Some(Value::Object(after))) = (&mut before, &mut after)
        {
            let unchanged: Vec<String> = before
                .iter()
                .filter(|(key, value)| after.get(*key) == Some(*value))
                .map(|(key, _)| key.clone())
                .collect();
            for key in unchanged {
                before.remove(&key);
                after.remove(&key);
            }
        }
        Self {
            after_path: path.clone(),
            path,
            before,
            after,
            reason,
            event: None,
        }
    }
}

pub struct Control {
    settings: Settings,
    pub messages_url: reqwest::Url,
    pub opencode_url: reqwest::Url,
    stats: Stats,
    history: history::History,
    next_id: u64,
    history_error: Option<String>,
}

pub struct Gateway {
    pub client: reqwest::Client,
    pub control: Mutex<Control>,
    settings_path: PathBuf,
    listen: SocketAddr,
    routing_hasher: RandomState,
}

pub struct Trace {
    gateway: Arc<Gateway>,
    id: u64,
    model: String,
    started: Instant,
    pub status: u16,
    pub result: &'static str,
    pub repairs: usize,
    pub diff: Diff,
    pub routing: Option<cache::Routing>,
    pub cache: cache::Usage,
    pub exchange: history::Exchange,
}

impl Trace {
    pub fn new(gateway: &Arc<Gateway>, model: String) -> Self {
        let mut control = gateway.control.lock().unwrap();
        control.stats.requests += 1;
        control.stats.active += 1;
        control.next_id += 1;
        Self {
            gateway: gateway.clone(),
            id: control.next_id,
            model,
            started: Instant::now(),
            status: 502,
            result: "失败",
            repairs: 0,
            diff: Diff::default(),
            routing: None,
            cache: cache::Usage::default(),
            exchange: history::Exchange::default(),
        }
    }

    pub fn record_routing(
        &mut self,
        source: &'static str,
        identity: &str,
        headers: &axum::http::HeaderMap,
    ) {
        let key = headers
            .get("x-api-key")
            .and_then(|key| key.to_str().ok())
            .or_else(|| {
                headers
                    .get(header::AUTHORIZATION)?
                    .to_str()
                    .ok()?
                    .strip_prefix("Bearer ")
            });
        // A random process-local seed prevents guessing identities from public fingerprints.
        let fingerprint = self.gateway.routing_hasher.hash_one((identity, key));
        self.routing = Some(cache::Routing {
            source,
            fingerprint: format!("{fingerprint:016x}"),
        });
    }

    pub fn snapshot(
        &self,
        target: String,
        status: Option<u16>,
        headers: &axum::http::HeaderMap,
        body: Vec<u8>,
        complete: bool,
    ) -> history::Snapshot {
        history::Snapshot {
            target,
            status,
            headers: headers
                .iter()
                .map(|(name, value)| {
                    let value = if matches!(
                        name.as_str(),
                        "authorization" | "x-api-key" | "cookie" | "set-cookie" | "proxy-authorization"
                    ) {
                        format!(
                            "[已隐藏 · {:016x}]",
                            self.gateway.routing_hasher.hash_one(value.as_bytes())
                        )
                    } else {
                        std::str::from_utf8(value.as_bytes()).map_or_else(
                            |_| format!("HEX {}", value.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
                            str::to_owned,
                        )
                    };
                    (name.to_string(), value)
                })
                .collect(),
            body,
            complete,
        }
    }

    pub fn upstream_response(&mut self) -> &mut history::Snapshot {
        self.exchange.attempts.last_mut().unwrap().response.as_mut().unwrap()
    }

    pub fn error(&mut self, error: String) {
        self.result = "失败";
        self.exchange.error = Some(error);
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        let mut control = self.gateway.control.lock().unwrap();
        control.stats.active -= 1;
        control.stats.repairs += self.repairs;
        control.stats.errors += u64::from(self.result == "失败");
        let call = Call {
            id: self.id,
            model: self.model.clone(),
            status: self.status,
            result: self.result,
            repairs: self.repairs,
            duration_ms: self.started.elapsed().as_millis(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            request_changes: self.diff.request.len(),
            response_changes: self.diff.response.len(),
            routing: self.routing.take(),
            cache: std::mem::take(&mut self.cache),
            diff: std::mem::take(&mut self.diff),
        };
        let summary = serde_json::to_value(&call).unwrap();
        let detail = json!({"call": summary, "diff": call.diff, "exchange": self.exchange});
        control.history_error = control
            .history
            .save(self.id, &summary, &detail)
            .err()
            .map(|error| format!("请求 #{} 未能落盘：{error}", self.id));
        if let Some(error) = &control.history_error {
            eprintln!("{error}");
        }
    }
}

impl Gateway {
    pub(crate) fn upstreams(
        &self,
        endpoint: &str,
    ) -> Result<[(reqwest::Url, String); 2], ApiError> {
        let control = self.control.lock().unwrap();
        if !control.settings.enabled {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "网关转发已停止".into(),
            ));
        }
        Ok([
            (&control.messages_url, &control.settings.stepfun_api_key),
            (&control.opencode_url, &control.settings.opencode_api_key),
        ]
        .map(|(url, key)| {
            let mut url = url.clone();
            url.set_path(&format!(
                "{}/{endpoint}",
                url.path().trim_end_matches("/messages")
            ));
            (url, key.clone())
        }))
    }

    fn save(&self, settings: &Settings) -> Result<(), ApiError> {
        if let Some(parent) = self.settings_path.parent() {
            fs::create_dir_all(parent).map_err(settings_error)?;
        }
        let temporary = self.settings_path.with_extension("tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(settings).unwrap())
            .map_err(settings_error)?;
        fs::rename(temporary, &self.settings_path).map_err(settings_error)
    }
}

fn settings_error(error: io::Error) -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("无法保存设置：{error}"),
    )
}

fn messages_url(base: &str) -> Result<reqwest::Url, ApiError> {
    let url =
        reqwest::Url::parse(&format!("{}/messages", base.trim_end_matches('/'))).map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "请输入完整的 HTTP(S) 上游基地址".into(),
            )
        })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "上游地址须使用 HTTP(S)，且不能包含查询参数或片段".into(),
        ));
    }
    Ok(url)
}

pub async fn prepare() -> Result<(tokio::net::TcpListener, Router), Box<dyn std::error::Error>> {
    let listen: SocketAddr = env::var("GATEWAY_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8789".into())
        .parse()?;
    let settings_path = env::var_os("GATEWAY_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("MessagesGateway/settings.json")
        });
    let mut settings = match fs::read(&settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Settings {
            upstream_base_url: "https://api.stepfun.ai/step_plan/v1".into(),
            opencode_base_url: opencode_base_url(),
            stepfun_api_key: String::new(),
            opencode_api_key: String::new(),
            enabled: true,
        },
        Err(error) => return Err(error.into()),
    };
    if let Ok(base) = env::var("GATEWAY_UPSTREAM_BASE_URL") {
        settings.upstream_base_url = base;
    }
    for (name, value) in [
        ("GATEWAY_OPENCODE_BASE_URL", &mut settings.opencode_base_url),
        ("GATEWAY_STEPFUN_API_KEY", &mut settings.stepfun_api_key),
        ("GATEWAY_OPENCODE_API_KEY", &mut settings.opencode_api_key),
    ] {
        if let Ok(overridden) = env::var(name) {
            *value = overridden;
        }
    }
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
    let gateway = Arc::new(Gateway {
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .build()?,
        control: Mutex::new(Control {
            settings,
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
        .route("/ui/calls/{id}", get(call_diff))
        .route("/ui/settings", post(update_settings))
        .route("/ui/enabled", post(set_enabled))
        .layer(middleware::from_fn(local_control));
    let app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/messages", post(messages))
        .route("/v1/chat/completions", post(messages))
        .route("/chat/completions", post(messages))
        .route("/v1/responses", post(messages))
        .route("/responses", post(messages))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/health", get(|| async { "ok" }))
        .route(
            "/",
            get(|| async { Html(include_str!("../ui/index.html")) }),
        )
        .route(
            "/app.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../ui/app.css"),
                )
            }),
        )
        .route(
            "/byte-diff.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../ui/byte-diff.js"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../ui/app.js"),
                )
            }),
        )
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
        "endpoint": format!("http://{}/v1", gateway.listen),
        "stats": control.stats, "calls": calls, "history_error": control.history_error,
    })))
}

async fn call_diff(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<u64>,
) -> Result<Json<Value>, ApiError> {
    let control = gateway.control.lock().unwrap();
    let detail = control
        .history
        .detail(id)
        .map_err(history_error)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "记录已过期，请选择最近的请求".into()))?;
    Ok(Json(detail))
}

fn history_error(error: rusqlite::Error) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("无法读取请求历史：{error}"))
}

#[derive(Deserialize)]
struct UpstreamInput {
    upstream_base_url: String,
    opencode_base_url: Option<String>,
    stepfun_api_key: Option<String>,
    opencode_api_key: Option<String>,
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
    gateway.save(&settings)?;
    control.settings = settings;
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
    gateway.save(&settings)?;
    control.settings = settings;
    Ok(StatusCode::NO_CONTENT)
}
