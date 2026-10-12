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

/// Zen 免费层用 `reasoning` 承载思维链，客户端只认 `reasoning_content`：
/// 搬到标准字段名并删掉原名；空串或标准字段已有值时不动。
/// 改动只记路径不搬正文，避免把思维链复制进差异视图。
pub(crate) fn rename_reasoning(delta: &mut Value, choice_index: u64, changes: &mut Vec<Change>) {
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
