//! OpenAI ↔ 内部（Anthropic 形状）格式转换
//!
//! 策略：把 OpenAI 请求拼成 Anthropic Messages 形状的 JSON，再交给现有
//! `MessagesRequest` 反序列化 + 既有管线处理；响应侧把内部产出的 Anthropic
//! JSON / SSE 转码回 OpenAI 形状。所有凭证/缓存/计费逻辑原样复用。

use serde_json::{Value, json};

use crate::anthropic::map_model;

/// 归一化后是否为 GPT 系列（gpt-5.6-*）。OpenAI 原生端点仅放行 GPT。
pub fn is_gpt_model(model: &str) -> bool {
    map_model(model).is_some_and(|m| m.starts_with("gpt-"))
}

/// 把 OpenAI Chat Completions 请求（原始 JSON）转成 Anthropic Messages 形状 JSON。
///
/// 返回的 JSON 可直接 `serde_json::from_value::<MessagesRequest>()`。
pub fn chat_request_to_anthropic(req: &Value) -> Result<Value, String> {
    let model = req
        .get("model")
        .and_then(Value::as_str)
        .ok_or("缺少 model 字段")?;

    // max_completion_tokens 优先（OpenAI 新字段），回退 max_tokens，再回退默认。
    let max_tokens = req
        .get("max_completion_tokens")
        .or_else(|| req.get("max_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(4096) as i64;

    let stream = req.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let messages = req
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("缺少 messages 数组")?;

    let mut system_texts: Vec<String> = Vec::new();
    let mut out_messages: Vec<Value> = Vec::new();

    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" | "developer" => {
                if let Some(s) = content_to_plain_text(m.get("content")) {
                    system_texts.push(s);
                }
            }
            "tool" => {
                // OpenAI tool 结果 → Anthropic tool_result（归到 user）
                let tool_call_id = m
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let content = content_to_plain_text(m.get("content")).unwrap_or_default();
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": content,
                });
                push_or_merge(&mut out_messages, "user", vec![block]);
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = content_to_plain_text(m.get("content")) {
                    if !text.is_empty() {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                }
                // assistant.tool_calls → tool_use 块
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let id = c.get("id").and_then(Value::as_str).unwrap_or_default();
                        let func = c.get("function");
                        let name = func
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let args_str = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let input: Value =
                            serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                push_or_merge(&mut out_messages, "assistant", blocks);
            }
            _ => {
                // user（及未知角色归一化为 user）
                let blocks = user_content_to_blocks(m.get("content"));
                push_or_merge(&mut out_messages, "user", blocks);
            }
        }
    }

    if out_messages.is_empty() {
        return Err("messages 转换后为空".to_string());
    }

    let mut anthropic = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": stream,
        "messages": out_messages,
    });

    if !system_texts.is_empty() {
        anthropic["system"] = Value::String(system_texts.join("\n\n"));
    }

    if let Some(tools) = convert_tools(req.get("tools")) {
        anthropic["tools"] = tools;
    }
    if let Some(tc) = convert_tool_choice(req.get("tool_choice")) {
        anthropic["tool_choice"] = tc;
    }

    // reasoning_effort → thinking + output_config（GPT 的 reasoning 字段据此下发）
    if let Some(effort) = req.get("reasoning_effort").and_then(Value::as_str) {
        anthropic["thinking"] = json!({ "type": "enabled", "budget_tokens": 20000 });
        anthropic["output_config"] = json!({ "effort": effort });
    }

    Ok(anthropic)
}

/// 把 OpenAI Responses 请求（原始 JSON）转成 Anthropic Messages 形状 JSON。
///
/// Responses 的 `input` 可为字符串或 items 数组；`instructions` → system；
/// `max_output_tokens` → max_tokens；`reasoning.effort` → thinking/effort。
pub fn responses_request_to_anthropic(req: &Value) -> Result<Value, String> {
    let model = req
        .get("model")
        .and_then(Value::as_str)
        .ok_or("缺少 model 字段")?;
    let max_tokens = req
        .get("max_output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(4096);
    let stream = req.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut out_messages: Vec<Value> = Vec::new();
    match req.get("input") {
        Some(Value::String(s)) => {
            out_messages.push(json!({ "role": "user", "content": s }));
        }
        Some(Value::Array(items)) => {
            for it in items {
                let role = it.get("role").and_then(Value::as_str).unwrap_or("user");
                let role = if role == "system" || role == "developer" {
                    "user"
                } else {
                    role
                };
                let blocks = user_content_to_blocks(it.get("content"));
                push_or_merge(&mut out_messages, role, blocks);
            }
        }
        _ => return Err("缺少 input 字段".to_string()),
    }
    if out_messages.is_empty() {
        return Err("input 转换后为空".to_string());
    }

    let mut anthropic = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": stream,
        "messages": out_messages,
    });
    if let Some(instr) = req.get("instructions").and_then(Value::as_str) {
        anthropic["system"] = Value::String(instr.to_string());
    }
    if let Some(tools) = convert_tools(req.get("tools")) {
        anthropic["tools"] = tools;
    }
    if let Some(effort) = req
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    {
        anthropic["thinking"] = json!({ "type": "enabled", "budget_tokens": 20000 });
        anthropic["output_config"] = json!({ "effort": effort });
    }
    Ok(anthropic)
}

