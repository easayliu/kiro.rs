//! OpenAI ↔ 内部（Anthropic 形状）格式转换
//!
//! 策略：把 OpenAI 请求拼成 Anthropic Messages 形状的 JSON，再交给现有
//! `MessagesRequest` 反序列化 + 既有管线处理；响应侧把内部产出的 Anthropic
//! JSON / SSE 转码回 OpenAI 形状。所有凭证/缓存/计费逻辑原样复用。

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::anthropic::map_model;

/// 归一化后是否为 GPT 系列（gpt-5.6-*）。OpenAI 原生端点仅放行 GPT。
pub fn is_gpt_model(model: &str) -> bool {
    map_model(model).is_some_and(|m| m.starts_with("gpt-"))
}

/// freeform（`type: "custom"`）工具在合成 schema 里承载原始载荷的属性名。
///
/// 上游 Kiro/Bedrock 的 toolSpecification 只接受 JSON Schema，无法表达 OpenAI 的
/// freeform 工具（入参是一整段原始文本/代码，由 `format.grammar` 约束）。桥接办法是
/// 声明成单字段 `{ input: string }` 的普通函数，出口再把该字段的字符串还原成
/// `custom_tool_call.input`。两端都以本常量为准。
pub const CUSTOM_TOOL_INPUT_KEY: &str = "input";

/// 请求转换结果：Anthropic 形状 JSON + 本轮声明为 freeform 的工具名集合。
///
/// `custom_tools` 必须一路带到响应转码：模型产出的 tool_use 若命中集合，出口要发
/// `custom_tool_call`（载荷字段 `input` 为原始文本）而非 `function_call`（载荷字段
/// `arguments` 为 JSON 串）；发错类型时客户端（codex CLI）按未知载荷丢弃，表现为
/// 「工具永不执行」。
pub struct ConvertedRequest {
    pub anthropic: Value,
    pub custom_tools: HashSet<String>,
}

