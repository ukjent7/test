use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{DefaultHasher, Hash, Hasher},
};

use serde_json::{Value, json};

use super::{
    markup::{self, MarkupCall},
    tools::{self, ZenTools},
};
use crate::app::Change;

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
                    if let Some(call) = markup::parse_markup_call(&buffer[..end]) {
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
            markup::rename_reasoning(delta, choice_index, &mut changes);
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
                    if !name.is_empty() {
                        self.calls_made = true;
                    }
                    let path = format!("/choices/{choice_index}/delta/tool_calls/{call_index}");
                    let fragment = call["function"]["arguments"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    let entry = self
                        .call_args
                        .entry((choice_index, call_index))
                        .or_default();
                    if !name.is_empty() {
                        entry.name = name;
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
                        Some(schema) => markup::typed_value(schema, key, raw),
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
                    tools::fix_output(output, &mut changes, &self.tools);
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
