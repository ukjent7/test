use std::{
    collections::VecDeque,
    env, fs, io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{ApiError, messages};

#[derive(Deserialize, Serialize)]
pub struct Settings {
    pub upstream_base_url: String,
    pub enabled: bool,
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
}

pub struct Control {
    settings: Settings,
    pub messages_url: reqwest::Url,
    stats: Stats,
    calls: VecDeque<Call>,
}

pub struct Gateway {
    pub client: reqwest::Client,
    pub control: Mutex<Control>,
    settings_path: PathBuf,
    listen: SocketAddr,
}

pub struct Trace {
    gateway: Arc<Gateway>,
    id: u64,
    model: String,
    started: Instant,
    pub status: u16,
    pub result: &'static str,
    pub repairs: usize,
}

impl Trace {
    pub fn new(gateway: &Arc<Gateway>, model: String) -> Self {
        let mut control = gateway.control.lock().unwrap();
        control.stats.requests += 1;
        control.stats.active += 1;
        Self {
            gateway: gateway.clone(),
            id: control.stats.requests,
            model,
            started: Instant::now(),
            status: 502,
            result: "失败",
            repairs: 0,
        }
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        let mut control = self.gateway.control.lock().unwrap();
        control.stats.active -= 1;
        control.stats.repairs += self.repairs;
        control.stats.errors += u64::from(self.result == "失败");
        control.calls.push_front(Call {
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
        });
        control.calls.truncate(30);
    }
}

impl Gateway {
    pub fn forwarding_url(&self) -> Option<reqwest::Url> {
        let control = self.control.lock().unwrap();
        control
            .settings
            .enabled
            .then(|| control.messages_url.clone())
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
            enabled: true,
        },
        Err(error) => return Err(error.into()),
    };
    if let Ok(base) = env::var("GATEWAY_UPSTREAM_BASE_URL") {
        settings.upstream_base_url = base;
    }
    let messages_url = messages_url(&settings.upstream_base_url)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.1))?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let listen = listener.local_addr()?;
    let gateway = Arc::new(Gateway {
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .build()?,
        control: Mutex::new(Control {
            settings,
            messages_url,
            stats: Stats::default(),
            calls: VecDeque::new(),
        }),
        settings_path,
        listen,
    });
    let ui = Router::new()
        .route("/ui/status", get(status))
        .route("/ui/settings", post(update_settings))
        .route("/ui/enabled", post(set_enabled))
        .layer(middleware::from_fn(local_control));
    let app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/messages", post(messages))
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

async fn status(State(gateway): State<Arc<Gateway>>) -> Json<serde_json::Value> {
    let control = gateway.control.lock().unwrap();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"), "enabled": control.settings.enabled,
        "upstream_base_url": control.settings.upstream_base_url,
        "endpoint": format!("http://{}/v1", gateway.listen),
        "stats": control.stats, "calls": control.calls,
    }))
}

#[derive(Deserialize)]
struct UpstreamInput {
    upstream_base_url: String,
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
    let settings = Settings {
        upstream_base_url: base,
        enabled: control.settings.enabled,
    };
    gateway.save(&settings)?;
    control.settings = settings;
    control.messages_url = url;
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
    let settings = Settings {
        upstream_base_url: control.settings.upstream_base_url.clone(),
        enabled: input.enabled,
    };
    gateway.save(&settings)?;
    control.settings = settings;
    Ok(StatusCode::NO_CONTENT)
}
