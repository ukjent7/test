use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{DefaultHasher, Hash, Hasher},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::http::{HeaderMap, StatusCode, header};
use serde_json::{Value, json};

use crate::{app::Change, cache, error::ApiError};

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Zen 会拒绝过旧的 OpenCode 版本（426），跟随 OpenCode 的当前发布。
const OPENCODE_VERSION: &str = "1.18.34";
/// Zen 免费层只服务 OpenCode 自身：请求须流式，且提供名为 bash 与 read 的
/// 工具（小写，仅按名匹配）。客户端已有的大小写变体（如 Claude Code 的
/// Bash）改名后提供，回复里的调用再改回客户端原名；缺的补一个永远不该
/// 被调用的桩，模型调用桩的内容会被丢弃。
const ZEN_TOOLS: [&str; 2] = ["bash", "read"];
const STUB_DESCRIPTION: &str = "Not available in this session. Never call this tool.";
const INVALID_STREAM: &str = "上游免费模型的流式响应不完整或无效";

pub fn is_zen(url: &reqwest::Url) -> bool {
    url.host_str() == Some("opencode.ai")
        && matches!(
            url.path(),
            "/zen/v1/messages"
                | "/zen/v1/chat/completions"
                | "/zen/v1/responses"
                | "/zen/v1/models"
        )
}

/// Zen 的免费模型：-free 后缀，或已知不带后缀的免费 id。
pub fn is_free_model(model: &str) -> bool {
    model.ends_with("-free") || matches!(model, "big-pickle" | "grok-code")
}

pub fn prepare_headers(
    headers: &mut HeaderMap,
    request: &Value,
    anthropic: bool,
) -> (&'static str, String) {
    let (source, session) = session_id(headers, request);
    let key = headers.get("x-api-key").cloned().or_else(|| {
        headers
            .get(header::AUTHORIZATION)?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?
            .parse()
            .ok()
    });
    let mut clean = HeaderMap::new();
    if anthropic {
        for name in ["anthropic-version", "anthropic-beta"] {
            if let Some(value) = headers.get(name) {
                clean.insert(name, value.clone());
            }
        }
        clean
            .entry("anthropic-version")
            .or_insert("2023-06-01".parse().unwrap());
    }
    if let Some(key) = key {
        if anthropic {
            clean.insert("x-api-key", key);
        } else {
            clean.insert(
                header::AUTHORIZATION,
                format!("Bearer {}", key.to_str().unwrap()).parse().unwrap(),
            );
        }
    }
    clean.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    clean.insert(
        header::ACCEPT,
        "application/json, text/event-stream".parse().unwrap(),
    );
    clean.insert(
        header::USER_AGENT,
        format!("opencode/{OPENCODE_VERSION}").parse().unwrap(),
    );
    clean.insert("x-opencode-session", session.parse().unwrap());
    // OpenCode 还带每轮一个 msg_ id 与客户端、项目标签，照它发送。
    clean.insert("x-opencode-request", turn_id().parse().unwrap());
    clean.insert("x-opencode-client", "cli".parse().unwrap());
    clean.insert("x-opencode-project", "global".parse().unwrap());
    *headers = clean;
    (source, session)
}

/// Zen 免费层的工具整形结果：改名映射与补出来的桩。
#[derive(Default)]
pub struct ZenTools {
    back: BTreeMap<String, String>,   // 提供给 Zen 的名字 → 客户端原名
    stubs: BTreeSet<String>,          // 客户端没有、只为 Zen 补的名字
    schemas: BTreeMap<String, Value>, // 客户端原名 → 参数 schema
}

impl ZenTools {
    fn client_name(&self, zen_name: &str) -> Option<&str> {
        self.back.get(zen_name).map(String::as_str)
    }

    fn is_stub(&self, name: &str) -> bool {
        self.stubs.contains(name)
    }

