//! OpenAI 原生端点：`/v1/chat/completions`、`/v1/responses`（仅 GPT）。
//!
//! 请求转成 Anthropic 形状后走同一套 `handle_messages` 管线，响应再转码回
//! OpenAI 形状。凭证/缓存/计费/流转换全部复用。

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Json};
use serde_json::Value;
use uuid::Uuid;

use crate::anthropic::handlers::handle_messages;
use crate::anthropic::middleware::{AppState, RequestId};
use crate::anthropic::types::MessagesRequest;

use super::convert::{
    chat_request_to_anthropic, inject_auto_cache_breakpoint, is_gpt_model,
    responses_request_to_anthropic,
};
use super::transcode::{
    chat_nonstream, chat_stream, json_response, openai_error_obj, responses_nonstream,
    responses_stream,
};

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn bad_request(msg: &str) -> Response {
    json_response(StatusCode::BAD_REQUEST, &openai_error_obj(msg, "invalid_request_error"))
}

/// 把 Anthropic 形状 JSON 反序列化为内部 `MessagesRequest`。
fn to_messages_request(anthropic: Value) -> Result<MessagesRequest, Response> {
    serde_json::from_value(anthropic)
        .map_err(|e| bad_request(&format!("请求转换失败: {e}")))
}

/// `POST /v1/chat/completions`（OpenAI Chat Completions，仅 GPT）。
pub async fn post_chat_completions(
    State(state): State<AppState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Json(req): Json<Value>,
) -> Response {
    let model = req.get("model").and_then(Value::as_str).unwrap_or_default();
    if !is_gpt_model(model) {
        return bad_request(&format!(
            "/v1/chat/completions 仅支持 gpt-* 模型，收到: {model}"
        ));
    }

    let stream = req.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = req
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut anthropic = match chat_request_to_anthropic(&req) {
        Ok(v) => v.anthropic,
        Err(e) => return bad_request(&e),
    };
    // OpenAI 端点客户端不发 cache_control，自动注入断点模拟 OpenAI 自动前缀缓存。
    inject_auto_cache_breakpoint(&mut anthropic);
    let payload = match to_messages_request(anthropic) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let resolved_model = payload.model.clone();

    let inner = handle_messages(state, request_id, payload).await;

    let id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let created = now_secs();
    if stream && inner.status() == StatusCode::OK {
        chat_stream(inner, id, resolved_model, created, include_usage)
    } else {
        chat_nonstream(inner, id, resolved_model, created).await
    }
}

/// `POST /v1/responses`（OpenAI Responses API，仅 GPT，流式 + 非流式）。
pub async fn post_responses(
    State(state): State<AppState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Json(req): Json<Value>,
) -> Response {
    let model = req.get("model").and_then(Value::as_str).unwrap_or_default();
    if !is_gpt_model(model) {
        return bad_request(&format!("/v1/responses 仅支持 gpt-* 模型，收到: {model}"));
    }

    let stream = req.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let converted = match responses_request_to_anthropic(&req) {
        Ok(v) => v,
        Err(e) => return bad_request(&e),
    };
    // freeform（type:"custom"）工具名要带到出口：决定工具调用发 custom_tool_call 还是
    // function_call，发错客户端不执行（codex 的 `exec` 即 freeform）。
    let custom_tools = converted.custom_tools;
    let mut anthropic = converted.anthropic;
    inject_auto_cache_breakpoint(&mut anthropic);

    let payload = match to_messages_request(anthropic) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let resolved_model = payload.model.clone();

    let inner = handle_messages(state, request_id, payload).await;

    let id = format!("resp_{}", Uuid::new_v4().simple());
    let created = now_secs();
    if stream && inner.status() == StatusCode::OK {
        responses_stream(inner, id, resolved_model, created, custom_tools)
    } else {
        responses_nonstream(inner, id, resolved_model, created, custom_tools).await
    }
}
