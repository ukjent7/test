mod trace;
mod ui;

use std::{
    env,
    hash::RandomState,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderMap, StatusCode, header},
    response::Html,
    routing::{get, post},
};
use serde::Serialize;

pub(crate) use trace::{Change, Trace};

use crate::{
    error::ApiError,
    forwarding, history,
    settings::{Settings, anonymous_base_url, messages_url, network_clients, program_directory},
};

#[derive(Default, Serialize)]
pub(super) struct Stats {
    pub(super) requests: u64,
    pub(super) active: u64,
    pub(super) repairs: usize,
    pub(super) errors: u64,
}

pub(super) struct Control {
    pub(super) settings: Settings,
    pub(super) clients: [reqwest::Client; 2],
    pub(super) messages_url: reqwest::Url,
    pub(super) opencode_url: reqwest::Url,
    pub(super) opencode_anonymous_url: reqwest::Url,
    pub(super) lunaroute_url: reqwest::Url,
    pub(super) stats: Stats,
    pub(super) history: history::History,
    pub(super) next_id: i64,
    pub(super) history_error: Option<String>,
}

pub struct Gateway {
    pub(super) control: Mutex<Control>,
    pub(super) settings_path: PathBuf,
    pub(super) listen: SocketAddr,
    pub(super) routing_hasher: RandomState,
}

pub(crate) struct Upstream {
    pub url: reqwest::Url,
    pub key: String,
    pub client: reqwest::Client,
}

impl Gateway {
    /// 完全无凭证（网关设置与客户端都没带 key）时走 Zen 匿名车道。匿名流量自 2026-10-14 起
    /// 只被 /inference 受理，keyed 流量仍走配置的 /zen 基地址。
    pub(crate) fn opencode_anonymous(&self, headers: &HeaderMap) -> bool {
        let control = self.control.lock().unwrap();
        control.settings.opencode_api_key.is_empty()
            && !headers.contains_key("x-api-key")
            && !headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("Bearer "))
    }

    pub(crate) fn upstreams(
        &self,
        endpoint: &str,
        anonymous: bool,
    ) -> Result<[Upstream; 3], ApiError> {
        let control = self.control.lock().unwrap();
        if !control.settings.enabled {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "网关转发已停止".into(),
            ));
        }
        let opencode_url = if anonymous {
            &control.opencode_anonymous_url
        } else {
            &control.opencode_url
        };
        Ok([
            (
                &control.messages_url,
                &control.settings.stepfun_api_key,
                control.settings.stepfun_use_proxy,
            ),
            (
                opencode_url,
                &control.settings.opencode_api_key,
                control.settings.opencode_use_proxy,
            ),
            (
                &control.lunaroute_url,
                &control.settings.lunaroute_api_key,
                control.settings.lunaroute_use_proxy,
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
    let opencode_anonymous_url = messages_url(&anonymous_base_url(&settings.opencode_base_url))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.1))?;
    let lunaroute_url = messages_url(&settings.lunaroute_base_url)
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
            opencode_anonymous_url,
            lunaroute_url,
            stats: Stats::default(),
            history,
            next_id,
            history_error: None,
        }),
        settings_path,
        listen,
        routing_hasher: RandomState::new(),
    });
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
        .merge(ui::assets())
        .merge(ui::router())
        .layer(DefaultBodyLimit::disable())
        .with_state(gateway);
    Ok((listener, app))
}
