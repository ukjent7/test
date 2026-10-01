use std::{
    hash::BuildHasher,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use axum::http::header;
use serde::Serialize;
use serde_json::{Value, json};

use super::Gateway;
use crate::{cache, history};

#[derive(Serialize)]
struct Call {
    id: i64,
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

pub struct Trace {
    gateway: Arc<Gateway>,
    id: i64,
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
                        "authorization"
                            | "x-api-key"
                            | "cookie"
                            | "set-cookie"
                            | "proxy-authorization"
                    ) {
                        format!(
                            "[已隐藏 · {:016x}]",
                            self.gateway.routing_hasher.hash_one(value.as_bytes())
                        )
                    } else {
                        std::str::from_utf8(value.as_bytes()).map_or_else(
                            |_| {
                                format!(
                                    "HEX {}",
                                    value
                                        .as_bytes()
                                        .iter()
                                        .map(|byte| format!("{byte:02x}"))
                                        .collect::<String>()
                                )
                            },
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
        self.exchange
            .attempts
            .last_mut()
            .unwrap()
            .response
            .as_mut()
            .unwrap()
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