    /// 非流式 JSON 回复：丢弃桩调用、回改工具名，没有真实调用时把工具
    /// 结束改判为普通结束。
    pub fn fix_response(&self, value: &mut Value, endpoint: &str) -> Vec<Change> {
        let mut changes = Vec::new();
        match endpoint {
            "messages" => {
                if let Some(blocks) = value["content"].as_array_mut() {
                    let mut kept = Vec::new();
                    for (position, mut block) in blocks.drain(..).enumerate() {
                        if block["type"] == "tool_use" {
                            let name = block["name"].as_str().unwrap_or("").to_owned();
                            if self.is_stub(&name) {
                                changes.push(Change::new(
                                    format!("/content/{position}"),
                                    Some(block),
                                    None,
                                    "丢弃 Zen 免费层桩工具调用",
                                ));
                                continue;
                            }
                            if let Some(client) = self.client_name(&name) {
                                let before = block.clone();
                                block["name"] = client.into();
                                changes.push(Change::new(
                                    format!("/content/{position}"),
                                    Some(before),
                                    Some(block.clone()),
                                    "Zen 免费层工具名回改",
                                ));
                            }
                        }
                        kept.push(block);
                    }
                    *blocks = kept;
                }
                let real_calls = value["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"));
                if value["stop_reason"] == "tool_use" && !self.stubs.is_empty() && !real_calls {
                    changes.push(Change::new(
                        "/stop_reason".into(),
                        Some(json!("tool_use")),
                        Some(json!("end_turn")),
                        "无真实工具调用，改判普通结束",
                    ));
                    value["stop_reason"] = "end_turn".into();
                }
            }
            "chat/completions" => {
                let Some(choices) = value["choices"].as_array_mut() else {
                    return changes;
                };
                for (position, choice) in choices.iter_mut().enumerate() {
                    let Some(message) = choice.get_mut("message") else {
                        continue;
                    };
                    let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut)
                    else {
                        continue;
                    };
                    let mut kept = Vec::new();
                    for (call_position, mut call) in calls.drain(..).enumerate() {
                        let name = call["function"]["name"].as_str().unwrap_or("").to_owned();
                        if self.is_stub(&name) {
                            changes.push(Change::new(
                                format!("/choices/{position}/message/tool_calls/{call_position}"),
                                Some(call),
                                None,
                                "丢弃 Zen 免费层桩工具调用",
                            ));
                            continue;
                        }
                        if let Some(client) = self.client_name(&name) {
                            let before = call.clone();
                            call["function"]["name"] = client.into();
                            changes.push(Change::new(
                                format!("/choices/{position}/message/tool_calls/{call_position}"),
                                Some(before),
                                Some(call.clone()),
                                "Zen 免费层工具名回改",
                            ));
                        }
                        kept.push(call);
                    }
                    let emptied = kept.is_empty();
                    *calls = kept;
                    if emptied && let Some(message) = message.as_object_mut() {
                        message.remove("tool_calls");
                    }
                    if choice["finish_reason"] == "tool_calls" && !self.stubs.is_empty() && emptied
                    {
                        changes.push(Change::new(
                            format!("/choices/{position}/finish_reason"),
                            Some(json!("tool_calls")),
                            Some(json!("stop")),
                            "无真实工具调用，改判普通结束",
                        ));
                        choice["finish_reason"] = "stop".into();
                    }
                }
            }
            "responses" => {
                if let Some(output) = value["output"].as_array_mut() {
                    fix_output(output, &mut changes, self);
                }
            }
            _ => {}
        }
        changes
    }
}

type MarkupCall = (String, Vec<(String, String)>);

impl StreamFix {
    /// 流式回复的逐帧修复：返回是否保留该帧。
    pub fn frame(&mut self, value: &mut Value, endpoint: &str) -> (bool, Vec<Change>) {
        match endpoint {
            "messages" => self.messages_frame(value),
            "chat/completions" => self.chat_frame(value),
            "responses" => self.responses_frame(value),
            _ => (true, Vec::new()),
        }
    }

