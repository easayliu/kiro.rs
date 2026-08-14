//! 把内部管线产出的 Anthropic 响应（JSON / SSE）转码回 OpenAI 形状。
//!
//! 复用现有 `handle_messages` 的完整输出（含缓存/计费/截断处理），只在最外层
//! 做格式翻译：非流式解析一次 JSON 重组；流式解析 Anthropic SSE 事件流后按
//! OpenAI `chat.completion.chunk` 协议逐块下发。

use std::collections::HashMap;

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::stream::{self, StreamExt};
use serde_json::{Value, json};

use super::convert::{map_finish_reason, map_usage};

const BODY_LIMIT: usize = 64 * 1024 * 1024;

/// 非流式：Anthropic message JSON → OpenAI `chat.completion`。
pub async fn chat_nonstream(resp: Response, id: String, model: String, created: i64) -> Response {
    let (status, body) = match read_body(resp).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if status != StatusCode::OK {
        return openai_error_from_anthropic(status, &body);
    }

    let (text, reasoning, tool_calls) = extract_content_blocks(&body);
    let finish = map_finish_reason(body.get("stop_reason").and_then(Value::as_str).unwrap_or("end_turn"));
    let usage = map_usage(body.get("usage").unwrap_or(&Value::Null));

    let mut message = json!({ "role": "assistant", "content": text });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["content"] = Value::Null;
        message["tool_calls"] = Value::Array(tool_calls);
    }

    let out = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": usage,
    });
    json_response(StatusCode::OK, &out)
}

/// 非流式：Anthropic message JSON → OpenAI Responses `response` 对象。
pub async fn responses_nonstream(resp: Response, id: String, model: String, created: i64) -> Response {
    let (status, body) = match read_body(resp).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if status != StatusCode::OK {
        return openai_error_from_anthropic(status, &body);
    }

    let (text, _reasoning, tool_calls) = extract_content_blocks(&body);
    let usage_a = body.get("usage").unwrap_or(&Value::Null);
    let inp = usage_a.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
    let cache_read = usage_a
        .get("cache_read_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_write = usage_a
        .get("cache_creation_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let out_tok = usage_a.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
    let input_total = inp + cache_read + cache_write;

    let mut output_items = Vec::new();
    output_items.push(json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text }],
    }));
    for tc in tool_calls {
        let func = tc.get("function").cloned().unwrap_or(Value::Null);
        output_items.push(json!({
            "type": "function_call",
            "call_id": tc.get("id").cloned().unwrap_or(Value::Null),
            "name": func.get("name").cloned().unwrap_or(Value::Null),
            "arguments": func.get("arguments").cloned().unwrap_or(Value::Null),
        }));
    }

    let out = json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "model": model,
        "status": "completed",
        "output": output_items,
        // OpenAI 官方 input_tokens_details 只有 cached_tokens（缓存写不上报、不加价），
        // cache_creation 留在 input_tokens 的未缓存部分里。
        "usage": {
            "input_tokens": input_total,
            "input_tokens_details": {
                "cached_tokens": cache_read,
            },
            "output_tokens": out_tok,
            "total_tokens": input_total + out_tok,
        },
    });
    json_response(StatusCode::OK, &out)
}

