use std::collections::HashMap;

use serde_json::{Value, json};

use crate::{app::Change, cache, zen};

fn unsigned_thinking(block: &Value) -> bool {
    block["type"] == "thinking"
        && block["signature"]
            .as_str()
            .is_none_or(|value| value.trim().is_empty())
}

/// 对齐并发工具响应顺序与工具名：客户端并发执行工具时，完成顺序
/// 与上一条 assistant 消息里的 tool_calls 声明顺序往往不同，且缺少 name 字段。
/// 上游模型如果按位置或缺失 name 处理，会导致工具结果错位（如将 git status
/// 的报错绑定到 list_dir）。将连续的 tool 响应按 tool_calls 顺序稳定重排并补齐 name。
pub(crate) fn align_tool_responses(request: &mut Value) -> Vec<Change> {
    let mut changes = Vec::new();
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return changes;
    };
    let mut i = 0;
    while i < messages.len() {
        if messages[i]["role"] != "assistant" {
            i += 1;
            continue;
        }
        let Some(tool_calls) = messages[i]["tool_calls"].as_array() else {
            i += 1;
            continue;
        };
        if tool_calls.is_empty() {
            i += 1;
            continue;
        }
        let original_tc = tool_calls.clone();
        let has_invalid_tc = tool_calls.iter().any(|tc| {
            tc["id"].as_str().is_none_or(str::is_empty)
                || tc["function"]["name"].as_str().is_none_or(str::is_empty)
        });
        if has_invalid_tc {
            let tc_array = messages[i]["tool_calls"].as_array_mut().unwrap();
            tc_array.retain(|tc| {
                tc["id"].as_str().is_some_and(|id| !id.is_empty())
                    && tc["function"]["name"]
                        .as_str()
                        .is_some_and(|name| !name.is_empty())
            });
            changes.push(Change::new(
                format!("/messages/{i}/tool_calls"),
                Some(json!(original_tc)),
                if tc_array.is_empty() {
                    None
                } else {
                    Some(json!(tc_array.clone()))
                },
                "剔除无效空工具调用",
            ));
            if tc_array.is_empty()
                && let Some(obj) = messages[i].as_object_mut()
            {
                obj.remove("tool_calls");
            }
        }
        let mut order_map = HashMap::new();
        if let Some(remaining_tc) = messages[i]["tool_calls"].as_array() {
            for (idx, tc) in remaining_tc.iter().enumerate() {
                if let Some(id) = tc["id"].as_str() {
                    let name = tc["function"]["name"].as_str().unwrap_or("").to_owned();
                    order_map.insert(id.to_owned(), (idx, name));
                }
            }
        }
        // 学习 pi 的思考模型重放经验：含 tool_calls 的 assistant 消息若缺失
        // reasoning_content 则补齐空串，防止小米 MiMo / DeepSeek 等上游报 400 校验异常。
        if !order_map.is_empty()
            && (messages[i].get("reasoning_content").is_none()
                || messages[i]["reasoning_content"].is_null())
        {
            messages[i]["reasoning_content"] = json!("");
            changes.push(Change::new(
                format!("/messages/{i}/reasoning_content"),
                None,
                Some(json!("")),
                "补齐思考模型助手工具调用缺失的 reasoning_content",
            ));
        }
        let start = i + 1;
        let mut end = start;
        while end < messages.len() && messages[end]["role"] == "tool" {
            end += 1;
        }
        if end > start {
            let already_aligned = end - start == order_map.len()
                && messages[start..end].iter().enumerate().all(|(pos, msg)| {
                    let Some(id) = msg["tool_call_id"].as_str() else {
                        return false;
                    };
                    let Some(&(expected_idx, ref name)) = order_map.get(id) else {
                        return false;
                    };
                    expected_idx == pos
                        && (name.is_empty() || msg["name"].as_str().is_some_and(|n| !n.is_empty()))
                });
            if already_aligned {
                i = end;
                continue;
            }

            let original_slice = messages[start..end].to_vec();
            let mut valid_slice: Vec<_> = original_slice
                .iter()
                .filter(|msg| {
                    msg["tool_call_id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty() && order_map.contains_key(id))
                })
                .cloned()
                .collect();
            valid_slice.sort_by_key(|msg| {
                msg["tool_call_id"]
                    .as_str()
                    .and_then(|id| order_map.get(id))
                    .map(|(idx, _)| *idx)
                    .unwrap_or(usize::MAX)
            });
            for msg in valid_slice.iter_mut() {
                let Some(id) = msg["tool_call_id"].as_str() else {
                    continue;
                };
                let Some((_, name)) = order_map.get(id) else {
                    continue;
                };
                if !name.is_empty() && msg["name"].as_str().is_none_or(str::is_empty) {
                    msg["name"] = json!(name);
                }
            }
            if valid_slice != original_slice {
                for (offset, msg) in original_slice.iter().enumerate() {
                    let id_valid = msg["tool_call_id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty() && order_map.contains_key(id));
                    if !id_valid {
                        changes.push(Change::new(
                            format!("/messages/{}", start + offset),
                            Some(msg.clone()),
                            None,
                            "剔除无效空工具调用",
                        ));
                    }
                }

                let orig_order: Vec<_> = original_slice
                    .iter()
                    .filter_map(|m| {
                        m["tool_call_id"]
                            .as_str()
                            .filter(|id| order_map.contains_key(*id))
                    })
                    .collect();
                let new_order: Vec<_> = valid_slice
                    .iter()
                    .filter_map(|m| m["tool_call_id"].as_str())
                    .collect();
                if orig_order != new_order {
                    changes.push(Change::new(
                        format!("/messages/{start}..{end}/order"),
                        Some(json!(orig_order)),
                        Some(json!(new_order)),
                        "对齐并发工具响应顺序与工具名",
                    ));
                }

                for (idx, msg) in valid_slice.iter().enumerate() {
                    let Some(id) = msg["tool_call_id"].as_str() else {
                        continue;
                    };
                    let orig_had_name = original_slice
                        .iter()
                        .find(|m| m["tool_call_id"].as_str() == Some(id))
                        .and_then(|m| m["name"].as_str())
                        .is_some_and(|n| !n.is_empty());
                    if !orig_had_name
                        && let Some(name) = msg["name"].as_str()
                        && !name.is_empty()
                    {
                        changes.push(Change::new(
                            format!("/messages/{}/name", start + idx),
                            None,
                            Some(json!(name)),
                            "对齐并发工具响应顺序与工具名",
                        ));
                    }
                }

                messages.splice(start..end, valid_slice.clone());
                end = start + valid_slice.len();
            }
        }
        i = end;
    }
    changes
}

pub(crate) fn normalize_history(request: &mut Value) -> Vec<Change> {
    let mut changes = Vec::new();
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return changes;
    };
    let mut source_message = 0;
    let mut target_message = 0;
    messages.retain_mut(|message| {
        let path = format!("/messages/{source_message}");
        source_message += 1;
        if message["role"] != "assistant" {
            target_message += 1;
            return true;
        }
        let has_tool_calls = message["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty());
        let Some(blocks) = message["content"].as_array() else {
            // 学习 pi 的空助手消息修剪经验：对非数组 content（Chat Completions 格式），
            // 若 content 为空且无 tool_calls，直接剔除，避免上游 400 拒收。
            let empty_text = message["content"].is_null()
                || message["content"]
                    .as_str()
                    .is_some_and(|s| s.trim().is_empty());
            if !has_tool_calls && empty_text {
                changes.push(Change::new(
                    path,
                    Some(message.clone()),
                    None,
                    "移除空助手消息",
                ));
                return false;
            }
            target_message += 1;
            return true;
        };
        if !has_tool_calls
            && (blocks.is_empty()
                || blocks.iter().all(|block| {
                    unsigned_thinking(block)
                        && block["thinking"].as_str().unwrap_or("").trim().is_empty()
                }))
        {
            changes.push(Change::new(
                path,
                Some(message.clone()),
                None,
                "移除空助手消息",
            ));
            return false;
        }
        let target_path = format!("/messages/{target_message}");
        target_message += 1;
        let blocks = message["content"].as_array_mut().unwrap();
        if !blocks.iter().any(unsigned_thinking) {
            return true;
        }
        let mut source_block = 0;
        let mut target_block = 0;
        // pi: unsigned thinking is plain text on replay; valid signatures remain opaque.
        blocks.retain_mut(|block| {
            let block_path = format!("{path}/content/{source_block}");
            source_block += 1;
            if !unsigned_thinking(block) {
                target_block += 1;
                return true;
            }
            let before = block.clone();
            let text = block["thinking"].as_str().unwrap_or("").to_owned();
            if text.trim().is_empty() {
                changes.push(Change::new(block_path, Some(before), None, "移除空思考块"));
                return false;
            }
            let object = block.as_object_mut().unwrap();
            object.insert("type".into(), json!("text"));
            object.insert("text".into(), json!(text));
            object.remove("thinking");
            object.remove("signature");
            let mut change = Change::new(
                block_path,
                Some(before),
                Some(block.clone()),
                "无签名思考内容转为文本",
            );
            change.after_path = format!("{target_path}/content/{target_block}");
            changes.push(change);
            target_block += 1;
            true
        });
        true
    });
    changes
}

pub(crate) fn normalize_response(value: &mut Value) -> Vec<Change> {
    let mut changes = Vec::new();
    let (blocks, prefix): (&mut [Value], &str) = match value["type"].as_str() {
        Some("content_block_start") => (
            value
                .get_mut("content_block")
                .map(std::slice::from_mut)
                .unwrap_or(&mut []),
            "/content_block",
        ),
        Some("message_start") => (
            value["message"]["content"]
                .as_array_mut()
                .map(Vec::as_mut_slice)
                .unwrap_or(&mut []),
            "/message/content",
        ),
        Some("message") => (
            value["content"]
                .as_array_mut()
                .map(Vec::as_mut_slice)
                .unwrap_or(&mut []),
            "/content",
        ),
        _ => return changes,
    };
    for (index, block) in blocks.iter_mut().enumerate() {
        if block["type"] != "thinking" {
            continue;
        }
        let mut changed = false;
        let before = block.clone();
        for field in ["thinking", "signature"] {
            if block.get(field).is_none_or(Value::is_null) {
                block[field] = json!("");
                changed = true;
            }
        }
        if changed {
            let path = if prefix == "/content_block" {
                prefix.into()
            } else {
                format!("{prefix}/{index}")
            };
            changes.push(Change::new(
                path,
                Some(before),
                Some(block.clone()),
                "补齐思考字段",
            ));
        }
    }
    changes
}

pub(crate) fn normalize_sse(
    frame: &[u8],
    frame_number: usize,
    endpoint: &str,
    usage: &mut cache::Usage,
    zen: Option<&mut zen::StreamFix>,
    responses_seq: &mut u64,
) -> (Vec<u8>, Vec<Change>) {
    let is_responses = endpoint == "responses";
    let has_tool_calls = frame.windows(12).any(|w| w == b"\"tool_calls\"");
    let has_usage = frame.windows(7).any(|w| w == b"\"usage\"");
    let has_reasoning = frame.windows(10).any(|w| w == b"\"reasoning");
    let has_markup = frame.windows(11).any(|w| w == b"<tool_call>");
    let markup_pending = zen.as_ref().is_some_and(|z| z.has_pending_markup());
    if endpoint != "messages"
        && !is_responses
        && !has_tool_calls
        && !has_usage
        && !has_reasoning
        && !has_markup
        && !markup_pending
    {
        return (frame.to_vec(), Vec::new());
    }
    let Ok(text) = std::str::from_utf8(frame) else {
        return (frame.to_vec(), Vec::new());
    };
    let data = text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .map(|value| value.strip_prefix(' ').unwrap_or(value))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let Ok(mut value) = serde_json::from_str::<Value>(&data) else {
        return (frame.to_vec(), Vec::new());
    };
    usage.observe(endpoint, &value);
    let mut changes = Vec::new();
    let mut keep = true;
    let pristine = zen.is_some().then(|| value.clone());
    if let Some(fix) = zen {
        let (kept, zen_changes) = fix.frame(&mut value, endpoint);
        changes.extend(zen_changes);
        keep = kept;
    }
    if keep && endpoint == "messages" {
        changes.extend(normalize_response(&mut value));
    }
    if keep
        && (is_responses
            || value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.starts_with("response.")))
    {
        if let Some(existing) = value.get("sequence_number").and_then(Value::as_u64) {
            *responses_seq = existing + 1;
        } else {
            let current_seq = *responses_seq;
            *responses_seq += 1;
            value["sequence_number"] = json!(current_seq);
            changes.push(Change::new(
                "/sequence_number".into(),
                None,
                Some(json!(current_seq)),
                "补齐 Responses 自增序号",
            ));
        }
    }
    if !keep {
        label_changes(&mut changes, frame_number, &value);
        return (Vec::new(), changes);
    }
    // Zen 修复可能不带差异记录地改写帧（如续传调用丢弃、序号平移），
    // 只要值确实变了就必须重打包，不能按原样透传。
    if changes.is_empty() && pristine.is_none_or(|before| before == value) {
        return (frame.to_vec(), Vec::new());
    }
    label_changes(&mut changes, frame_number, &value);
    let mut output = String::new();
    let mut replaced = false;
    for line in text.split_inclusive('\n') {
        if line.starts_with("data:") {
            if !replaced {
                output.push_str("data: ");
                output.push_str(&serde_json::to_string(&value).unwrap());
                if line.ends_with("\r\n") {
                    output.push_str("\r\n");
                } else if line.ends_with('\n') {
                    output.push('\n');
                }
                replaced = true;
            }
        } else {
            output.push_str(line);
        }
    }
    (output.into_bytes(), changes)
}

fn label_changes(changes: &mut [Change], frame_number: usize, value: &Value) {
    let mut event = format!("SSE #{frame_number}");
    if let Some(kind) = value["type"].as_str().filter(|kind| !kind.is_empty()) {
        event.push_str(&format!(" · {kind}"));
    }
    if let Some(index) = value["index"].as_u64() {
        event.push_str(&format!(" · index {index}"));
    }
    for change in changes {
        change.event = Some(event.clone());
    }
}
