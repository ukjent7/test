use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::app::Change;

/// Zen 免费层只服务 OpenCode 自身：请求须流式，且提供名为 bash 与 read 的
/// 工具（小写，仅按名匹配）。客户端已有工具完整保留；缺的补一个永远不该
/// 被调用的桩，模型调用桩的内容会被丢弃。
const ZEN_TOOLS: [&str; 2] = ["bash", "read"];
const STUB_DESCRIPTION: &str = "Not available in this session. Never call this tool.";

/// Zen 免费层的工具整形结果：补齐缺失的 bash/read 桩工具。
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
        let mut value = serde_json::from_str::<Value>(text).ok()?;
        let object = value.as_object_mut()?;
        let mut changed = false;
        const ALIASES: &[(&str, &[&str])] = &[
            ("target_file", &["file_path", "filepath", "path"]),
            (
                "target_directory",
                &["directory", "dir_path", "dir", "path"],
            ),
            ("file_path", &["target_file", "filepath"]),
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
        if changed {
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
                                );
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
                    if choice["finish_reason"] == "tool_calls" && emptied
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

/// 免费模型在发送前就按 Zen 免费层的要求整形：流式、补齐缺失的 bash/read 基础工具。
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
    let original_tools = request.get("tools").cloned();
    if request.get("tools").is_none() {
        request["tools"] = json!([]);
    }
    if let Some(tools_array) = request["tools"].as_array_mut() {
        for need in ZEN_TOOLS {
            if tools_array.iter().any(|tool| tool_name(tool, chat) == need) {
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
    let after_tools = request["tools"].clone();
    if original_tools.as_ref() != Some(&after_tools) {
        changes.push(Change::new(
            "/tools".into(),
            original_tools,
            Some(after_tools),
            "Zen 免费层要求流式与基础工具",
        ));
    }
    (changes, tools)
}