/// 流式：Anthropic SSE → OpenAI `chat.completion.chunk` SSE。
///
/// 调用方须保证 `resp` 为 200（非 200 请走 `chat_nonstream` 以复用错误转码）。
pub fn chat_stream(
    resp: Response,
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
) -> Response {
    let upstream = resp.into_body().into_data_stream();
    let state = ChatStreamState {
        id,
        model,
        created,
        include_usage,
        buffer: Vec::new(),
        role_sent: false,
        block_types: HashMap::new(),
        tool_index: HashMap::new(),
        next_tool_index: 0,
        finish_reason: None,
        usage: None,
        done: false,
    };

    let out = stream::unfold((upstream, state), |(mut up, mut st)| async move {
        if st.done {
            return None;
        }
        loop {
            match up.next().await {
                Some(Ok(chunk)) => {
                    st.buffer.extend_from_slice(&chunk);
                    let mut emit = String::new();
                    for (ev, data) in drain_sse_events(&mut st.buffer) {
                        st.handle_event(&ev, &data, &mut emit);
                    }
                    if emit.is_empty() {
                        continue; // 本 chunk 未凑出完整事件，继续读
                    }
                    return Some((Ok::<Bytes, std::convert::Infallible>(Bytes::from(emit)), (up, st)));
                }
                Some(Err(_)) | None => {
                    // 上游流结束/出错：补最终块 + [DONE]
                    let mut emit = String::new();
                    st.finalize(&mut emit);
                    st.done = true;
                    return Some((Ok(Bytes::from(emit)), (up, st)));
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(out))
        .unwrap()
}

struct ChatStreamState {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    buffer: Vec<u8>,
    role_sent: bool,
    /// anthropic block index → 块类型（text/thinking/tool_use）
    block_types: HashMap<i64, String>,
    /// anthropic block index → openai tool_calls 下标
    tool_index: HashMap<i64, i64>,
    next_tool_index: i64,
    finish_reason: Option<String>,
    usage: Option<Value>,
    done: bool,
}

impl ChatStreamState {
    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        let obj = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        format!("data: {}\n\n", obj)
    }

    fn ensure_role(&mut self, out: &mut String) {
        if !self.role_sent {
            self.role_sent = true;
            out.push_str(&self.chunk(json!({ "role": "assistant", "content": "" }), None));
        }
    }

    fn handle_event(&mut self, event: &str, data: &Value, out: &mut String) {
        match event {
            "content_block_start" => {
                let idx = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                let block = data.get("content_block");
                let btype = block
                    .and_then(|b| b.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    .to_string();
                if btype == "tool_use" {
                    let oi = self.next_tool_index;
                    self.next_tool_index += 1;
                    self.tool_index.insert(idx, oi);
                    self.ensure_role(out);
                    let id = block.and_then(|b| b.get("id")).and_then(Value::as_str).unwrap_or("");
                    let name = block.and_then(|b| b.get("name")).and_then(Value::as_str).unwrap_or("");
                    out.push_str(&self.chunk(
                        json!({ "tool_calls": [{
                            "index": oi, "id": id, "type": "function",
                            "function": { "name": name, "arguments": "" }
                        }]}),
                        None,
                    ));
                }
                self.block_types.insert(idx, btype);
            }
            "content_block_delta" => {
                let idx = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                let delta = match data.get("delta") {
                    Some(d) => d,
                    None => return,
                };
                let dtype = delta.get("type").and_then(Value::as_str).unwrap_or("");
                match dtype {
                    "text_delta" => {
                        let t = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        self.ensure_role(out);
                        out.push_str(&self.chunk(json!({ "content": t }), None));
                    }
                    "thinking_delta" => {
                        let t = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        self.ensure_role(out);
                        out.push_str(&self.chunk(json!({ "reasoning_content": t }), None));
                    }
                    "input_json_delta" => {
                        let pj = delta.get("partial_json").and_then(Value::as_str).unwrap_or("");
                        if let Some(&oi) = self.tool_index.get(&idx) {
                            out.push_str(&self.chunk(
                                json!({ "tool_calls": [{
                                    "index": oi, "function": { "arguments": pj }
                                }]}),
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = data.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                    self.finish_reason = Some(map_finish_reason(sr).to_string());
                }
                if let Some(u) = data.get("usage") {
                    self.usage = Some(map_usage(u));
                }
            }
            _ => {}
        }
    }

    fn finalize(&mut self, out: &mut String) {
        self.ensure_role(out);
        let finish = self.finish_reason.clone().unwrap_or_else(|| "stop".to_string());
        out.push_str(&self.chunk(json!({}), Some(&finish)));
        if self.include_usage {
            if let Some(usage) = &self.usage {
                let obj = json!({
                    "id": self.id,
                    "object": "chat.completion.chunk",
                    "created": self.created,
                    "model": self.model,
                    "choices": [],
                    "usage": usage,
                });
                out.push_str(&format!("data: {}\n\n", obj));
            }
        }
        out.push_str("data: [DONE]\n\n");
    }
}

/// 流式：Anthropic SSE → OpenAI Responses 事件协议
/// （`response.created` / `response.output_item.added` / `response.output_text.delta` /
///  `response.output_item.done` / `response.completed`）。
///
/// **`response.output_item.done` 是必发项**：Responses 客户端（如 codex CLI）只从
/// `output_item.done` 收集本轮产出，`response.completed` 里的 `output` 数组不读、
/// `output_text.delta` 只用于实时显示、`function_call_arguments.delta` 直接忽略。
/// 少发 `output_item.done` 的表现就是「请求成功但回复为空、工具永不执行」。
///
/// 调用方须保证 `resp` 为 200（非 200 请走 `responses_nonstream`）。
pub fn responses_stream(resp: Response, id: String, model: String, created: i64) -> Response {
    let upstream = resp.into_body().into_data_stream();
    let state = RespStreamState {
        id,
        model,
        created,
        buffer: Vec::new(),
        created_sent: false,
        text_blocks: HashMap::new(),
        tool_blocks: HashMap::new(),
        output_items: Vec::new(),
        next_output_index: 0,
        usage: None,
        done: false,
    };

    let out = stream::unfold((upstream, state), |(mut up, mut st)| async move {
        if st.done {
            return None;
        }
        loop {
            match up.next().await {
                Some(Ok(chunk)) => {
                    st.buffer.extend_from_slice(&chunk);
                    let mut emit = String::new();
                    for (ev, data) in drain_sse_events(&mut st.buffer) {
                        st.handle_event(&ev, &data, &mut emit);
                    }
                    if emit.is_empty() {
                        continue;
                    }
                    return Some((
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(emit)),
                        (up, st),
                    ));
                }
                Some(Err(_)) | None => {
                    let mut emit = String::new();
                    st.finalize(&mut emit);
                    st.done = true;
                    return Some((Ok(Bytes::from(emit)), (up, st)));
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(out))
        .unwrap()
}

/// 流中一个 anthropic text 块 → Responses 的 message item。
struct TextBlock {
    output_index: i64,
    item_id: String,
    text: String,
}

/// 流中一个 anthropic tool_use 块 → Responses 的 function_call item。
struct ToolBlock {
    output_index: i64,
    item_id: String,
    call_id: String,
    name: String,
    /// 累积的 `input_json_delta`，收尾时作为 `arguments` 整串下发。
    arguments: String,
}

struct RespStreamState {
    id: String,
    model: String,
    created: i64,
    buffer: Vec<u8>,
    created_sent: bool,
    /// anthropic text block index → 该块的 message item 状态
    text_blocks: HashMap<i64, TextBlock>,
    /// anthropic tool_use block index → 该块的 function_call item 状态
    tool_blocks: HashMap<i64, ToolBlock>,
    /// 已 `output_item.done` 的 item（按下发顺序），供 completed 的 output 数组复述
    output_items: Vec<Value>,
    next_output_index: i64,
    usage: Option<Value>,
    done: bool,
}

impl RespStreamState {
    fn event(&self, name: &str, data: Value) -> String {
        format!("event: {}\ndata: {}\n\n", name, data)
    }

    fn ensure_created(&mut self, out: &mut String) {
        if !self.created_sent {
            self.created_sent = true;
            out.push_str(&self.event(
                "response.created",
                json!({
                    "type": "response.created",
                    "response": {
                        "id": self.id, "object": "response", "created_at": self.created,
                        "model": self.model, "status": "in_progress", "output": []
                    }
                }),
            ));
        }
    }

    /// 开一个 message item（text 块），下发 `output_item.added`。
    ///
    /// anthropic 的 `content_block_start` 有时不带 text 块（或被上游省略），
    /// 故 `text_delta` 也会兜底调用本函数，保证 item 一定被开出来。
    fn open_text_block(&mut self, idx: i64, out: &mut String) {
        if self.text_blocks.contains_key(&idx) {
            return;
        }
        self.ensure_created(out);
        let oi = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("msg_{oi}");
        out.push_str(&self.event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": oi,
                "item": {
                    "id": item_id, "type": "message", "role": "assistant",
                    "status": "in_progress", "content": []
                }
            }),
        ));
        self.text_blocks.insert(idx, TextBlock { output_index: oi, item_id, text: String::new() });
    }

    /// text 块收尾：`output_text.done` + `output_item.done`（message）。
    ///
    /// 空文本不下发 item：只出工具调用的那一轮不该往对话里塞一条空 assistant 消息。
    fn close_text_block(&mut self, idx: i64, out: &mut String) {
        let Some(blk) = self.text_blocks.remove(&idx) else { return };
        if blk.text.is_empty() {
            return;
        }
        out.push_str(&self.event(
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "item_id": blk.item_id, "output_index": blk.output_index,
                "content_index": 0, "text": blk.text
            }),
        ));
        let item = json!({
            "id": blk.item_id, "type": "message", "role": "assistant", "status": "completed",
            "content": [{ "type": "output_text", "text": blk.text }]
        });
        out.push_str(&self.event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": blk.output_index, "item": item
            }),
        ));
        self.output_items.push(item);
    }

    /// tool_use 块收尾：`function_call_arguments.done` + `output_item.done`（function_call）。
    fn close_tool_block(&mut self, idx: i64, out: &mut String) {
        let Some(blk) = self.tool_blocks.remove(&idx) else { return };
        // 空 arguments 补成 `{}`：Responses 侧 arguments 是必填 JSON 字符串。
        let args = if blk.arguments.is_empty() { "{}".to_string() } else { blk.arguments };
        out.push_str(&self.event(
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": blk.item_id, "output_index": blk.output_index, "arguments": args
            }),
        ));
        let item = json!({
            "id": blk.item_id, "type": "function_call", "status": "completed",
            "call_id": blk.call_id, "name": blk.name, "arguments": args
        });
        out.push_str(&self.event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": blk.output_index, "item": item
            }),
        ));
        self.output_items.push(item);
    }

    fn handle_event(&mut self, event: &str, data: &Value, out: &mut String) {
        match event {
            "content_block_start" => {
                let idx = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                let block = data.get("content_block");
                match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                    Some("tool_use") => {
                        self.ensure_created(out);
                        let call_id = block
                            .and_then(|b| b.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .and_then(|b| b.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let oi = self.next_output_index;
                        self.next_output_index += 1;
                        let item_id = format!("fc_{oi}");
                        out.push_str(&self.event(
                            "response.output_item.added",
                            json!({
                                "type": "response.output_item.added",
                                "output_index": oi,
                                "item": {
                                    "id": item_id, "type": "function_call",
                                    "call_id": call_id, "name": name, "arguments": ""
                                }
                            }),
                        ));
                        self.tool_blocks.insert(
                            idx,
                            ToolBlock {
                                output_index: oi,
                                item_id,
                                call_id,
                                name,
                                arguments: String::new(),
                            },
                        );
                    }
                    Some("text") => self.open_text_block(idx, out),
                    _ => {}
                }
            }
            "content_block_delta" => {
                let idx = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                let delta = match data.get("delta") {
                    Some(d) => d,
                    None => return,
                };
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        let t = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        self.open_text_block(idx, out);
                        let Some(blk) = self.text_blocks.get_mut(&idx) else { return };
                        blk.text.push_str(t);
                        let (item_id, oi) = (blk.item_id.clone(), blk.output_index);
                        out.push_str(&self.event(
                            "response.output_text.delta",
                            json!({
                                "type": "response.output_text.delta",
                                "item_id": item_id, "output_index": oi, "content_index": 0,
                                "delta": t
                            }),
                        ));
                    }
                    "input_json_delta" => {
                        let pj = delta.get("partial_json").and_then(Value::as_str).unwrap_or("");
                        let Some(blk) = self.tool_blocks.get_mut(&idx) else { return };
                        blk.arguments.push_str(pj);
                        let (item_id, oi) = (blk.item_id.clone(), blk.output_index);
                        out.push_str(&self.event(
                            "response.function_call_arguments.delta",
                            json!({
                                "type": "response.function_call_arguments.delta",
                                "item_id": item_id, "output_index": oi, "delta": pj
                            }),
                        ));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let idx = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                self.close_text_block(idx, out);
                self.close_tool_block(idx, out);
            }
            "message_delta" => {
                if let Some(u) = data.get("usage") {
                    self.usage = Some(u.clone());
                }
            }
            _ => {}
        }
    }

    fn finalize(&mut self, out: &mut String) {
        self.ensure_created(out);
        // 上游断流/缺 content_block_stop 时兜底收尾，避免已产出的内容丢失。
        let mut open: Vec<i64> = self.text_blocks.keys().chain(self.tool_blocks.keys()).copied().collect();
        open.sort_unstable();
        for idx in open {
            self.close_text_block(idx, out);
            self.close_tool_block(idx, out);
        }
        let usage_a = self.usage.clone().unwrap_or(Value::Null);
        let inp = usage_a.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
        let cache_read = usage_a
            .get("cache_read_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let cache_write = usage_a
            .get("cache_creation_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let out_tok = usage_a.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
        let input_total = inp + cache_read + cache_write;

        // 与已下发的 output_item.done 保持一致（客户端不读这里，仅为协议完整性）。
        let output = self.output_items.clone();

        out.push_str(&self.event(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "id": self.id, "object": "response", "created_at": self.created,
                    "model": self.model, "status": "completed", "output": output,
                    // 同 responses 非流式：官方只有 cached_tokens，无缓存写字段。
                    "usage": {
                        "input_tokens": input_total,
                        "input_tokens_details": {
                            "cached_tokens": cache_read
                        },
                        "output_tokens": out_tok,
                        "total_tokens": input_total + out_tok
                    }
                }
            }),
        ));
    }
}

