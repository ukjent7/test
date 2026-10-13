use serde_json::{Value, json};

use crate::app::Change;

pub(crate) type MarkupCall = (String, Vec<(String, String)>);

/// 解析单个 <tool_call> 标记段（不含收尾标签）：<function=名字> 后跟
/// 一串 <parameter=键>值</parameter>；结构不符时放弃，content 原样保留。
pub(crate) fn parse_markup_call(region: &str) -> Option<MarkupCall> {
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

pub(crate) fn prop_has_type(prop: &Value, expected: &str) -> bool {
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
pub(crate) fn typed_value(schema: &Value, key: &str, raw: &str) -> Value {
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

/// 修复损坏或非标的 JSON 字符串字面量（学习 pi 的 repairJson 成熟经验）：
/// - 转义字符串内的裸控制字符（换行、回车、制表符等）；
/// - 修复非法或孤立的反斜杠转义（如 Windows 路径中的 `\p` 或正则中的 `\d`）。
pub(crate) fn repair_json(raw: &str) -> String {
    let mut repaired = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if !in_string {
            repaired.push(c);
            if c == '"' {
                in_string = true;
            }
            continue;
        }
        if c == '"' {
            repaired.push(c);
            in_string = false;
            continue;
        }
        if c == '\\' {
            let Some(&next) = chars.peek() else {
                repaired.push_str("\\\\");
                continue;
            };
            if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
                repaired.push('\\');
                repaired.push(next);
                chars.next();
                continue;
            }
            if next == 'u' {
                let peek: Vec<char> = chars.clone().take(5).collect();
                if peek.len() == 5 && peek[1..5].iter().all(|h| h.is_ascii_hexdigit()) {
                    repaired.push('\\');
                    repaired.push('u');
                    chars.next();
                    for _ in 0..4 {
                        if let Some(h) = chars.next() {
                            repaired.push(h);
                        }
                    }
                    continue;
                }
            }
            repaired.push_str("\\\\");
            continue;
        }
        if (c as u32) <= 0x1f {
            match c {
                '\n' => repaired.push_str("\\n"),
                '\r' => repaired.push_str("\\r"),
                '\t' => repaired.push_str("\\t"),
                '\x08' => repaired.push_str("\\b"),
                '\x0c' => repaired.push_str("\\f"),
                _ => {
                    use std::fmt::Write;
                    let _ = write!(repaired, "\\u{:04x}", c as u32);
                }
            }
        } else {
            repaired.push(c);
        }
    }
    repaired
}

/// 学习 pi 的思维链优先级探测经验（reasoning_content > reasoning > reasoning_text）：
/// 客户端只认 reasoning_content，优先使用首个非空思维链字段并统一为标准字段名，
/// 剔除原别名字段；空串或标准字段已有非空值时不动。
/// 改动只记路径不搬正文，避免把思维链复制进差异视图。
pub(crate) fn rename_reasoning(delta: &mut Value, choice_index: u64, changes: &mut Vec<Change>) {
    let taken = matches!(
        delta.get("reasoning_content"),
        Some(value) if !value.is_null() && value.as_str().is_some_and(|s| !s.is_empty())
    );
    if taken {
        return;
    }
    let (source_field, text) = if let Some(text) = delta
        .get("reasoning")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        ("reasoning", text)
    } else if let Some(text) = delta
        .get("reasoning_text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        ("reasoning_text", text)
    } else {
        return;
    };
    delta["reasoning_content"] = json!(text);
    if let Some(object) = delta.as_object_mut() {
        object.remove(source_field);
    }
    changes.push(Change::new(
        format!("/choices/{choice_index}/delta/{source_field}"),
        None,
        None,
        "思维链字段回改为 reasoning_content",
    ));
}
