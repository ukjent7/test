use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;

#[derive(Clone, Deserialize, Serialize)]
pub struct Settings {
    pub upstream_base_url: String,
    #[serde(default = "opencode_base_url")]
    pub opencode_base_url: String,
    #[serde(default)]
    pub stepfun_api_key: String,
    #[serde(default)]
    pub opencode_api_key: String,
    #[serde(default)]
    pub proxy: String,
    #[serde(default = "use_proxy")]
    pub stepfun_use_proxy: bool,
    #[serde(default = "use_proxy")]
    pub opencode_use_proxy: bool,
    pub enabled: bool,
}

fn use_proxy() -> bool {
    true
}

fn opencode_base_url() -> String {
    "https://opencode.ai/zen/v1".into()
}

impl Settings {
    pub(crate) fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut settings = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Settings {
                upstream_base_url: "https://api.stepfun.ai/step_plan/v1".into(),
                opencode_base_url: opencode_base_url(),
                proxy: String::new(),
                stepfun_use_proxy: true,
                opencode_use_proxy: true,
                stepfun_api_key: String::new(),
                opencode_api_key: String::new(),
                enabled: true,
            },
            Err(error) => return Err(error.into()),
        };
        for (name, value) in [
            ("GATEWAY_UPSTREAM_BASE_URL", &mut settings.upstream_base_url),
            ("GATEWAY_OPENCODE_BASE_URL", &mut settings.opencode_base_url),
            ("GATEWAY_STEPFUN_API_KEY", &mut settings.stepfun_api_key),
            ("GATEWAY_OPENCODE_API_KEY", &mut settings.opencode_api_key),
        ] {
            if let Ok(overridden) = env::var(name) {
                *value = overridden;
            }
        }
        Ok(settings)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<(), ApiError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(settings_error)?;
        }
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(self).unwrap()).map_err(settings_error)?;
        fs::rename(temporary, path).map_err(settings_error)
    }
}

fn settings_error(error: io::Error) -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("无法保存设置：{error}"),
    )
}

pub(crate) fn network_clients(proxy: &str) -> Result<[reqwest::Client; 2], ApiError> {
    let invalid = || {
        ApiError(
            StatusCode::BAD_REQUEST,
            "代理地址须使用 HTTP(S)、SOCKS5 或 SOCKS5H，例如 http://127.0.0.1:7890".into(),
        )
    };
    let builder = || {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
    };
    let mut proxied = builder();
    if proxy == "direct" {
        proxied = proxied.no_proxy();
    } else if !proxy.is_empty() {
        let address = if proxy.contains("://") {
            proxy.to_owned()
        } else {
            format!("http://{proxy}")
        };
        let url = reqwest::Url::parse(&address).map_err(|_| invalid())?;
        if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
            || url.host_str().is_none()
            || (!proxy.contains("://") && !proxy.contains(':'))
        {
            return Err(invalid());
        }
        proxied = proxied.proxy(reqwest::Proxy::all(url).map_err(|_| invalid())?);
    }
    let build = |builder: reqwest::ClientBuilder| {
        builder.build().map_err(|error| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("无法应用网络代理设置：{}", error.without_url()),
            )
        })
    };
    Ok([build(proxied)?, build(builder().no_proxy())?])
}

pub(crate) fn messages_url(base: &str) -> Result<reqwest::Url, ApiError> {
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

pub(crate) fn program_directory() -> io::Result<PathBuf> {
    let mut path = env::current_exe()?;
    path.pop();
    Ok(path)
}