/// 从缓冲区切出完整的 Anthropic SSE 事件（以空行 `\n\n` 分隔），返回 (event, data_json)。
fn drain_sse_events(buffer: &mut Vec<u8>) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    loop {
        // 找事件分隔（\n\n）
        let pos = buffer.windows(2).position(|w| w == b"\n\n");
        let end = match pos {
            Some(p) => p,
            None => break,
        };
        let block = String::from_utf8_lossy(&buffer[..end]).to_string();
        buffer.drain(..end + 2);

        let mut event_name = String::new();
        let mut data_line = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event_name = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                data_line.push_str(v.trim());
            }
        }
        if data_line.is_empty() {
            continue;
        }
        if let Ok(data) = serde_json::from_str::<Value>(&data_line) {
            events.push((event_name, data));
        }
    }
    events
}

/// 提取 Anthropic content 数组的 (纯文本, 思考文本, tool_calls)。
fn extract_content_blocks(body: &Value) -> (String, String, Vec<Value>) {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut ti = 0;
    if let Some(blocks) = body.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("thinking") => {
                    reasoning.push_str(b.get("thinking").and_then(Value::as_str).unwrap_or(""))
                }
                Some("tool_use") => {
                    let args = b.get("input").cloned().unwrap_or_else(|| json!({}));
                    tool_calls.push(json!({
                        "index": ti,
                        "id": b.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {
                            "name": b.get("name").cloned().unwrap_or(Value::Null),
                            "arguments": serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string()),
                        }
                    }));
                    ti += 1;
                }
                _ => {}
            }
        }
    }
    (text, reasoning, tool_calls)
}

