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
    id: String,
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
    calls_made: bool,
    stub_ids: BTreeSet<String>,
    dropped_items: u64,
    call_args: BTreeMap<(u64, u64), CallArgs>,
    call_map: BTreeMap<(u64, u64), u64>,
    next_client_index: BTreeMap<u64, u64>,
    markup: Option<String>,
    synthetic: u64,
}

impl StreamFix {
    pub fn new(tools: ZenTools) -> Self {
        Self {
            tools,
            stub_blocks: BTreeSet::new(),
            dropped_blocks: 0,
            stub_calls: BTreeSet::new(),
            calls_made: false,
            stub_ids: BTreeSet::new(),
            dropped_items: 0,
            call_args: BTreeMap::new(),
            call_map: BTreeMap::new(),
            next_client_index: BTreeMap::new(),
            markup: None,
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

    pub fn has_pending_markup(&self) -> bool {
        self.markup.is_some()
    }

    /// 上游参数要同时满足 JSON 完整（支持容错修复）与工具必填字段才下发：原生流经常
    /// 缺字段或串台，仅 JSON 合法不足以让客户端接受。
    fn args_valid(schemas: &BTreeMap<String, Value>, name: &str, text: &str) -> bool {
        let value = serde_json::from_str::<Value>(text).ok().or_else(|| {
            let repaired = markup::repair_json(text);
            serde_json::from_str::<Value>(&repaired).ok()
        });
        let Some(value) = value else {
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
            let mut kept = Vec::new();
            if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for mut call in calls.drain(..) {
                    let call_index = call["index"].as_u64().unwrap_or(0);
                    let mut name = call["function"]["name"].as_str().unwrap_or("").to_owned();
                    let call_id = call["id"].as_str().unwrap_or("").to_owned();
                    let has_new_identity = !call_id.is_empty() || !name.is_empty();

                    let is_reused_index = if let Some(&existing_idx) =
                        self.call_map.get(&(choice_index, call_index))
                    {
                        if let Some(existing_args) =
                            self.call_args.get(&(choice_index, existing_idx))
                        {
                            let different_id = !call_id.is_empty()
                                && !existing_args.id.is_empty()
                                && call_id != existing_args.id;
                            let flushed_with_new_call = existing_args.flushed && has_new_identity;
                            different_id || flushed_with_new_call
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if is_reused_index {
                        self.stub_calls.remove(&(choice_index, call_index));
                        self.call_map.remove(&(choice_index, call_index));
                    }

                    let is_empty_call = name.is_empty()
                        && call_id.is_empty()
                        && !self.call_map.contains_key(&(choice_index, call_index));
                    if is_empty_call {
                        self.stub_calls.insert((choice_index, call_index));
                        changes.push(Change::new(
                            format!("/choices/{choice_index}/delta/tool_calls/{call_index}"),
                            Some(call),
                            None,
                            "丢弃空工具调用",
                        ));
                        continue;
                    }
                    if self.tools.is_stub(&name) {
                        self.stub_calls.insert((choice_index, call_index));
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
                    let client_index = *self
                        .call_map
                        .entry((choice_index, call_index))
                        .or_insert_with(|| {
                            let next = self.next_client_index.entry(choice_index).or_default();
                            let current = *next;
                            *next += 1;
                            current
                        });
                    call["index"] = json!(client_index);
                    let path = format!("/choices/{choice_index}/delta/tool_calls/{client_index}");
                    if let Some(normalized) = self.tools.normalize_name(&name) {
                        changes.push(Change::new(
                            format!("{path}/function/name"),
                            Some(json!(name)),
                            Some(json!(&normalized)),
                            "纠偏工具名标记符号泄漏",
                        ));
                        name = normalized;
                        call["function"]["name"] = json!(name);
                    }
                    let fragment = call["function"]["arguments"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    let entry = self
                        .call_args
                        .entry((choice_index, client_index))
                        .or_default();
                    if !call_id.is_empty() && entry.id.is_empty() {
                        entry.id = call_id.clone();
                    }
                    let has_name = !name.is_empty();
                    if has_name {
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
                        if let Some(before) =
                            self.tools.normalize_args(&entry.name, &mut entry.text)
                        {
                            changes.push(Change::new(
                                format!("{path}/function/arguments"),
                                Some(json!(before)),
                                Some(json!(&entry.text)),
                                "对齐工具参数别名",
                            ));
                        }
                        if Self::args_valid(&self.tools.schemas, &entry.name, &entry.text) {
                            entry.flushed = true;
                            if serde_json::from_str::<Value>(&entry.text).is_err() {
                                let repaired = markup::repair_json(&entry.text);
                                if serde_json::from_str::<Value>(&repaired).is_ok() {
                                    entry.text = repaired;
                                }
                            }
                            call["function"]["arguments"] = json!(entry.text.clone());
                        } else {
                            call["function"]["arguments"] = json!("");
                        }
                    }
                    let has_id = call["id"].as_str().is_some_and(|id| !id.is_empty());
                    let has_args = call["function"]["arguments"]
                        .as_str()
                        .is_some_and(|args| !args.is_empty());
                    if has_id || has_name || has_args {
                        kept.push(call);
                    }
                }
                *calls = kept;
                if calls.is_empty()
                    && let Some(delta) = delta.as_object_mut()
                {
                    delta.remove("tool_calls");
                }
            }
            for (mut name, params) in markup_calls {
                if let Some(normalized) = self.tools.normalize_name(&name) {
                    changes.push(Change::new(
                        format!("/choices/{choice_index}/delta/tool_calls"),
                        Some(json!(name)),
                        Some(json!(&normalized)),
                        "纠偏工具名标记符号泄漏",
                    ));
                    name = normalized;
                }
                let mut object = serde_json::Map::new();
                for (key, raw) in &params {
                    let typed = match self.tools.schemas.get(&name) {
                        Some(schema) => markup::typed_value(schema, key, raw),
                        None => json!(raw),
                    };
                    object.insert(key.clone(), typed);
                }
                let mut args = serde_json::to_string(&object).unwrap();
                if let Some(before) = self.tools.normalize_args(&name, &mut args) {
                    changes.push(Change::new(
                        format!("/choices/{choice_index}/delta/tool_calls"),
                        Some(json!(before)),
                        Some(json!(&args)),
                        "对齐工具参数别名",
                    ));
                }
                let matched = self
                    .call_args
                    .iter_mut()
                    .find(|((choice, _), entry)| {
                        *choice == choice_index && !entry.flushed && entry.name == name
                    })
                    .map(|((_, client_index), entry)| {
                        entry.flushed = true;
                        entry.text = args.clone();
                        *client_index
                    });
                let entry = match matched {
                    Some(index) => json!({"index": index,
                        "function": {"arguments": args}}),
                    None => {
                        let next = self.next_client_index.entry(choice_index).or_default();
                        let index = *next;
                        *next += 1;
                        self.synthetic += 1;
                        let mut hasher = DefaultHasher::new();
                        (choice_index, index, self.synthetic).hash(&mut hasher);
                        let id = format!("call_{:016x}", hasher.finish());
                        self.call_args.insert(
                            (choice_index, index),
                            CallArgs {
                                id: id.clone(),
                                name: name.clone(),
                                text: String::new(),
                                flushed: true,
                            },
                        );
                        json!({"index": index, "id": id,
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
                && (!self.tools.stubs.is_empty() || !self.stub_calls.is_empty())
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
            let mut extra = Vec::new();
            for ((call_choice, client_index), entry) in self.call_args.iter_mut() {
                if *call_choice != choice_index || entry.flushed || entry.text.is_empty() {
                    continue;
                }
                if serde_json::from_str::<Value>(&entry.text).is_err() {
                    let repaired = markup::repair_json(&entry.text);
                    if serde_json::from_str::<Value>(&repaired).is_ok() {
                        entry.text = repaired;
                    }
                }
                if let Some(before) = self.tools.normalize_args(&entry.name, &mut entry.text) {
                    changes.push(Change::new(
                        format!("/choices/{choice_index}/delta/tool_calls/{client_index}/function/arguments"),
                        Some(json!(before)),
                        Some(json!(&entry.text)),
                        "对齐工具参数别名",
                    ));
                }
                entry.flushed = true;
                extra.push(json!({"index": *client_index,
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
                    let mut name = value["item"]["name"].as_str().unwrap_or("").to_owned();
                    if let Some(normalized) = self.tools.normalize_name(&name) {
                        changes.push(Change::new(
                            format!("/output_item/{output_index}/item/name"),
                            Some(json!(name)),
                            Some(json!(&normalized)),
                            "纠偏工具名标记符号泄漏",
                        ));
                        name = normalized;
                        value["item"]["name"] = json!(name);
                    }
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