    /// 上游参数要同时满足 JSON 完整与工具必填字段才下发：原生流经常
    /// 缺字段或串台，仅 JSON 合法不足以让客户端接受。
    fn args_valid(schemas: &BTreeMap<String, Value>, name: &str, text: &str) -> bool {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return false;
        };
        let Some(schema) = schemas.get(name) else {
            return true;
        };
        let Some(object) = value.as_object() else {
            return false;
        };
        schema["required"].as_array().is_none_or(|required| {
            required
                .iter()
                .all(|field| object.contains_key(field.as_str().unwrap_or("")))
        })
    }

    /// 扫描并剔除 content 里的 <tool_call> 文本标记：标记段完整时解析出调用，
    /// 正文只保留标记外的普通文本，避免标记泄露给客户端污染历史。
    fn scan_markup(&mut self, chunk: &str) -> (String, Vec<MarkupCall>) {
        let mut calls = Vec::new();
        let mut clean = String::new();
        let mut text = chunk;
        loop {
            if let Some(mut buffer) = self.markup.take() {
                buffer.push_str(text);
                if let Some(end) = buffer.find("</tool_call>") {
                    if let Some(call) = parse_markup_call(&buffer[..end]) {
                        calls.push(call);
                    }
                    let after = buffer[end + "</tool_call>".len()..].to_owned();
                    text = "";
                    if let Some(start) = after.find("<tool_call>") {
                        clean.push_str(&after[..start]);
                        self.markup = Some(after[start..].to_owned());
                        continue;
                    } else {
                        clean.push_str(&after);
                        break;
                    }
                } else {
                    self.markup = Some(buffer);
                    break;
                }
            } else if let Some(start) = text.find("<tool_call>") {
                clean.push_str(&text[..start]);
                self.markup = Some(text[start..].to_owned());
                text = "";
                continue;
            } else {
                clean.push_str(text);
                break;
            }
        }
        (clean, calls)
    }

    fn messages_frame(&mut self, value: &mut Value) -> (bool, Vec<Change>) {
        match value["type"].as_str() {
            Some("content_block_start") => {
                let index = value["index"].as_u64().unwrap_or(0);
                let mut changes = Vec::new();
                let mut keep = true;
                if value["content_block"]["type"] == "tool_use" {
                    let name = value["content_block"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    if self.tools.is_stub(&name) {
                        changes.push(Change::new(
                            format!("/content_block/{index}"),
                            Some(value["content_block"].clone()),
                            None,
                            "丢弃 Zen 免费层桩工具调用",
                        ));
                        self.stub_blocks.insert(index);
                        self.dropped_blocks += 1;
                        keep = false;
                    } else {
                        if let Some(client) = self.tools.client_name(&name) {
                            let before = value["content_block"].clone();
                            value["content_block"]["name"] = client.into();
                            changes.push(Change::new(
                                format!("/content_block/{index}"),
                                Some(before),
                                Some(value["content_block"].clone()),
                                "Zen 免费层工具名回改",
                            ));
                        }
                        self.calls_made = true;
                    }
                }
                if keep {
                    self.shift(value, "index", self.dropped_blocks);
                }
                (keep, changes)
            }
            Some("content_block_delta" | "content_block_stop") => {
                let index = value["index"].as_u64().unwrap_or(0);
                if self.stub_blocks.contains(&index) {
                    return (false, Vec::new());
                }
                self.shift(value, "index", self.dropped_blocks);
                (true, Vec::new())
            }
            Some("message_delta") => {
                if value["delta"]["stop_reason"] == "tool_use"
                    && !self.tools.stubs.is_empty()
                    && !self.calls_made
                {
                    value["delta"]["stop_reason"] = "end_turn".into();
                    return (
                        true,
                        vec![Change::new(
                            "/delta/stop_reason".into(),
                            Some(json!("tool_use")),
                            Some(json!("end_turn")),
                            "无真实工具调用，改判普通结束",
                        )],
                    );
                }
                (true, Vec::new())
            }
            _ => (true, Vec::new()),
        }
    }

    fn chat_frame(&mut self, value: &mut Value) -> (bool, Vec<Change>) {
        let mut changes = Vec::new();
        let with_usage = value.get("usage").is_some();
        let Some(choices) = value["choices"].as_array_mut() else {
            return (true, changes);
        };
        for choice in choices.iter_mut() {
            let choice_index = choice["index"].as_u64().unwrap_or(0);
            let Some(delta) = choice.get_mut("delta") else {
                continue;
            };
            rename_reasoning(delta, choice_index, &mut changes);
            // 模型常把完整调用以 <tool_call> 文本混在 content 里，而上游
            // 转出的原生参数流残缺、串台。文本标记才是可靠来源：解析出
            // 完整调用后按名字补到未完成的序号上，否则追加新序号；正文中的
            // 标记一并剔除，避免污染会话历史。
            let content_opt = delta
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let mut markup_calls = Vec::new();
            if let Some(content) = content_opt {
                let (clean_content, calls) = self.scan_markup(&content);
                markup_calls = calls;
                if clean_content != content {
                    let path = format!("/choices/{choice_index}/delta/content");
                    let before = delta["content"].clone();
                    if clean_content.is_empty() {
                        if let Some(obj) = delta.as_object_mut() {
                            obj.remove("content");
                        }
                        changes.push(Change::new(
                            path,
                            Some(before),
                            None,
                            "从正文剔除文本工具标记",
                        ));
                    } else {
                        delta["content"] = json!(clean_content);
                        changes.push(Change::new(
                            path,
                            Some(before),
                            Some(json!(clean_content)),
                            "从正文剔除文本工具标记",
                        ));
                    }
                }
            }
            let dropped = self.dropped_calls.entry(choice_index).or_default();
            let mut kept = Vec::new();
            if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for mut call in calls.drain(..) {
                    let call_index = call["index"].as_u64().unwrap_or(0);
                    let name = call["function"]["name"].as_str().unwrap_or("").to_owned();
                    if self.tools.is_stub(&name) {
                        self.stub_calls.insert((choice_index, call_index));
                        *dropped += 1;
                        changes.push(Change::new(
                            format!("/choices/{choice_index}/delta/tool_calls/{call_index}"),
                            Some(call),
                            None,
                            "丢弃 Zen 免费层桩工具调用",
                        ));
                        continue;
                    }
                    if self.stub_calls.contains(&(choice_index, call_index)) {
                        continue;
                    }
                    // 序号上的工具名统一记客户端原名，与 schema 和文本
                    // 标记里的名字一致。
                    let client = self.tools.client_name(&name).unwrap_or(&name).to_owned();
                    if !client.is_empty() {
                        if client != name {
                            let before = call.clone();
                            call["function"]["name"] = client.clone().into();
                            changes.push(Change::new(
                                format!("/choices/{choice_index}/delta/tool_calls/{call_index}"),
                                Some(before),
                                Some(call.clone()),
                                "Zen 免费层工具名回改",
                            ));
                        }
                        self.calls_made = true;
                    }
                    // 免费模型会在同一序号上重试参数：每轮重试以空参数帧
                    // 开头，中途放弃。参数累积到 JSON 完整且满足工具必填
                    // 字段才下发，重试标记出现时丢弃尚未下发的残缺片段，
                    // 客户端只看到最后一次有效尝试。
                    let path = format!("/choices/{choice_index}/delta/tool_calls/{call_index}");
                    let fragment = call["function"]["arguments"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    let entry = self
                        .call_args
                        .entry((choice_index, call_index))
                        .or_default();
                    if !client.is_empty() {
                        entry.name = client;
                    }
                    if entry.flushed {
                        if !fragment.is_empty() {
                            changes.push(Change::new(path, None, None, "丢弃完整参数后的多余片段"));
                            continue;
                        }
                    } else if fragment.is_empty() {
                        if !entry.text.is_empty() {
                            changes.push(Change::new(path, None, None, "丢弃重试前残缺的工具参数"));
                            entry.text.clear();
                        }
                    } else {
                        entry.text.push_str(&fragment);
                        if Self::args_valid(&self.tools.schemas, &entry.name, &entry.text) {
                            entry.flushed = true;
                            call["function"]["arguments"] = json!(entry.text.clone());
                        } else {
                            call["function"]["arguments"] = json!("");
                        }
                    }
                    if *dropped > 0 {
                        call["index"] = json!(call_index - *dropped);
                    }
                    let max = self.max_index.entry(choice_index).or_insert(0);
                    *max = (*max).max(call_index.saturating_sub(*dropped));
                    kept.push(call);
                }
                *calls = kept;
                if calls.is_empty()
                    && let Some(delta) = delta.as_object_mut()
                {
                    delta.remove("tool_calls");
                }
            }
            for (name, params) in markup_calls {
                let mut object = serde_json::Map::new();
                for (key, raw) in &params {
                    let typed = match self.tools.schemas.get(&name) {
                        Some(schema) => typed_value(schema, key, raw),
                        None => json!(raw),
                    };
                    object.insert(key.clone(), typed);
                }
                let args = serde_json::to_string(&object).unwrap();
                let matched = self
                    .call_args
                    .iter_mut()
                    .find(|((choice, _), entry)| {
                        *choice == choice_index && !entry.flushed && entry.name == name
                    })
                    .map(|(key, entry)| {
                        entry.flushed = true;
                        key.1.saturating_sub(*dropped)
                    });
                let entry = match matched {
                    Some(index) => json!({"index": index,
                        "function": {"arguments": args}}),
                    None => {
                        let index = self
                            .max_index
                            .get(&choice_index)
                            .copied()
                            .map_or(0, |max| max + 1);
                        self.max_index.insert(choice_index, index);
                        self.synthetic += 1;
                        let mut hasher = DefaultHasher::new();
                        (choice_index, index, self.synthetic).hash(&mut hasher);
                        self.call_args.insert(
                            (choice_index, index + *dropped),
                            CallArgs {
                                name: name.clone(),
                                text: String::new(),
                                flushed: true,
                            },
                        );
                        json!({"index": index, "id": format!("call_{:016x}", hasher.finish()),
                            "type": "function",
                            "function": {"name": name, "arguments": args}})
                    }
                };
                if let Some(array) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) {
                    array.push(entry);
                } else {
                    delta["tool_calls"] = json!([entry]);
                }
                changes.push(Change::new(
                    format!("/choices/{choice_index}/delta/tool_calls"),
                    None,
                    None,
                    "从文本工具标记重建工具调用",
                ));
                self.calls_made = true;
            }
        }
        for choice in choices.iter_mut() {
            if choice["finish_reason"] == "tool_calls"
                && !self.tools.stubs.is_empty()
                && !self.calls_made
            {
                choice["finish_reason"] = "stop".into();
                changes.push(Change::new(
                    "/finish_reason".into(),
                    Some(json!("tool_calls")),
                    Some(json!("stop")),
                    "无真实工具调用，改判普通结束",
                ));
            }
        }
        // 流结束时仍有未完整的参数：补发最后一次尝试的片段，让客户端
        // 看到明确的残缺参数而不是空参数。
        for choice in choices.iter_mut() {
            if choice["finish_reason"].is_null() {
                continue;
            }
            let choice_index = choice["index"].as_u64().unwrap_or(0);
            let dropped = self.dropped_calls.get(&choice_index).copied().unwrap_or(0);
            let mut extra = Vec::new();
            for ((call_choice, call_index), entry) in self.call_args.iter_mut() {
                if *call_choice != choice_index || entry.flushed || entry.text.is_empty() {
                    continue;
                }
                entry.flushed = true;
                extra.push(json!({"index": *call_index - dropped,
                    "function": {"arguments": entry.text.clone()}}));
                changes.push(Change::new(
                    format!("/choices/{choice_index}/delta/tool_calls"),
                    None,
                    None,
                    "流结束时补发未完成的工具参数",
                ));
            }
            if let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut)
                && !extra.is_empty()
            {
                match delta.get_mut("tool_calls") {
                    Some(Value::Array(items)) => items.extend(extra),
                    _ => {
                        delta.insert("tool_calls".into(), Value::Array(extra));
                    }
                }
            }
        }
        // 桩调用被剔除后可能整个帧只剩空壳；usage 单独成帧时不能丢。
        let emptied = !choices.is_empty()
            && !with_usage
            && choices
                .iter()
                .all(|choice| choice["delta"] == json!({}) && choice["finish_reason"].is_null());
        (!emptied, changes)
    }

    fn responses_frame(&mut self, value: &mut Value) -> (bool, Vec<Change>) {
        let mut changes = Vec::new();
        match value["type"].as_str() {
            Some("response.output_item.added" | "response.output_item.done") => {
                let output_index = value["output_index"].as_u64().unwrap_or(0);
                if value["item"]["type"] == "function_call" {
                    let name = value["item"]["name"].as_str().unwrap_or("").to_owned();
                    let id = value["item"]["id"].as_str().unwrap_or("").to_owned();
                    if self.tools.is_stub(&name) {
                        self.stub_ids.insert(id);
                        self.dropped_items += 1;
                        changes.push(Change::new(
                            format!("/output_item/{output_index}"),
                            Some(value["item"].clone()),
                            None,
                            "丢弃 Zen 免费层桩工具调用",
                        ));
                        return (false, changes);
                    }
                    if let Some(client) = self.tools.client_name(&name) {
                        let before = value["item"].clone();
                        value["item"]["name"] = client.into();
                        changes.push(Change::new(
                            format!("/output_item/{output_index}"),
                            Some(before),
                            Some(value["item"].clone()),
                            "Zen 免费层工具名回改",
                        ));
                    }
                    self.calls_made = true;
                }
                self.shift(value, "output_index", self.dropped_items);
                (true, changes)
            }
            Some(name) if name.starts_with("response.function_call_arguments.") => {
                let id = value["item_id"].as_str().unwrap_or("").to_owned();
                if self.stub_ids.contains(&id) {
                    return (false, Vec::new());
                }
                self.shift(value, "output_index", self.dropped_items);
                (true, changes)
            }
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                if let Some(output) = value["response"]["output"].as_array_mut() {
                    fix_output(output, &mut changes, &self.tools);
                }
                (true, changes)
            }
            _ => {
                self.shift(value, "output_index", self.dropped_items);
                (true, changes)
            }
        }
    }

    fn shift(&self, value: &mut Value, field: &str, dropped: u64) {
        if dropped > 0
            && let Some(index) = value.get(field).and_then(Value::as_u64)
        {
            value[field] = json!(index - dropped);
        }
    }
}