async fn read_body(resp: Response) -> Result<(StatusCode, Value), Response> {
    let status = resp.status();
    let bytes = match axum::body::to_bytes(resp.into_body(), BODY_LIMIT).await {
        Ok(b) => b,
        Err(_) => {
            return Err(json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &openai_error_obj("读取内部响应体失败", "internal_error"),
            ));
        }
    };
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok((status, value))
}

fn openai_error_from_anthropic(status: StatusCode, body: &Value) -> Response {
    let (msg, etype) = body
        .get("error")
        .map(|e| {
            (
                e.get("message").and_then(Value::as_str).unwrap_or("上游错误").to_string(),
                e.get("type").and_then(Value::as_str).unwrap_or("api_error").to_string(),
            )
        })
        .unwrap_or_else(|| ("上游错误".to_string(), "api_error".to_string()));
    json_response(status, &openai_error_obj(&msg, &etype))
}

pub fn openai_error_obj(message: &str, etype: &str) -> Value {
    json!({ "error": { "message": message, "type": etype, "code": Value::Null } })
}

pub fn json_response(status: StatusCode, body: &Value) -> Response {
    (status, axum::Json(body.clone())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_state() -> ChatStreamState {
        ChatStreamState {
            id: "chatcmpl-x".into(),
            model: "gpt-5.6-terra".into(),
            created: 0,
            include_usage: true,
            buffer: Vec::new(),
            role_sent: false,
            block_types: HashMap::new(),
            tool_index: HashMap::new(),
            next_tool_index: 0,
            finish_reason: None,
            usage: None,
            done: false,
        }
    }

    fn feed(st: &mut ChatStreamState, sse: &str) -> String {
        st.buffer.extend_from_slice(sse.as_bytes());
        let mut out = String::new();
        for (ev, data) in drain_sse_events(&mut st.buffer) {
            st.handle_event(&ev, &data, &mut out);
        }
        out
    }

    #[test]
    fn text_stream_maps_to_chat_chunks() {
        let mut st = new_state();
        let mut all = String::new();
        all.push_str(&feed(&mut st, "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n"));
        all.push_str(&feed(&mut st, "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n"));
        all.push_str(&feed(&mut st, "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n"));
        all.push_str(&feed(&mut st, "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":9,\"output_tokens\":2}}\n\n"));
        let mut fin = String::new();
        st.finalize(&mut fin);
        all.push_str(&fin);

        // 首块带 role
        assert!(all.contains("\"role\":\"assistant\""));
        assert!(all.contains("\"content\":\"Hel\""));
        assert!(all.contains("\"content\":\"lo\""));
        assert!(all.contains("\"finish_reason\":\"stop\""));
        // include_usage → 末尾带 usage chunk
        assert!(all.contains("\"prompt_tokens\":9"));
        assert!(all.trim_end().ends_with("data: [DONE]"));
    }

    #[test]
    fn tool_use_stream_maps_to_tool_calls() {
        let mut st = new_state();
        let mut all = String::new();
        all.push_str(&feed(&mut st, "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\n"));
        all.push_str(&feed(&mut st, "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n"));
        all.push_str(&feed(&mut st, "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"SF\\\"}\"}}\n\n"));
        all.push_str(&feed(&mut st, "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n"));
        let mut fin = String::new();
        st.finalize(&mut fin);
        all.push_str(&fin);

        assert!(all.contains("\"tool_calls\""));
        assert!(all.contains("\"name\":\"get_weather\""));
        assert!(all.contains("\"id\":\"toolu_1\""));
        assert!(all.contains("\"arguments\":\"{\\\"city\\\":\""));
        assert!(all.contains("\"finish_reason\":\"tool_calls\""));
    }

    #[test]
    fn extract_blocks_for_nonstream() {
        let body = json!({
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "t1", "name": "f", "input": {"a": 1}}
            ]
        });
        let (text, _r, calls) = extract_content_blocks(&body);
        assert_eq!(text, "hi");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "f");
        assert_eq!(calls[0]["function"]["arguments"], "{\"a\":1}");
    }

    fn new_resp_state() -> RespStreamState {
        RespStreamState {
            id: "resp_x".into(), model: "gpt-5.6-terra".into(), created: 0,
            buffer: Vec::new(), created_sent: false,
            text_blocks: HashMap::new(), tool_blocks: HashMap::new(),
            output_items: Vec::new(),
            next_output_index: 0, usage: None, done: false,
        }
    }

    fn feed_resp(st: &mut RespStreamState, sse: &str) -> String {
        st.buffer.extend_from_slice(sse.as_bytes());
        let mut out = String::new();
        for (ev, data) in drain_sse_events(&mut st.buffer) {
            st.handle_event(&ev, &data, &mut out);
        }
        out
    }

    #[test]
    fn responses_stream_text_and_completed() {
        let mut st = new_resp_state();
        let mut all = String::new();
        all.push_str(&feed_resp(&mut st, "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n"));
        all.push_str(&feed_resp(&mut st, "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":9,\"output_tokens\":1,\"cache_read_input_tokens\":100}}\n\n"));
        let mut fin = String::new();
        st.finalize(&mut fin);
        all.push_str(&fin);
        assert!(all.contains("event: response.created"));
        assert!(all.contains("event: response.output_text.delta"));
        assert!(all.contains("\"delta\":\"Hi\""));
        assert!(all.contains("event: response.completed"));
        assert!(all.contains("\"output_text\""));
        // input_tokens 含缓存：9+100=109
        assert!(all.contains("\"input_tokens\":109"));
        assert!(all.contains("\"cached_tokens\":100"));
    }

    /// codex CLI 只从 output_item.done 收集本轮产出：少发即「回复为空」。
    #[test]
    fn responses_stream_emits_output_item_done_for_message() {
        let mut st = new_resp_state();
        let mut all = String::new();
        all.push_str(&feed_resp(&mut st, "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"));
        all.push_str(&feed_resp(&mut st, "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n"));
        all.push_str(&feed_resp(&mut st, "event: content_block_stop\ndata: {\"index\":0}\n\n"));
        let mut fin = String::new();
        st.finalize(&mut fin);
        all.push_str(&fin);

        assert!(all.contains("event: response.output_item.added"));
        assert!(all.contains("event: response.output_item.done"));
        assert!(all.contains("\"type\":\"message\""));
        assert!(all.contains("\"text\":\"Hi\""));
        // 只该有一个 message item（done 后不再于 finalize 重复收尾）
        assert_eq!(all.matches("event: response.output_item.done").count(), 1);
    }

    /// 缺 content_block_stop（上游断流）时 finalize 兜底补 done，已产出文本不丢。
    #[test]
    fn responses_stream_finalize_closes_open_text_block() {
        let mut st = new_resp_state();
        let mut all = String::new();
        all.push_str(&feed_resp(&mut st, "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n"));
        let mut fin = String::new();
        st.finalize(&mut fin);
        all.push_str(&fin);
        assert!(all.contains("event: response.output_item.done"));
        assert!(all.contains("\"text\":\"partial\""));
    }

    /// 工具调用：arguments 需在 output_item.done 里给全串（delta 事件客户端会忽略）。
    #[test]
    fn responses_stream_tool_call_done_carries_full_arguments() {
        let mut st = new_resp_state();
        let mut all = String::new();
        all.push_str(&feed_resp(&mut st, "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"shell\"}}\n\n"));
        all.push_str(&feed_resp(&mut st, "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\"}}\n\n"));
        all.push_str(&feed_resp(&mut st, "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"}\"}}\n\n"));
        all.push_str(&feed_resp(&mut st, "event: content_block_stop\ndata: {\"index\":0}\n\n"));
        let mut fin = String::new();
        st.finalize(&mut fin);
        all.push_str(&fin);

        assert!(all.contains("event: response.output_item.done"));
        assert!(all.contains("\"type\":\"function_call\""));
        assert!(all.contains("\"call_id\":\"call_1\""));
        assert!(all.contains("\"name\":\"shell\""));
        // arguments 为累积后的完整 JSON 串
        assert!(all.contains(r#""arguments":"{\"cmd\":\"ls\"}""#));
    }

    /// 空文本 + 工具调用：不产出空 assistant message item。
    #[test]
    fn responses_stream_skips_empty_message_item() {
        let mut st = new_resp_state();
        let mut all = String::new();
        all.push_str(&feed_resp(&mut st, "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"));
        all.push_str(&feed_resp(&mut st, "event: content_block_stop\ndata: {\"index\":0}\n\n"));
        all.push_str(&feed_resp(&mut st, "event: content_block_start\ndata: {\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_2\",\"name\":\"f\"}}\n\n"));
        all.push_str(&feed_resp(&mut st, "event: content_block_stop\ndata: {\"index\":1}\n\n"));
        let mut fin = String::new();
        st.finalize(&mut fin);
        all.push_str(&fin);

        assert_eq!(all.matches("event: response.output_item.done").count(), 1);
        assert!(all.contains("\"type\":\"function_call\""));
        // 空 arguments 兜底为 {}
        assert!(all.contains(r#""arguments":"{}""#));
    }
}
