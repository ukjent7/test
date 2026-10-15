use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use serde_json::{Value, json};

use super::markup;
use crate::app::Change;

/// OpenCode 官方客户端（@opencode/cli@2.0.29）的真实工具定义。Zen 免费层自 2026-10-14 起
/// 逐字节校验每个它认识的工具名的描述与参数 schema，自编桩（哪怕名字正确）一律 403。
const OPENCODE_TOOLS: &str = include_str!("opencode_tools.json");

/// 【硬性协议门槛说明】Zen 免费层只服务 OpenCode 自身：服务端强制校验请求必须为流式，且
/// tools 中必须带 read 的真实定义（shell 单独不满足，已对线上端点实测），否则直接返回 403
/// FreeTierError。定义必须与官方客户端逐字节一致，因此这里内嵌官方原文，绝不手写。
///
/// 客户端已有工具完整保留；缺的补齐官方桩，模型若调用桩工具其内容会被安全丢弃。
const ZEN_TOOLS: [&str; 2] = ["shell", "read"];

fn opencode_tools() -> &'static BTreeMap<String, Value> {
    static TOOLS: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    TOOLS.get_or_init(|| serde_json::from_str(OPENCODE_TOOLS).unwrap_or_default())
}

/// Zen 免费层的工具整形结果：补齐缺失的 shell/read 官方桩工具。
#[derive(Default)]
pub struct ZenTools {
    pub(crate) stubs: BTreeSet<String>, // 客户端没有、只为 Zen 补的名字
    pub(crate) schemas: BTreeMap<String, Value>, // 客户端原名 → 参数 schema
}

impl ZenTools {
    pub(crate) fn is_stub(&self, name: &str) -> bool {
        self.stubs.contains(name)
    }

    /// 纠偏常见工具名符号错误（例如模型将 XML 标记中的 `=` 带入函数名 `hashline=grep`）。
    pub(crate) fn normalize_name(&self, name: &str) -> Option<String> {
        if name.is_empty() || self.schemas.contains_key(name) {
            return None;
        }
        let candidate = name.replace('=', "_");
        if candidate != name && self.schemas.contains_key(&candidate) {
            Some(candidate)
        } else {
            None
        }
    }

    /// 对齐常见参数别名（例如模型习惯输出 file_path，但客户端仅接受 target_file）。
    pub(crate) fn normalize_args(&self, name: &str, text: &mut String) -> Option<String> {
        let schema = self.schemas.get(name)?;
        let mut value = match serde_json::from_str::<Value>(text) {
            Ok(v) => v,
            Err(_) => {
                let repaired = markup::repair_json(text);
                serde_json::from_str::<Value>(&repaired).ok()?
            }
        };
        let object = value.as_object_mut()?;
        let mut changed = false;
        const ALIASES: &[(&str, &[&str])] = &[
            ("target_file", &["file_path", "filepath", "path"]),
            (
                "target_directory",
                &["directory", "dir_path", "dir", "path"],
            ),
            ("file_path", &["target_file", "filepath"]),
            ("block_until_ms", &["block_until"]),
        ];
        for &(canonical, aliases) in ALIASES {
            let has_canonical_prop = schema["properties"].get(canonical).is_some();
            let is_canonical_req = schema["required"]
                .as_array()
                .is_some_and(|r| r.iter().any(|v| v == canonical));
            if (!has_canonical_prop && !is_canonical_req) || object.contains_key(canonical) {
                continue;
            }
            for &alias in aliases {
                if schema["properties"].get(alias).is_none()
                    && let Some(val) = object.remove(alias)
                {
                    object.insert(canonical.to_string(), val);
                    changed = true;
                    break;
                }
            }
        }
        if let Some(properties) = schema["properties"].as_object() {
            for (key, val) in object.iter_mut() {
                let Some(raw) = val.as_str() else {
                    continue;
                };
                let Some(prop) = properties.get(key) else {
                    continue;
                };
                let trimmed = raw.trim();
                let expect_object = trimmed.starts_with('{')
                    && trimmed.ends_with('}')
                    && markup::prop_has_type(prop, "object");
                let expect_array = trimmed.starts_with('[')
                    && trimmed.ends_with(']')
                    && markup::prop_has_type(prop, "array");
                if (expect_object || expect_array)
                    && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
                    && (parsed.is_object() || parsed.is_array())
                {
                    *val = parsed;
                    changed = true;
                } else if markup::prop_has_type(prop, "integer")
                    && let Ok(parsed) = trimmed.parse::<i64>()
                {
                    *val = json!(parsed);
                    changed = true;
                } else if markup::prop_has_type(prop, "number")
                    && let Ok(parsed) = trimmed.parse::<f64>()
                {
                    *val = json!(parsed);
                    changed = true;
                } else if markup::prop_has_type(prop, "boolean") {
                    if trimmed == "true" {
                        *val = json!(true);
                        changed = true;
                    } else if trimmed == "false" {
                        *val = json!(false);
                        changed = true;
                    }
                }
            }
        }
        if changed || serde_json::from_str::<Value>(text).is_err() {
            let serialized = serde_json::to_string(&value).ok()?;
            let old = std::mem::replace(text, serialized);
            Some(old)
        } else {
            None
        }
    }