/// 在最后一条消息的末块注入 `cache_control`，模拟 OpenAI 自动前缀缓存。
///
/// OpenAI 对 ≥1024 token 的最长稳定前缀自动缓存；Anthropic 机制里，末块上的
/// 断点会把 system + 全部在先内容纳入可缓存前缀。多轮对话中每轮在自身末尾落一个
/// 断点，下一轮即命中到该断点为止的前缀（cache_read），新增部分记 cache_creation，
/// 与 OpenAI 自动缓存行为一致。小于 min-cacheable 的前缀由 cache_tracker 自然忽略。
pub fn inject_auto_cache_breakpoint(anthropic: &mut Value) {
    let Some(msgs) = anthropic.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(last) = msgs.last_mut() else { return };
    let content = match last.get_mut("content") {
        Some(c) => c,
        None => return,
    };
    // content 归一化为块数组
    if let Some(s) = content.as_str() {
        *content = json!([{ "type": "text", "text": s }]);
    }
    if let Some(arr) = content.as_array_mut() {
        if let Some(last_block) = arr.last_mut() {
            last_block["cache_control"] = json!({ "type": "ephemeral" });
        }
    }
}

/// 追加消息；若与上一条同角色则合并 content 数组（避免连续同角色触发上游校验）。
fn push_or_merge(out: &mut Vec<Value>, role: &str, mut blocks: Vec<Value>) {
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            // 把上一条 content 归一化成数组再追加
            let existing = last.get_mut("content").unwrap();
            let mut arr = value_content_to_block_array(existing);
            arr.append(&mut blocks);
            *existing = Value::Array(arr);
            return;
        }
    }
    out.push(json!({ "role": role, "content": Value::Array(blocks) }));
}

/// 把 content（string 或数组）归一化为块数组。
fn value_content_to_block_array(v: &Value) -> Vec<Value> {
    match v {
        Value::String(s) => vec![json!({ "type": "text", "text": s })],
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    }
}

/// 提取 content 的纯文本（string 直接取；数组拼接其中 text 部分）。
fn content_to_plain_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let mut buf = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    buf.push_str(t);
                }
            }
            Some(buf)
        }
        _ => None,
    }
}

/// user content → Anthropic 块数组（支持 text 与 image_url）。
fn user_content_to_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => vec![json!({ "type": "text", "text": s })],
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for p in parts {
                let ptype = p.get("type").and_then(Value::as_str).unwrap_or("");
                match ptype {
                    "text" | "input_text" => {
                        let t = p.get("text").and_then(Value::as_str).unwrap_or("");
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                    "image_url" | "input_image" => {
                        let url = p
                            .get("image_url")
                            .and_then(|u| u.get("url"))
                            .and_then(Value::as_str)
                            .or_else(|| p.get("image_url").and_then(Value::as_str))
                            .unwrap_or("");
                        if let Some(block) = image_url_to_block(url) {
                            blocks.push(block);
                        }
                    }
                    _ => {}
                }
            }
            if blocks.is_empty() {
                blocks.push(json!({ "type": "text", "text": "" }));
            }
            blocks
        }
        _ => vec![json!({ "type": "text", "text": "" })],
    }
}

/// image_url → Anthropic image 块（data: URL 拆 base64；http(s) 用 url source）。
fn image_url_to_block(url: &str) -> Option<Value> {
    if url.is_empty() {
        return None;
    }
    if let Some(rest) = url.strip_prefix("data:") {
        // data:<media_type>;base64,<data>
        if let Some((meta, data)) = rest.split_once(',') {
            let media_type = meta.split(';').next().unwrap_or("image/png");
            return Some(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            }));
        }
        return None;
    }
    Some(json!({
        "type": "image",
        "source": { "type": "url", "url": url },
    }))
}

/// OpenAI function tools → Anthropic tools。
fn convert_tools(tools: Option<&Value>) -> Option<Value> {
    let arr = tools?.as_array()?;
    let mut out = Vec::new();
    for t in arr {
        // {type:"function", function:{name, description, parameters}}
        let func = t.get("function").unwrap_or(t);
        let name = func.get("name").and_then(Value::as_str)?;
        let description = func
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let params = func
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        out.push(json!({
            "name": name,
            "description": description,
            "input_schema": params,
        }));
    }
    if out.is_empty() { None } else { Some(Value::Array(out)) }
}

/// OpenAI tool_choice → Anthropic tool_choice。
fn convert_tool_choice(tc: Option<&Value>) -> Option<Value> {
    match tc {
        Some(Value::String(s)) => match s.as_str() {
            "required" => Some(json!({ "type": "any" })),
            "auto" => Some(json!({ "type": "auto" })),
            _ => None, // "none" 等：不下发，走默认
        },
        Some(Value::Object(_)) => {
            // {type:"function", function:{name}}
            let name = tc?
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)?;
            Some(json!({ "type": "tool", "name": name }))
        }
        _ => None,
    }
}

