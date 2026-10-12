use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::app::Change;

/// Zen 免费层只服务 OpenCode 自身：请求须流式，且提供名为 bash 与 read 的
/// 工具（小写，仅按名匹配）。客户端已有的大小写变体（如 Claude Code 的
/// Bash）改名后提供，回复里的调用再改回客户端原名；缺的补一个永远不该
/// 被调用的桩，模型调用桩的内容会被丢弃。
const ZEN_TOOLS: [&str; 2] = ["bash", "read"];
const STUB_DESCRIPTION: &str = "Not available in this session. Never call this tool.";

/// Zen 免费层的工具整形结果：改名映射与补出来的桩。
#[derive(Default)]
pub struct ZenTools {
    pub(crate) back: BTreeMap<String, String>,   // 提供给 Zen 的名字 → 客户端原名
    pub(crate) stubs: BTreeSet<String>,          // 客户端没有、只为 Zen 补的名字
    pub(crate) schemas: BTreeMap<String, Value>, // 客户端原名 → 参数 schema
}

impl ZenTools {
    pub(crate) fn client_name(&self, zen_name: &str) -> Option<&str> {
        self.back.get(zen_name).map(String::as_str)
    }

    pub(crate) fn is_stub(&self, name: &str) -> bool {
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

pub(crate) fn fix_output(output: &mut Vec<Value>, changes: &mut Vec<Change>, tools: &ZenTools) {
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