    /// 非流式 JSON 回复：丢弃桩调用，没有真实调用时把工具
    /// 结束改判为普通结束。
    pub fn fix_response(&self, value: &mut Value, endpoint: &str) -> Vec<Change> {
        let mut changes = Vec::new();
        match endpoint {
            "messages" => {
                if let Some(blocks) = value["content"].as_array_mut() {
                    let mut kept = Vec::new();
                    for (position, block) in blocks.drain(..).enumerate() {
                        if block["type"] == "tool_use" {
                            let name = block["name"].as_str().unwrap_or("");
                            if self.is_stub(name) {
                                changes.push(Change::new(
                                    format!("/content/{position}"),
                                    Some(block),
                                    None,
                                    "丢弃 Zen 免费层桩工具调用",
                                ));
                                continue;
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
                        let mut name = call["function"]["name"].as_str().unwrap_or("").to_owned();
                        if let Some(normalized) = self.normalize_name(&name) {
                            changes.push(Change::new(
                                format!("/choices/{position}/message/tool_calls/{call_position}/function/name"),
                                Some(json!(name)),
                                Some(json!(&normalized)),
                                "纠偏工具名标记符号泄漏",
                            ));
                            name = normalized;
                            call["function"]["name"] = json!(name);
                        }
                        if name.is_empty() {
                            changes.push(Change::new(
                                format!("/choices/{position}/message/tool_calls/{call_position}"),
                                Some(call),
                                None,
                                "丢弃空工具调用",
                            ));
                            continue;
                        }
                        if self.is_stub(&name) {
                            changes.push(Change::new(
                                format!("/choices/{position}/message/tool_calls/{call_position}"),
                                Some(call),
                                None,
                                "丢弃 Zen 免费层桩工具调用",
                            ));
                            continue;
                        }
                        if let Some(args) = call["function"]["arguments"].as_str() {
                            let mut args_text = args.to_owned();
                            if let Some(before) = self.normalize_args(&name, &mut args_text) {
                                changes.push(Change::new(
                                    format!("/choices/{position}/message/tool_calls/{call_position}/function/arguments"),
                                    Some(json!(before)),
                                    Some(json!(&args_text)),
                                    "对齐工具参数别名",
                                ));
                                call["function"]["arguments"] = json!(args_text);
                            }
                        }
                        kept.push(call);
                    }
                    let emptied = kept.is_empty();
                    *calls = kept;
                    if emptied && let Some(message) = message.as_object_mut() {
                        message.remove("tool_calls");
                    }
                    if choice["finish_reason"] == "tool_calls" && emptied {
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

pub(crate) fn fix_output(output: &mut Vec<Value>, changes: &mut Vec<Change>, tools: &ZenTools) {
    let mut kept = Vec::new();
    for (position, mut item) in output.drain(..).enumerate() {
        if item["type"] == "function_call" {
            let mut name = item["name"].as_str().unwrap_or("").to_owned();
            if let Some(normalized) = tools.normalize_name(&name) {
                changes.push(Change::new(
                    format!("/output/{position}/name"),
                    Some(json!(name)),
                    Some(json!(&normalized)),
                    "纠偏工具名标记符号泄漏",
                ));
                name = normalized;
                item["name"] = json!(name);
            }
            if tools.is_stub(&name) {
                changes.push(Change::new(
                    format!("/output/{position}"),
                    Some(item),
                    None,
                    "丢弃 Zen 免费层桩工具调用",
                ));
                continue;
            }
            if let Some(args) = item["arguments"].as_str() {
                let mut args_text = args.to_owned();
                if let Some(before) = tools.normalize_args(&name, &mut args_text) {
                    changes.push(Change::new(
                        format!("/output/{position}/arguments"),
                        Some(json!(before)),
                        Some(json!(&args_text)),
                        "对齐工具参数别名",
                    ));
                    item["arguments"] = json!(args_text);
                }
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

/// 把引导语并入顶层指令字段（messages 的 system、responses 的 instructions）。
fn steer_field(request: &mut Value, field: &str, steering: &str) -> (String, Option<Value>, Value) {
    match request.get_mut(field) {
        Some(Value::String(existing)) => {
            let before = json!(existing.clone());
            let merged = format!("{steering}\n\n{existing}");
            *existing = merged.clone();
            (format!("/{field}"), Some(before), json!(merged))
        }
        Some(Value::Array(blocks)) => {
            let block = json!({"type": "text", "text": steering});
            blocks.insert(0, block.clone());
            (format!("/{field}/0"), None, block)
        }
        Some(other) => {
            let before = other.clone();
            *other = json!(steering);
            (format!("/{field}"), Some(before), json!(steering))
        }
        None => {
            request[field] = json!(steering);
            (format!("/{field}"), None, json!(steering))
        }
    }
}

/// 免费模型在发送前就按 Zen 免费层的要求整形：流式、把官方清单里的工具定义改写为原文、
/// 补齐缺失的 shell/read 官方桩工具，并在系统指令里劝退模型调用这些占位工具。
/// 改动以字段为单位记录，避免把用户正文带进差异视图。
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
    if request.get("response_format").is_some_and(|rf| {
        rf.get("type") == Some(&json!("json_schema")) || rf.get("json_schema").is_some()
    }) {
        let before = request
            .as_object_mut()
            .and_then(|obj| obj.remove("response_format"));
        changes.push(Change::new(
            "/response_format".into(),
            before,
            None,
            "Zen 免费层不支持 json_schema 类型的 response_format",
        ));
    }
    if request.get("tools").is_none() {
        request["tools"] = json!([]);
    }
    // 免费层同时校验客户端自带工具的 definition：名字在官方清单里的必须换成原文，
    // 否则整个请求 403。清单外的名字原样透传，下游函数调用不受影响。
    if let Some(tools_array) = request["tools"].as_array_mut() {
        for (position, tool) in tools_array.iter_mut().enumerate() {
            let name = tool_name(tool, chat).to_owned();
            let Some(definition) = opencode_tools().get(&name) else {
                continue;
            };
            if chat {
                let func = &mut tool["function"];
                if func["description"] != definition["description"] {
                    let before = func["description"].clone();
                    func["description"] = definition["description"].clone();
                    changes.push(Change::new(
                        format!("/tools/{position}/function/description"),
                        Some(before),
                        Some(definition["description"].clone()),
                        "Zen 免费层校验工具定义，同名工具改写为官方原文",
                    ));
                }
                if func["parameters"] != definition["parameters"] {
                    let before = func["parameters"].clone();
                    func["parameters"] = definition["parameters"].clone();
                    changes.push(Change::new(
                        format!("/tools/{position}/function/parameters"),
                        Some(before),
                        Some(definition["parameters"].clone()),
                        "Zen 免费层校验工具定义，同名工具改写为官方原文",
                    ));
                }
            } else if endpoint == "responses" {
                if tool["description"] != definition["description"] {
                    let before = tool["description"].clone();
                    tool["description"] = definition["description"].clone();
                    changes.push(Change::new(
                        format!("/tools/{position}/description"),
                        Some(before),
                        Some(definition["description"].clone()),
                        "Zen 免费层校验工具定义，同名工具改写为官方原文",
                    ));
                }
                if tool["parameters"] != definition["parameters"] {
                    let before = tool["parameters"].clone();
                    tool["parameters"] = definition["parameters"].clone();
                    changes.push(Change::new(
                        format!("/tools/{position}/parameters"),
                        Some(before),
                        Some(definition["parameters"].clone()),
                        "Zen 免费层校验工具定义，同名工具改写为官方原文",
                    ));
                }
            } else {
                if tool["description"] != definition["description"] {
                    let before = tool["description"].clone();
                    tool["description"] = definition["description"].clone();
                    changes.push(Change::new(
                        format!("/tools/{position}/description"),
                        Some(before),
                        Some(definition["description"].clone()),
                        "Zen 免费层校验工具定义，同名工具改写为官方原文",
                    ));
                }
                if tool["input_schema"] != definition["parameters"] {
                    let before = tool["input_schema"].clone();
                    tool["input_schema"] = definition["parameters"].clone();
                    changes.push(Change::new(
                        format!("/tools/{position}/input_schema"),
                        Some(before),
                        Some(definition["parameters"].clone()),
                        "Zen 免费层校验工具定义，同名工具改写为官方原文",
                    ));
                }
            }
        }
    }
    let tools_before_stubs = request.get("tools").cloned();
    if let Some(tools_array) = request["tools"].as_array_mut() {
        for need in ZEN_TOOLS {
            if tools_array.iter().any(|tool| tool_name(tool, chat) == need) {
                continue;
            }
            let Some(definition) = opencode_tools().get(need) else {
                continue;
            };
            let stub = match endpoint {
                "chat/completions" => json!({"type": "function", "function": {
                    "name": need, "description": definition["description"],
                    "parameters": definition["parameters"]}}),
                "responses" => json!({"type": "function", "name": need,
                    "description": definition["description"],
                    "parameters": definition["parameters"]}),
                _ => json!({"name": need, "description": definition["description"],
                    "input_schema": definition["parameters"]}),
            };
            tools_array.push(stub);
            tools.stubs.insert(need.to_owned());
        }
    }
    let after_tools = request["tools"].clone();
    if tools_before_stubs.as_ref() != Some(&after_tools) {
        changes.push(Change::new(
            "/tools".into(),
            tools_before_stubs,
            Some(after_tools),
            "Zen 免费层要求流式与基础工具",
        ));
    }
    // 桩定义必须与官方逐字节一致，无法再用描述劝退模型；改为在系统指令里点名这些占位
    // 工具不可用，把幻觉调用消灭在发生前。shell/read 与真实工具同名，必须明说不是本环境
    // 的工具，否则模型会把它当 bash 用。只针对网关注入的名字，客户端自带的同名工具
    // 有真实实现，不能劝退；也不劝模型改用文本回答，那会连带压制客户端自己的真实工具。
    if !tools.stubs.is_empty() {
        let names = tools.stubs.iter().cloned().collect::<Vec<_>>().join("、");
        let steering = format!("{names} 不是本环境的工具，只是无实现的占位符，不可调用。");
        let change = if chat {
            request
                .get_mut("messages")
                .and_then(Value::as_array_mut)
                .map(|messages| {
                    let message = json!({"role": "system", "content": &steering});
                    messages.insert(0, message.clone());
                    ("/messages/0".to_owned(), None, Some(message))
                })
        } else if endpoint == "messages" {
            let (path, before, after) = steer_field(request, "system", &steering);
            Some((path, before, Some(after)))
        } else {
            let (path, before, after) = steer_field(request, "instructions", &steering);
            Some((path, before, Some(after)))
        };
        if let Some((path, before, after)) = change {
            changes.push(Change::new(path, before, after, "Zen 免费层占位工具引导"));
        }
    }
    (changes, tools)
}