/// 解析单个 <tool_call> 标记段（不含收尾标签）：<function=名字> 后跟
/// 一串 <parameter=键>值</parameter>；结构不符时放弃，content 原样保留。
fn parse_markup_call(region: &str) -> Option<MarkupCall> {
    let rest = region.strip_prefix("<tool_call>")?.trim_start();
    let rest = rest.strip_prefix("<function=")?;
    let end = rest.find('>')?;
    let name = rest[..end].trim().to_owned();
    let mut rest = &rest[end + 1..];
    let mut params = Vec::new();
    while let Some(item) = rest.strip_prefix("<parameter=") {
        let Some(end) = item.find('>') else {
            break;
        };
        let key = item[..end].trim().to_owned();
        let after = &item[end + 1..];
        let Some(value_end) = after.find("</parameter>") else {
            break;
        };
        params.push((key, after[..value_end].to_owned()));
        rest = after[value_end + "</parameter>".len()..].trim_start();
    }
    if name.is_empty() {
        return None;
    }
    Some((name, params))
}

fn prop_has_type(prop: &Value, expected: &str) -> bool {
    if prop["type"].as_str() == Some(expected) {
        return true;
    }
    if let Some(types) = prop["type"].as_array() {
        return types.iter().any(|item| item.as_str() == Some(expected));
    }
    if let Some(union) = prop["anyOf"]
        .as_array()
        .or_else(|| prop["oneOf"].as_array())
    {
        return union.iter().any(|item| prop_has_type(item, expected));
    }
    false
}