/// 把 OpenAI Chat Completions 请求（原始 JSON）转成 Anthropic Messages 形状 JSON。
///
/// 返回的 JSON 可直接 `serde_json::from_value::<MessagesRequest>()`。
pub fn chat_request_to_anthropic(req: &Value) -> Result<ConvertedRequest, String> {
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

    let (tools, custom_tools) = convert_tools(req.get("tools"));
    if let Some(tools) = tools {
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

    Ok(ConvertedRequest { anthropic, custom_tools })
}

/// 把 OpenAI Responses 请求（原始 JSON）转成 Anthropic Messages 形状 JSON。
///
/// Responses 的 `input` 可为字符串或 items 数组；`instructions` → system；
/// `max_output_tokens` → max_tokens；`reasoning.effort` → thinking/effort。
pub fn responses_request_to_anthropic(req: &Value) -> Result<ConvertedRequest, String> {
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
                // Responses 的 input items 是带 type 的联合体；缺省按 message 处理。
                match it.get("type").and_then(Value::as_str).unwrap_or("message") {
                    // 上一轮的工具调用回放 → assistant 的 tool_use
                    //
                    // function_call 的 `arguments` 是 JSON 串，解析成对象；custom_tool_call
                    // 的 `input` 是 freeform 原文（codex 的 `exec` 工具里就是一整段 JS 代码），
                    // 解析必失败——必须原样包进合成字段，否则历史里每一次调用的入参都被清成
                    // `{}`，模型看不到自己上一步执行了什么。
                    "function_call" | "custom_tool_call" => {
                        let is_custom =
                            it.get("type").and_then(Value::as_str) == Some("custom_tool_call");
                        let call_id = it
                            .get("call_id")
                            .or_else(|| it.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let name = it.get("name").and_then(Value::as_str).unwrap_or_default();
                        let input: Value = if is_custom {
                            let raw = it.get("input").and_then(Value::as_str).unwrap_or_default();
                            json!({ CUSTOM_TOOL_INPUT_KEY: raw })
                        } else {
                            let args =
                                it.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                            serde_json::from_str(args).unwrap_or_else(|_| json!({}))
                        };
                        push_or_merge(
                            &mut out_messages,
                            "assistant",
                            vec![json!({
                                "type": "tool_use",
                                "id": call_id,
                                "name": name,
                                "input": input,
                            })],
                        );
                    }
                    // 工具执行结果 → user 的 tool_result
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = it
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        // output 可为纯字符串，也可为 content items 数组。
                        let content =
                            content_to_plain_text(it.get("output")).unwrap_or_default();
                        push_or_merge(
                            &mut out_messages,
                            "user",
                            vec![json!({
                                "type": "tool_result",
                                "tool_use_id": call_id,
                                "content": content,
                            })],
                        );
                    }
                    // reasoning 是上游加密回放件，Anthropic 形状无对应载体，丢弃。
                    "reasoning" => {}
                    _ => {
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
    let (tools, custom_tools) = convert_tools(req.get("tools"));
    if let Some(tools) = tools {
        anthropic["tools"] = tools;
    }
    if let Some(tc) = convert_tool_choice(req.get("tool_choice")) {
        anthropic["tool_choice"] = tc;
    }
    if let Some(effort) = req
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    {
        anthropic["thinking"] = json!({ "type": "enabled", "budget_tokens": 20000 });
        anthropic["output_config"] = json!({ "effort": effort });
    }
    Ok(ConvertedRequest { anthropic, custom_tools })
}

/// 在最后一条消息的末块注入 `cache_control`，模拟 OpenAI 自动前缀缓存。
///
/// OpenAI 对 ≥1024 token 的最长稳定前缀自动缓存；Anthropic 机制里，末块上的
/// 断点会把 system + 全部在先内容纳入可缓存前缀。多轮对话中每轮在自身末尾落一个
/// 断点，下一轮即命中到该断点为止的前缀（cache_read），新增部分记 cache_creation，
/// 与 OpenAI 自动缓存行为一致。小于 min-cacheable 的前缀由 cache_tracker 自然忽略。
///
/// TTL 用 `1h`：OpenAI GPT-5.6 缓存保底存活 ≥30 分钟（官方文档，`ttl` 唯一值 30m，
/// 是最小时长非上限）。Anthropic 无 30m 档，取 1h 覆盖该保底窗口，避免 5m 默认档
/// 过早判过期、在 5~30 分钟间隔时误报未命中。注意这只影响命中判定，不影响计价：
/// OpenAI 缓存写不加价，`official_price_usd` 的 GPT 分支两档写均按 1×（等同普通
/// input），不套 Anthropic 的 5m 1.25× / 1h 2×。
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
            last_block["cache_control"] = json!({ "type": "ephemeral", "ttl": "1h" });
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
                    // output_text：Responses 回放的 assistant 历史消息用这个 part 类型。
                    "text" | "input_text" | "output_text" => {
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

/// OpenAI tools → Anthropic tools，同时返回其中 freeform（`type: "custom"`）工具的名字集合。
///
/// 兼容三种声明形态：
/// - Chat Completions 函数：`{type:"function", function:{name, description, parameters}}`
/// - Responses 函数（扁平）：`{type:"function", name, description, parameters}`
/// - freeform 工具：`{type:"custom", name, description, format:{...}}`（Chat 侧套在 `custom` 里）
///
/// freeform 工具没有 `parameters`，其入参是受 `format.grammar` 约束的一整段原始文本。
/// 上游只吃 JSON Schema，故合成 `{ input: string }` 单字段 schema，并把「这是原文、不是
/// JSON」连同语法定义写进 description——否则模型只能看见一个空对象 schema，压根不知道
/// 该产出什么（codex 的 `exec` 工具即属此类，读文件全靠它）。
fn convert_tools(tools: Option<&Value>) -> (Option<Value>, HashSet<String>) {
    let mut custom_tools = HashSet::new();
    let Some(arr) = tools.and_then(Value::as_array) else {
        return (None, custom_tools);
    };
    let mut out = Vec::new();
    for t in arr {
        let ttype = t.get("type").and_then(Value::as_str).unwrap_or("function");
        let is_custom = ttype == "custom";
        // 内层规格：Chat 套在 function/custom 下，Responses 直接扁平在顶层。
        let spec = t
            .get("function")
            .or_else(|| t.get("custom"))
            .unwrap_or(t);
        // 无名工具（如 `{"type":"web_search"}`、`{"type":"image_generation"}`）跳过即可，
        // 不能连累整张表——早期实现在这里 `?` 直接返回 None，一个无名条目就让全部工具消失。
        let Some(name) = spec.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = spec.get("description").and_then(Value::as_str).unwrap_or("");

        let (description, schema) = if is_custom {
            custom_tools.insert(name.to_string());
            (
                custom_tool_description(description),
                custom_tool_schema(spec.get("format")),
            )
        } else {
            (
                description.to_string(),
                spec.get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            )
        };

        out.push(json!({
            "name": name,
            "description": description,
            "input_schema": schema,
        }));
    }
    let tools = if out.is_empty() { None } else { Some(Value::Array(out)) };
    (tools, custom_tools)
}

/// freeform 工具的语法定义写进 description 的长度上限（超出截断，避免撑爆工具描述）。
const CUSTOM_TOOL_GRAMMAR_MAX: usize = 4000;

/// 给 freeform 工具的描述追加调用约定说明。
fn custom_tool_description(original: &str) -> String {
    let note = format!(
        "This tool takes freeform input: put the ENTIRE call payload verbatim into the \
         `{CUSTOM_TOOL_INPUT_KEY}` string field. Do not wrap it in JSON, do not add fields."
    );
    if original.trim().is_empty() {
        note
    } else {
        format!("{original}\n\n{note}")
    }
}

/// 由 freeform 工具的 `format` 合成 `{ input: string }` schema（语法定义写进属性描述）。
fn custom_tool_schema(format: Option<&Value>) -> Value {
    let mut desc = String::from(
        "The complete raw payload of this tool call, passed through verbatim as plain text \
         (NOT a JSON object).",
    );
    if let Some(f) = format {
        match f.get("type").and_then(Value::as_str) {
            Some("grammar") => {
                let syntax = f.get("syntax").and_then(Value::as_str).unwrap_or("grammar");
                if let Some(def) = f.get("definition").and_then(Value::as_str) {
                    let def = match def.char_indices().nth(CUSTOM_TOOL_GRAMMAR_MAX) {
                        Some((i, _)) => &def[..i],
                        None => def,
                    };
                    desc.push_str(&format!(
                        " It must conform to the following {syntax} grammar:\n{def}"
                    ));
                }
            }
            _ => desc.push_str(" It is free-form text."),
        }
    }
    json!({
        "type": "object",
        "properties": { CUSTOM_TOOL_INPUT_KEY: { "type": "string", "description": desc } },
        "required": [CUSTOM_TOOL_INPUT_KEY],
    })
}

/// 从模型产出的 tool_use 入参 JSON 串里取回 freeform 原文。
///
/// 正常路径是合成 schema 的 `input` 字段；模型偶尔不按 schema 走（直接给字符串、或换了
/// 字段名），则退回原串透传——总比丢空强。
pub fn custom_tool_input_from_args(args: &str) -> String {
    match serde_json::from_str::<Value>(args) {
        Ok(Value::Object(map)) => match map.get(CUSTOM_TOOL_INPUT_KEY) {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => args.to_string(),
        },
        Ok(Value::String(s)) => s,
        _ => args.to_string(),
    }
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
/// 故 prompt_tokens = 未缓存 + cache_read + cache_creation。
///
/// OpenAI 官方口径只有「缓存读」：缓存写是请求的隐式副作用、不加价也不上报，
/// `prompt_tokens_details` 中仅有 `cached_tokens`（= cache_read），没有任何写字段。
/// 因此 cache_creation 不单列，直接留在 `prompt_tokens - cached_tokens` 里按 1× input 计。
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
        "prompt_tokens_details": {
            "cached_tokens": cache_read,
        },
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
        let a = chat_request_to_anthropic(&req).unwrap().anthropic;
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
        let a = chat_request_to_anthropic(&req).unwrap().anthropic;
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
        let a = chat_request_to_anthropic(&req).unwrap().anthropic;
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
        // OpenAI 官方无缓存写字段：cache_creation(20) 融进 prompt_tokens 的未缓存部分。
        assert!(
            u["prompt_tokens_details"].get("cache_write_tokens").is_none(),
            "OpenAI 官方 prompt_tokens_details 不含缓存写字段"
        );
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
        let a = responses_request_to_anthropic(&req).unwrap().anthropic;
        assert_eq!(a["system"], "Be terse.");
        assert_eq!(a["max_tokens"], 64);
        assert_eq!(a["output_config"]["effort"], "low");
        assert_eq!(a["messages"][0]["role"], "user");
        let _mr: MessagesRequest = serde_json::from_value(a).unwrap();
    }

    /// codex CLI 的多轮形态：message / function_call / function_call_output / reasoning
    /// 混在同一个 input 数组里回放，须还原成 tool_use + tool_result 的对话。
    #[test]
    fn responses_input_items_restore_tool_loop() {
        let req = json!({
            "model": "gpt-5.6-terra",
            "instructions": "You are a coding agent.",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "reasoning", "summary": [], "encrypted_content": "opaque"},
                {"type": "function_call", "name": "shell", "call_id": "call_1",
                 "arguments": "{\"command\":[\"ls\"]}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.txt\nb.txt"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "two files"}]},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "thanks"}]}
            ],
            "tools": [
                {"type": "function", "name": "shell", "description": "run",
                 "parameters": {"type": "object", "properties": {}}}
            ]
        });
        let a = responses_request_to_anthropic(&req).unwrap().anthropic;
        let msgs = a["messages"].as_array().unwrap();

        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["text"], "list files");
        // reasoning 被丢弃，不该塞出一条空消息
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "call_1");
        assert_eq!(msgs[1]["content"][0]["name"], "shell");
        assert_eq!(msgs[1]["content"][0]["input"]["command"][0], "ls");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(msgs[2]["content"][0]["content"], "a.txt\nb.txt");
        // assistant 历史用 output_text part，须保留文本
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(msgs[3]["content"][0]["text"], "two files");
        assert_eq!(msgs[4]["role"], "user");

        // Responses 的 tools 是扁平结构（name/parameters 不套 function）
        assert_eq!(a["tools"][0]["name"], "shell");
        assert_eq!(a["tools"][0]["input_schema"]["type"], "object");
        let _mr: MessagesRequest = serde_json::from_value(a).unwrap();
    }

    /// codex CLI 现役形态：唯一的执行工具 `exec` 是 freeform（`type:"custom"`），
    /// 入参是一整段 JS 代码而非 JSON。声明须桥接成 `{input: string}`，回放须原样保留。
    #[test]
    fn responses_custom_tool_declaration_and_replay() {
        let code = "const r = await tools.exec_command({\n  cmd: \"sed -n '1,40p' src/main.rs\",\n  workdir: \"/w\"\n});\ntext(r.output);\n";
        let req = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "读一下 main.rs"}]},
                {"type": "custom_tool_call", "name": "exec", "call_id": "call_1", "input": code},
                {"type": "custom_tool_call_output", "call_id": "call_1",
                 "output": [{"type": "input_text", "text": "fn main() {}"}]}
            ],
            "tools": [
                {"type": "custom", "name": "exec", "description": "Run a script.",
                 "format": {"type": "grammar", "syntax": "lark", "definition": "start: CODE"}}
            ]
        });
        let converted = responses_request_to_anthropic(&req).unwrap();
        assert!(converted.custom_tools.contains("exec"));
        let a = converted.anthropic;

        // 声明：合成单字段 string schema，语法定义与调用约定进 description
        let tool = &a["tools"][0];
        assert_eq!(tool["name"], "exec");
        assert_eq!(tool["input_schema"]["properties"]["input"]["type"], "string");
        assert_eq!(tool["input_schema"]["required"][0], "input");
        assert!(tool["input_schema"]["properties"]["input"]["description"]
            .as_str()
            .unwrap()
            .contains("start: CODE"));
        let desc = tool["description"].as_str().unwrap();
        assert!(desc.starts_with("Run a script."));
        assert!(desc.contains("freeform"));

        // 回放：JS 原文原样进 input 字段（旧实现在这里 JSON parse 失败、清成 {}）
        let msgs = a["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["name"], "exec");
        assert_eq!(msgs[1]["content"][0]["input"]["input"], code);
        assert_eq!(msgs[2]["content"][0]["content"], "fn main() {}");
        let _mr: MessagesRequest = serde_json::from_value(a).unwrap();
    }

    /// 无名工具（`{"type":"web_search"}` 等）只跳过自己，不能让整张 tools 表消失。
    #[test]
    fn nameless_tool_does_not_drop_the_others() {
        let req = json!({
            "model": "gpt-5.6-terra",
            "input": "hi",
            "tools": [
                {"type": "web_search"},
                {"type": "function", "name": "shell", "description": "run",
                 "parameters": {"type": "object", "properties": {}}}
            ],
            "tool_choice": "required"
        });
        let a = responses_request_to_anthropic(&req).unwrap().anthropic;
        let tools = a["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "shell");
        // Responses 侧也要转 tool_choice（此前只有 chat 路径转）
        assert_eq!(a["tool_choice"]["type"], "any");
    }

    /// Chat Completions 的 custom 工具套在 `custom` 对象里，同样要被识别。
    #[test]
    fn chat_custom_tool_is_recognized() {
        let req = json!({
            "model": "gpt-5.6-terra",
            "messages": [{"role": "user", "content": "go"}],
            "tools": [{"type": "custom", "custom": {"name": "exec", "description": "",
                       "format": {"type": "text"}}}]
        });
        let converted = chat_request_to_anthropic(&req).unwrap();
        assert!(converted.custom_tools.contains("exec"));
        assert_eq!(
            converted.anthropic["tools"][0]["input_schema"]["properties"]["input"]["type"],
            "string"
        );
    }

    #[test]
    fn custom_tool_input_extraction() {
        // 正常路径：取合成字段
        assert_eq!(custom_tool_input_from_args(r#"{"input":"ls -la"}"#), "ls -la");
        // 模型没按 schema 走：原串透传，不丢内容
        assert_eq!(custom_tool_input_from_args(r#"{"cmd":"ls"}"#), r#"{"cmd":"ls"}"#);
        assert_eq!(custom_tool_input_from_args("raw text"), "raw text");
        assert_eq!(custom_tool_input_from_args(""), "");
    }

    /// function_call_output 的 output 也可为 content items 数组。
    #[test]
    fn responses_function_call_output_accepts_content_items() {
        let req = json!({
            "model": "gpt-5.6-terra",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                {"type": "function_call", "name": "f", "call_id": "c1", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1",
                 "output": [{"type": "input_text", "text": "done"}]}
            ]
        });
        let a = responses_request_to_anthropic(&req).unwrap().anthropic;
        let msgs = a["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["content"][0]["content"], "done");
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
        })).unwrap().anthropic;
        inject_auto_cache_breakpoint(&mut a);
        let msgs = a["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        let last_block = last["content"].as_array().unwrap().last().unwrap();
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
        // TTL 1h：覆盖 OpenAI ≥30min 保底窗口
        assert_eq!(last_block["cache_control"]["ttl"], "1h");
        // 仍能被内部类型反序列化（cache_control 合法）
        let _mr: MessagesRequest = serde_json::from_value(a).unwrap();
    }
}
