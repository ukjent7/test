use serde_json::{Value, json};

use crate::{app::Change, cache};

fn unsigned_thinking(block: &Value) -> bool {
    block["type"] == "thinking"
        && block["signature"]
            .as_str()
            .is_none_or(|value| value.trim().is_empty())
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
        let Some(blocks) = message["content"].as_array() else {
            target_message += 1;
            return true;
        };
        if blocks.iter().all(|block| {
            unsigned_thinking(block) && block["thinking"].as_str().unwrap_or("").trim().is_empty()
        }) {
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
) -> (Vec<u8>, Vec<Change>) {
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
    if endpoint != "messages" {
        return (frame.to_vec(), Vec::new());
    }
    let mut changes = normalize_response(&mut value);
    if changes.is_empty() {
        return (frame.to_vec(), changes);
    }
    let mut event = format!(
        "SSE #{frame_number} · {}",
        value["type"].as_str().unwrap_or("")
    );
    if let Some(index) = value["index"].as_u64() {
        event.push_str(&format!(" · index {index}"));
    }
    for change in &mut changes {
        change.event = Some(event.clone());
    }
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