/// 按工具 schema 把文本参数值转成声明类型，避免客户端把布尔或数字
/// 当字符串拒绝反序列化。支持联合与可空声明（如 `["integer", "null"]`）。
fn typed_value(schema: &Value, key: &str, raw: &str) -> Value {
    let prop = &schema["properties"][key];
    let trimmed = raw.trim();
    if prop_has_type(prop, "boolean") {
        match trimmed {
            "true" => json!(true),
            "false" => json!(false),
            _ => json!(raw),
        }
    } else if prop_has_type(prop, "integer") {
        trimmed
            .parse::<i64>()
            .map(|value| json!(value))
            .unwrap_or_else(|_| json!(raw))
    } else if prop_has_type(prop, "number") {
        trimmed
            .parse::<f64>()
            .map(|value| json!(value))
            .unwrap_or_else(|_| json!(raw))
    } else if prop_has_type(prop, "array") || prop_has_type(prop, "object") {
        serde_json::from_str(raw).unwrap_or_else(|_| json!(raw))
    } else {
        json!(raw)
    }
}

/// Zen 免费层用 `reasoning` 承载思维链，客户端只认 `reasoning_content`：
/// 搬到标准字段名并删掉原名；空串或标准字段已有值时不动。
/// 改动只记路径不搬正文，避免把思维链复制进差异视图。
fn rename_reasoning(delta: &mut Value, choice_index: u64, changes: &mut Vec<Change>) {
    let Some(text) = delta.get("reasoning").and_then(Value::as_str) else {
        return;
    };
    let taken = matches!(delta.get("reasoning_content"), Some(value) if !value.is_null());
    if text.is_empty() || taken {
        return;
    }
    delta["reasoning_content"] = json!(text);
    if let Some(object) = delta.as_object_mut() {
        object.remove("reasoning");
    }
    changes.push(Change::new(
        format!("/choices/{choice_index}/delta/reasoning"),
        None,
        None,
        "思维链字段回改为 reasoning_content",
    ));
}

fn fix_output(output: &mut Vec<Value>, changes: &mut Vec<Change>, tools: &ZenTools) {
    let mut kept = Vec::new();
    for (position, mut item) in output.drain(..).enumerate() {
        if item["type"] == "function_call" {
            let name = item["name"].as_str().unwrap_or("").to_owned();
            if tools.is_stub(&name) {
                changes.push(Change::new(
                    format!("/output/{position}"),
                    Some(item),
                    None,
                    "丢弃 Zen 免费层桩工具调用",
                ));
                continue;
            }
            if let Some(client) = tools.client_name(&name) {
                let before = item.clone();
                item["name"] = client.into();
                changes.push(Change::new(
                    format!("/output/{position}"),
                    Some(before),
                    Some(item.clone()),
                    "Zen 免费层工具名回改",
                ));
            }
        }
        kept.push(item);
    }
    *output = kept;
}

