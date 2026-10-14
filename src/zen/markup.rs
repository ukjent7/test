use serde_json::{Value, json};

use crate::app::Change;

pub(crate) type MarkupCall = (String, Vec<(String, String)>);

fn extract_attribute(text: &str, attr: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let pattern = format!("{attr}={quote}");
        if let Some(pos) = text.find(&pattern) {
            let after = &text[pos + pattern.len()..];
            if let Some(end) = after.find(quote) {
                return Some(after[..end].to_owned());
            }
        }
    }
    None
}

fn extract_func_name(header: &str) -> Option<String> {
    if let Some(name) = extract_attribute(header, "name") {
        return Some(name);
    }
    let trimmed = header.trim_start();
    let s = trimmed
        .strip_prefix('=')
        .or_else(|| trimmed.strip_prefix(':'))
        .unwrap_or(trimmed)
        .trim();
    let name = s
        .trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace())
        .split_whitespace()
        .next()?
        .to_owned();
    if name.is_empty() { None } else { Some(name) }
}

fn extract_key(header: &str) -> Option<String> {
    if let Some(key) =
        extract_attribute(header, "name").or_else(|| extract_attribute(header, "key"))
    {
        return Some(key);
    }
    let trimmed = header.trim_start();
    let s = trimmed.strip_prefix('=').unwrap_or(trimmed).trim();
    let key = s
        .trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace())
        .split_whitespace()
        .next()?
        .to_owned();
    if key.is_empty() { None } else { Some(key) }
}

fn trim_single_newline_pair(s: &str) -> &str {
    let mut result = s;
    if let Some(stripped) = result
        .strip_prefix("\r\n")
        .or_else(|| result.strip_prefix('\n'))
    {
        result = stripped;
    }
    if let Some(stripped) = result
        .strip_suffix("\r\n")
        .or_else(|| result.strip_suffix('\n'))
    {
        result = stripped;
    }
    result
}

fn parse_param_block(block: &str) -> Option<(String, String)> {
    let trimmed = block.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(gt) = block.find('>') {
        let header = &block[..gt];
        let is_malformed_header = if let Some(nl) = header.find('\n') {
            let first_line = header[..nl]
                .trim()
                .strip_prefix('=')
                .unwrap_or(header[..nl].trim());
            first_line.contains('=')
        } else {
            false
        };
        if !is_malformed_header {
            let key = extract_key(header)?;
            let body = &block[gt + 1..];
            let value = if body.trim().is_empty() {
                if let Some(val) = extract_attribute(header, "value") {
                    val
                } else if let Some(eq) = header.strip_prefix('=').and_then(|h| h.find('=')) {
                    header.strip_prefix('=').unwrap()[eq + 1..]
                        .trim()
                        .trim_matches(|c: char| c == '"' || c == '\'')
                        .to_owned()
                } else {
                    String::new()
                }
            } else {
                trim_single_newline_pair(body).to_owned()
            };
            return Some((key, value));
        }
    }
    let content = block.strip_prefix('=').unwrap_or(block).trim();
    if let Some(eq) = content.find('=') {
        let key = content[..eq]
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'');
        let val = content[eq + 1..]
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'');
        if !key.is_empty() {
            return Some((key.to_owned(), val.to_owned()));
        }
    }
    None
}

/// 解析单个 <tool_call> 标记段（不含收尾标签）：容错解析非标与残缺格式
/// （漏闭合 >、属性写法、换行等），准确提取函数名与参数键值对。
pub(crate) fn parse_markup_call(region: &str) -> Option<MarkupCall> {
    let rest = region.strip_prefix("<tool_call>")?.trim_start();
    let func_pos = rest.find("<function")?;
    let after_func = &rest[func_pos + "<function".len()..];

    let gt_pos = after_func.find('>');
    let next_param = after_func.find("<parameter");
    let nl_pos = after_func.find('\n');

    let (func_header, params_start) = match (gt_pos, next_param) {
        (Some(gt), Some(np)) if gt < np => (&after_func[..gt], &after_func[gt + 1..]),
        (Some(gt), None) => (&after_func[..gt], &after_func[gt + 1..]),
        _ => {
            let candidates = [next_param, nl_pos];
            let end = candidates
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(after_func.len());
            (&after_func[..end], &after_func[end..])
        }
    };

    let name = extract_func_name(func_header)?;
    let mut params = Vec::new();
    let mut cursor = params_start;

    while let Some(param_pos) = cursor.find("<parameter") {
        let after_tag = &cursor[param_pos + "<parameter".len()..];
        let close_tag = after_tag.find("</parameter>");
        let next_open = after_tag.find("<parameter");
        let func_close = after_tag.find("</function>");

        let end_candidates = [close_tag, next_open, func_close];
        let end_pos = end_candidates
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(after_tag.len());

        let block = &after_tag[..end_pos];
        if let Some((k, v)) = parse_param_block(block) {
            params.push((k, v));
        }

        if close_tag == Some(end_pos) {
            cursor = &after_tag[end_pos + "</parameter>".len()..];
        } else {
            cursor = &after_tag[end_pos..];
            if func_close == Some(end_pos) {
                break;
            }
        }
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
/// 当字符串拒绝反序列化。支持联合与可空声明（如 `["integer", "null"]`），支持别名匹配。
pub(crate) fn typed_value(schema: &Value, key: &str, raw: &str) -> Value {
    let prop = match schema["properties"].get(key) {
        Some(p) if !p.is_null() => p,
        _ => {
            const ALIAS_MAP: &[(&str, &str)] = &[
                ("block_until", "block_until_ms"),
                ("path", "file_path"),
                ("filepath", "file_path"),
                ("dir", "target_directory"),
            ];
            let canonical = ALIAS_MAP
                .iter()
                .find(|(alias, _)| *alias == key)
                .map(|(_, can)| *can);
            if let Some(can) = canonical
                && let Some(p) = schema["properties"].get(can)
            {
                p
            } else {
                &schema["properties"][key]
            }
        }
    };
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