/// Anthropic stop_reason → OpenAI finish_reason。
pub fn map_finish_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        "tool_use" => "tool_calls",
        "max_tokens" | "model_context_window_exceeded" => "length",
        "stop_sequence" => "stop",
        _ => "stop", // end_turn 及其它
    }
}

/// 由 Anthropic usage 计算 OpenAI usage（含缓存细分）。
///
/// OpenAI 的 `prompt_tokens` 含缓存部分；Anthropic 的 `input_tokens` 仅未缓存。
/// 故 prompt_tokens = 未缓存 + cache_read + cache_creation；
/// `cached_tokens`=cache_read，`cache_write_tokens`=cache_creation（GPT-5.6+）。
pub fn map_usage(anthropic_usage: &Value) -> Value {
    let inp = anthropic_usage
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let out = anthropic_usage
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_read = anthropic_usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_write = anthropic_usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let prompt_tokens = inp + cache_read + cache_write;
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": out,
        "total_tokens": prompt_tokens + out,
        "prompt_tokens_details": { "cached_tokens": cache_read },
        "cache_write_tokens": cache_write,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::MessagesRequest;

    #[test]
    fn gpt_gate() {
        assert!(is_gpt_model("gpt-5.6-terra"));
        assert!(is_gpt_model("gpt-4")); // 归一化到 terra
        assert!(!is_gpt_model("claude-sonnet-4-5"));
        assert!(!is_gpt_model("llama-3"));
    }

    #[test]
    fn chat_basic_roundtrips_to_messages_request() {
        let req = json!({
            "model": "gpt-5.6-terra",
            "max_completion_tokens": 128,
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "hi"}
            ],
            "reasoning_effort": "high"
        });
        let a = chat_request_to_anthropic(&req).unwrap();
        assert_eq!(a["model"], "gpt-5.6-terra");
        assert_eq!(a["max_tokens"], 128);
        assert_eq!(a["stream"], true);
        assert_eq!(a["system"], "You are helpful.");
        assert_eq!(a["thinking"]["type"], "enabled");
        assert_eq!(a["output_config"]["effort"], "high");
        // 关键：能被内部类型反序列化
        let mr: MessagesRequest = serde_json::from_value(a).unwrap();
        assert_eq!(mr.messages.len(), 1);
        assert_eq!(mr.messages[0].role, "user");
    }

    #[test]
    fn chat_tools_and_tool_choice() {
        let req = json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "weather?"}],
            "tools": [{"type": "function", "function": {
                "name": "get_weather", "description": "city weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }}],
            "tool_choice": "required"
        });
        let a = chat_request_to_anthropic(&req).unwrap();
        assert_eq!(a["tools"][0]["name"], "get_weather");
        assert_eq!(a["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(a["tool_choice"]["type"], "any");
        let _mr: MessagesRequest = serde_json::from_value(a).unwrap();
    }

    #[test]
    fn assistant_tool_calls_and_tool_result() {
        let req = json!({
            "model": "gpt-5.6-luna",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ]
        });
        let a = chat_request_to_anthropic(&req).unwrap();
        let msgs = a["messages"].as_array().unwrap();
        // user, assistant(tool_use), user(tool_result)
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["input"]["city"], "SF");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
        let _mr: MessagesRequest = serde_json::from_value(a).unwrap();
    }

    #[test]
    fn finish_reason_and_usage_mapping() {
        assert_eq!(map_finish_reason("end_turn"), "stop");
        assert_eq!(map_finish_reason("tool_use"), "tool_calls");
        assert_eq!(map_finish_reason("max_tokens"), "length");
        let u = map_usage(&json!({
            "input_tokens": 9, "output_tokens": 5,
            "cache_read_input_tokens": 100, "cache_creation_input_tokens": 20
        }));
        assert_eq!(u["prompt_tokens"], 129); // 9+100+20
        assert_eq!(u["completion_tokens"], 5);
        assert_eq!(u["total_tokens"], 134);
        assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 100);
        assert_eq!(u["cache_write_tokens"], 20);
    }

    #[test]
    fn responses_input_string_and_instructions() {
        let req = json!({
            "model": "gpt-5.6-terra",
            "instructions": "Be terse.",
            "input": "hello",
            "max_output_tokens": 64,
            "reasoning": {"effort": "low"}
        });
        let a = responses_request_to_anthropic(&req).unwrap();
        assert_eq!(a["system"], "Be terse.");
        assert_eq!(a["max_tokens"], 64);
        assert_eq!(a["output_config"]["effort"], "low");
        assert_eq!(a["messages"][0]["role"], "user");
        let _mr: MessagesRequest = serde_json::from_value(a).unwrap();
    }

    #[test]
    fn auto_cache_breakpoint_marks_last_block() {
        let mut a = chat_request_to_anthropic(&json!({
            "model": "gpt-5.6-terra",
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": "second"}
            ]
        })).unwrap();
        inject_auto_cache_breakpoint(&mut a);
        let msgs = a["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        let last_block = last["content"].as_array().unwrap().last().unwrap();
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
        // 仍能被内部类型反序列化（cache_control 合法）
        let _mr: MessagesRequest = serde_json::from_value(a).unwrap();
    }
}