fn tool_name(tool: &Value, chat: bool) -> &str {
    if chat {
        tool["function"]["name"].as_str().unwrap_or("")
    } else {
        tool["name"].as_str().unwrap_or("")
    }
}

fn set_tool_name(tool: &mut Value, chat: bool, name: &str) {
    if chat {
        tool["function"]["name"] = name.into();
    } else {
        tool["name"] = name.into();
    }
}

/// 把会话历史里客户端原名的工具调用一并改掉，模型看到的全程一致。
fn rename_history(request: &mut Value, endpoint: &str, from: &str, to: &str) {
    let rename = |name: &mut Value| {
        if name.as_str() == Some(from) {
            *name = json!(to);
        }
    };
    let conversation = if endpoint == "responses" {
        request["input"]
            .as_array_mut()
            .map(|items| items.iter_mut())
    } else {
        request["messages"]
            .as_array_mut()
            .map(|items| items.iter_mut())
    };
    for message in conversation.into_iter().flatten() {
        match endpoint {
            "chat/completions" => {
                if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                    for call in calls {
                        if let Some(function) = call.get_mut("function") {
                            rename(&mut function["name"]);
                        }
                    }
                }
            }
            "responses" => {
                if message["type"] == "function_call"
                    && let Some(name) = message.get_mut("name")
                {
                    rename(name);
                }
            }
            _ => {
                if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                    for block in blocks {
                        if block["type"] == "tool_use"
                            && let Some(name) = block.get_mut("name")
                        {
                            rename(name);
                        }
                    }
                }
            }
        }
    }
}

/// 客户端强制指定了被改名的工具时，tool_choice 里的名字一并映射。
fn map_tool_choice(request: &mut Value, endpoint: &str, tools: &ZenTools) {
    if tools.back.is_empty() {
        return;
    }
    let present = match endpoint {
        "chat/completions" => request["tool_choice"]["function"]["name"].is_string(),
        _ => request["tool_choice"]["name"].is_string(),
    };
    if !present {
        return;
    }
    let slot = match endpoint {
        "chat/completions" => &mut request["tool_choice"]["function"]["name"],
        _ => &mut request["tool_choice"]["name"],
    };
    let name = slot.as_str().unwrap_or("").to_owned();
    for (zen, client) in &tools.back {
        if client == &name {
            *slot = json!(zen);
            break;
        }
    }
}

/// 免费模型在发送前就按 Zen 免费层的要求整形：流式、bash/read 工具、
/// 历史与 tool_choice 的名字映射。改动以字段为单位记录，避免把用户正文
/// 带进差异视图。
pub fn prepare_free_body(request: &mut Value, endpoint: &str) -> (Vec<Change>, ZenTools) {
    let mut tools = ZenTools::default();
    if request.get("tools").is_some_and(|tools| !tools.is_array()) {
        return (Vec::new(), tools);
    }
    let chat = endpoint == "chat/completions";
    // 记录每个客户端工具的参数 schema：流式修复用它校验上游参数的
    // 完整性，并把文本标记里的参数值转成声明类型。
    if let Some(tools_array) = request.get("tools").and_then(Value::as_array) {
        for tool in tools_array {
            let name = tool_name(tool, chat).to_owned();
            let schema = if chat {
                &tool["function"]["parameters"]
            } else if endpoint == "responses" {
                &tool["parameters"]
            } else {
                &tool["input_schema"]
            };
            if schema.is_object() {
                tools.schemas.insert(name, schema.clone());
            }
        }
    }
    let mut changes = Vec::new();
    if request.get("stream").and_then(Value::as_bool) != Some(true) {
        changes.push(Change::new(
            "/stream".into(),
            request.get("stream").cloned(),
            Some(json!(true)),
            "Zen 免费层要求流式与基础工具",
        ));
        request["stream"] = true.into();
    }
    let original_tools = request.get("tools").cloned();
    if request.get("tools").is_none() {
        request["tools"] = json!([]);
    }
    let mut renames = Vec::new();
    if let Some(tools_array) = request["tools"].as_array_mut() {
        for need in ZEN_TOOLS {
            let found = tools_array
                .iter()
                .position(|tool| tool_name(tool, chat) == need)
                .or_else(|| {
                    tools_array
                        .iter()
                        .position(|tool| tool_name(tool, chat).eq_ignore_ascii_case(need))
                });
            if let Some(index) = found {
                let client = tool_name(&tools_array[index], chat).to_owned();
                if client != need {
                    tools.back.insert(need.to_owned(), client.clone());
                    set_tool_name(&mut tools_array[index], chat, need);
                    renames.push((client, need.to_owned()));
                }
                continue;
            }
            let stub = match endpoint {
                "chat/completions" => json!({"type": "function", "function": {
                    "name": need, "description": STUB_DESCRIPTION,
                    "parameters": {"type": "object", "properties": {}}}}),
                "responses" => json!({"type": "function", "name": need,
                    "description": STUB_DESCRIPTION,
                    "parameters": {"type": "object", "properties": {}}}),
                _ => json!({"name": need, "description": STUB_DESCRIPTION,
                    "input_schema": {"type": "object", "properties": {}}}),
            };
            tools_array.push(stub);
            tools.stubs.insert(need.to_owned());
        }
    }
    for (client, need) in &renames {
        rename_history(request, endpoint, client, need);
    }
    let after_tools = request["tools"].clone();
    if original_tools.as_ref() != Some(&after_tools) {
        changes.push(Change::new(
            "/tools".into(),
            original_tools,
            Some(after_tools),
            "Zen 免费层要求流式与基础工具",
        ));
    }
    map_tool_choice(request, endpoint, &tools);
    (changes, tools)
}

/// 同一序号工具参数的缓冲：累积片段、是否已把参数发给客户端、序号上的
/// 工具名。
#[derive(Default)]
struct CallArgs {
    name: String,
    text: String,
    flushed: bool,
}

/// 免费层流式的逐帧修复状态：桩块的索引、桩调用的序号、真实调用计数、
/// 未下发的工具参数、content 里的文本标记缓冲。
pub struct StreamFix {
    tools: ZenTools,
    stub_blocks: BTreeSet<u64>,
    dropped_blocks: u64,
    stub_calls: BTreeSet<(u64, u64)>,
    dropped_calls: BTreeMap<u64, u64>,
    calls_made: bool,
    stub_ids: BTreeSet<String>,
    dropped_items: u64,
    call_args: BTreeMap<(u64, u64), CallArgs>,
    markup: Option<String>,
    max_index: BTreeMap<u64, u64>,
    synthetic: u64,
}

impl StreamFix {
    pub fn new(tools: ZenTools) -> Self {
        Self {
            tools,
            stub_blocks: BTreeSet::new(),
            dropped_blocks: 0,
            stub_calls: BTreeSet::new(),
            dropped_calls: BTreeMap::new(),
            calls_made: false,
            stub_ids: BTreeSet::new(),
            dropped_items: 0,
            call_args: BTreeMap::new(),
            markup: None,
            max_index: BTreeMap::new(),
            synthetic: 0,
        }
    }
}

/// 把免费模型的流式回复聚合成客户端要的单条 chat.completion / response。
pub fn collapse(bytes: &[u8], endpoint: &str) -> Result<Value, ApiError> {
    let (events, done) = parse_events(bytes)?;
    if endpoint == "responses" {
        return events
            .into_iter()
            .rev()
            .find(|event| {
                matches!(
                    event["type"].as_str(),
                    Some("response.completed" | "response.incomplete" | "response.failed")
                )
            })
            .and_then(|mut event| {
                event
                    .get_mut("response")
                    .filter(|response| response.is_object())
                    .map(Value::take)
            })
            .ok_or_else(|| ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()));
    }
    if !done {
        return Err(ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()));
    }
    let mut response = json!({"object": "chat.completion"});
    let mut choices = BTreeMap::new();
    for event in events {
        let Some(fields) = event.as_object() else {
            return Err(ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()));
        };
        for (key, value) in fields {
            if !matches!(key.as_str(), "object" | "choices") && !value.is_null() {
                response[key] = value.clone();
            }
        }
        for choice in event["choices"].as_array().into_iter().flatten() {
            let index = choice["index"].as_u64().unwrap_or(0);
            let output = choices.entry(index).or_insert_with(|| {
                json!({"index": index,
                "message": {"role": "assistant", "content": ""}, "finish_reason": null})
            });
            merge_delta(&mut output["message"], &choice["delta"]);
            if !choice["finish_reason"].is_null() {
                output["finish_reason"] = choice["finish_reason"].clone();
            }
            if !choice["logprobs"].is_null() {
                merge_delta(&mut output["logprobs"], &choice["logprobs"]);
            }
        }
    }
    if choices.is_empty() {
        return Err(ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()));
    }
    response["choices"] = Value::Array(choices.into_values().collect());
    Ok(response)
}

/// 把 Anthropic 形态的流式回复聚合成客户端要的单条 message JSON。
pub fn collapse_messages(bytes: &[u8]) -> Result<Value, ApiError> {
    let (events, _) = parse_events(bytes)?;
    let mut message = json!({"type": "message", "role": "assistant", "content": [],
        "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": null, "output_tokens": null}});
    for event in events {
        match event["type"].as_str() {
            Some("message_start") => {
                for field in ["id", "model", "role"] {
                    if !event["message"][field].is_null() {
                        message[field] = event["message"][field].clone();
                    }
                }
                merge_usage(&mut message["usage"], &event["message"]["usage"]);
            }
            Some("content_block_start") => {
                let index = event["index"].as_u64().unwrap_or(
                    message["content"]
                        .as_array()
                        .map_or(0, |blocks| blocks.len() as u64),
                ) as usize;
                let Some(blocks) = message["content"].as_array_mut() else {
                    continue;
                };
                while blocks.len() <= index {
                    blocks.push(Value::Null);
                }
                blocks[index] = event["content_block"].clone();
            }
            Some("content_block_delta") => {
                let Some(blocks) = message["content"].as_array_mut() else {
                    continue;
                };
                let Some(block) =
                    blocks.get_mut(event["index"].as_u64().unwrap_or(u64::MAX) as usize)
                else {
                    continue;
                };
                match event["delta"]["type"].as_str() {
                    Some("text_delta") => append_str(&mut block["text"], &event["delta"]["text"]),
                    Some("thinking_delta") => {
                        append_str(&mut block["thinking"], &event["delta"]["thinking"]);
                    }
                    Some("signature_delta") => {
                        append_str(&mut block["signature"], &event["delta"]["signature"]);
                    }
                    Some("input_json_delta") => {
                        append_str(&mut block["partial_json"], &event["delta"]["partial_json"]);
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let Some(blocks) = message["content"].as_array_mut() else {
                    continue;
                };
                let Some(block) =
                    blocks.get_mut(event["index"].as_u64().unwrap_or(u64::MAX) as usize)
                else {
                    continue;
                };
                if let Some(partial) = block["partial_json"].as_str() {
                    let input = if partial.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(partial)
                            .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into()))?
                    };
                    block["input"] = input;
                    if let Some(object) = block.as_object_mut() {
                        object.remove("partial_json");
                    }
                }
            }
            Some("message_delta") => {
                for field in ["stop_reason", "stop_sequence"] {
                    if !event["delta"][field].is_null() {
                        message[field] = event["delta"][field].clone();
                    }
                }
                merge_usage(&mut message["usage"], &event["usage"]);
            }
            Some("message_stop") => break,
            _ => {}
        }
    }
    Ok(message)
}

fn parse_events(bytes: &[u8]) -> Result<(Vec<Value>, bool), ApiError> {
    let invalid = || ApiError(StatusCode::BAD_GATEWAY, INVALID_STREAM.into());
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid())?
        .replace("\r\n", "\n");
    let mut events = Vec::new();
    let mut done = false;
    for frame in text.split("\n\n") {
        let data = frame
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data:")
                    .map(|data| data.strip_prefix(' ').unwrap_or(data))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            done = true;
            continue;
        }
        let event: Value = serde_json::from_str(&data).map_err(|_| invalid())?;
        if event["error"].is_object() || event["type"] == "error" {
            return Err(invalid());
        }
        events.push(event);
    }
    Ok((events, done))
}

fn append_str(target: &mut Value, text: &Value) {
    if let Some(text) = text.as_str() {
        *target = format!("{}{text}", target.as_str().unwrap_or("")).into();
    }
}

fn merge_usage(target: &mut Value, usage: &Value) {
    if let (Some(target), Some(fields)) = (target.as_object_mut(), usage.as_object()) {
        for (key, value) in fields {
            if !value.is_null() {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn merge_delta(target: &mut Value, delta: &Value) {
    match delta {
        Value::Null => {}
        Value::Object(fields) => {
            if !target.is_object() {
                *target = json!({});
            }
            for (key, value) in fields {
                if value.is_null() {
                    continue;
                }
                if matches!(key.as_str(), "id" | "type" | "role" | "format" | "index") {
                    target[key] = value.clone();
                } else {
                    merge_delta(&mut target[key], value);
                }
            }
        }
        Value::Array(items) => {
            if !target.is_array() {
                *target = json!([]);
            }
            let output = target.as_array_mut().unwrap();
            for item in items {
                if let Some(index) = item.get("index")
                    && let Some(existing) = output
                        .iter_mut()
                        .find(|existing| existing["index"] == *index)
                {
                    merge_delta(existing, item);
                } else {
                    output.push(item.clone());
                }
            }
        }
        Value::String(text) => *target = format!("{}{text}", target.as_str().unwrap_or("")).into(),
        value => *target = value.clone(),
    }
}

fn session_id(headers: &HeaderMap, request: &Value) -> (&'static str, String) {
    let identity = cache::identity(headers, request);
    if let Some((source, signal)) = identity
        && signal.len() == 30
        && signal.starts_with("ses_")
        && signal.as_bytes()[4..16]
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        && signal.as_bytes()[16..]
            .iter()
            .all(u8::is_ascii_alphanumeric)
    {
        return (source, signal.to_owned());
    }
    let first_turn = request
        .get("messages")
        .or_else(|| request.get("input"))
        .and_then(Value::as_array)
        .and_then(|messages| messages.iter().find(|message| message["role"] == "user"))
        .map(|message| message["content"].to_string())
        .unwrap_or_else(|| request["input"].as_str().unwrap_or("").to_owned());
    // Routing identity only: deterministic within this build, never an auth token.
    let mut hash = DefaultHasher::new();
    headers
        .get("x-api-key")
        .or_else(|| headers.get(header::AUTHORIZATION))
        .map(|key| key.as_bytes())
        .hash(&mut hash);
    let (source, signal) = identity.unwrap_or((
        if request
            .get("messages")
            .or_else(|| request.get("input"))
            .is_some_and(Value::is_array)
        {
            "first_user_message"
        } else {
            "input"
        },
        &first_turn,
    ));
    signal.hash(&mut hash);
    let time = hash.finish();
    "ses".hash(&mut hash);
    (source, opencode_id("ses", time, hash.finish()))
}

/// 每次上游请求一个 msg_ 形态的轮次 id，如同 OpenCode 自己发送。
fn turn_id() -> String {
    let mut hash = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_nanos() as u64)
        .unwrap_or(0)
        .hash(&mut hash);
    let time = hash.finish();
    "msg".hash(&mut hash);
    opencode_id("msg", time, hash.finish())
}

fn opencode_id(prefix: &str, time: u64, random: u64) -> String {
    let mut suffix = [b'0'; 14];
    let mut random = random;
    for byte in suffix.iter_mut().rev() {
        *byte = BASE62[(random % 62) as usize];
        random /= 62;
    }
    format!(
        "{prefix}_{:012x}{}",
        time & 0xffff_ffff_ffff,
        std::str::from_utf8(&suffix).unwrap()
    )
}
