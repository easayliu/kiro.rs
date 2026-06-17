//! Anthropic API Handler 函数

use std::convert::Infallible;

use anyhow::Error;
use crate::kiro::errors::{NoAvailableCredentialsError, UpstreamHttpError};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::interval;
use uuid::Uuid;

use std::sync::Arc;

use super::cache_tracker::{CacheProfile, CacheScope, CacheTracker};
use super::converter::{ConversionError, KIRO_MAX_REQUEST_BYTES, convert_request};
use super::middleware::AppState;
use super::stream::{BufferedStreamContext, CacheUsage, SseEvent, StreamContext};
use super::types::{CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse};
use super::websearch;
use crate::kiro::binding::BindingTable;
use crate::kiro::provider::KiroProvider;

/// 汇总 prompt caching 相关的 usage 数值
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CacheUsageContext {
    pub cache_creation_input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
    /// 最后 breakpoint 之后的未缓存 tokens，对应 Anthropic 返回的 input_tokens
    pub uncached_input_tokens: i32,
    /// 命中条目持久化的「上游计费口径」累计 token（W），计费时钉住 cache_read 以守恒。
    pub cache_read_billed: Option<i32>,
}

/// 粘性绑定解析：返回本次请求应优先使用的凭证 id。
///
/// 粘性仅在 cache scope 为 `PerCredential` 时启用——此模式下本地 prompt cache
/// 模拟按 credential 隔离，同一用户换号会 miss，故需要把同一身份钉在同一凭据上。
/// 默认 `Global` 模式下 cache 命中只取决于用户身份、与选到哪个凭据无关（Kiro 上游
/// 本身也无真实 prompt cache），粘性零收益却会把负载倾斜到单号，因此直接跳过，
/// 让选号走纯 least-request / LRU 均衡。
///
/// 其余短路条件：
/// - 未提供 binding_key（没有 metadata.user_id）→ 返回 None，走默认选择
/// - 无可用凭证 → 返回 None
/// - 首次见到的用户会在绑定表中创建新绑定
fn resolve_sticky_preference(
    cache_tracker: &CacheTracker,
    binding_table: &BindingTable,
    provider: &KiroProvider,
    binding_key: Option<u64>,
    model: &str,
) -> Option<u64> {
    // Global 模式：换号不影响 cache 命中，禁用粘性以保留负载均衡。
    if !matches!(cache_tracker.cache_scope(), CacheScope::PerCredential) {
        return None;
    }
    let identity = binding_key?;
    let available = provider.available_credential_ids(Some(model));
    binding_table.resolve(identity, &available)
}

/// 调用完成后的粘性绑定维护。
///
/// - 成功但落在非 preferred 凭证 → preferred 本轮失败，累计错误，必要时改绑
/// - 调用整体失败 → 累计错误，必要时改绑（下一次请求生效）
fn update_binding_after_call(
    binding_table: &BindingTable,
    provider: &KiroProvider,
    binding_key: Option<u64>,
    preferred: Option<u64>,
    actual: Option<u64>,
    model: &str,
) {
    let (identity, pref) = match (binding_key, preferred) {
        (Some(i), Some(p)) => (i, p),
        _ => return,
    };
    let preferred_failed = match actual {
        Some(used) => used != pref,
        None => true,
    };
    if !preferred_failed {
        return;
    }
    if binding_table.report_error(pref) {
        let available = provider.available_credential_ids(Some(model));
        if let Some(new_cred) = binding_table.rebind(identity, pref, &available) {
            tracing::info!(
                identity = identity,
                from = pref,
                to = new_cred,
                "粘性绑定改绑：preferred 凭证累计错误达阈值"
            );
        }
    }
}

/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(err: Error) -> Response {
    // 内部"无可用凭据"：请求未发到上游，503 服务不可用更准确
    if let Some(no_creds) = err.downcast_ref::<NoAvailableCredentialsError>() {
        tracing::warn!(error = %err, "服务暂时不可用：所有凭据均已禁用");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                format!(
                    "服务暂时不可用：所有凭据均已禁用（{}/{}）",
                    no_creds.available, no_creds.total
                ),
            )),
        )
            .into_response();
    }

    // 上游 HTTP 错误：按 status 透传（429 → 429、5xx → 502、其他 4xx → 502）
    if let Some(upstream) = err.downcast_ref::<UpstreamHttpError>() {
        // 上下文窗口满 / 输入过长：保留原 400 + invalid_request_error 映射
        if upstream.body.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
            tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "Context window is full. Reduce conversation history, system prompt, or tools.",
                )),
            )
                .into_response();
        }
        if upstream.body.contains("Input is too long") {
            tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "Input is too long. Reduce the size of your messages.",
                )),
            )
                .into_response();
        }

        // 透传上游 status，让客户端按上游真实状态分类（4xx 不重试、5xx 可重试）。
        // err_type 按 Anthropic error 规范归类，未知 status 兜底为 502。
        let mapped_status = StatusCode::from_u16(upstream.status)
            .unwrap_or(StatusCode::BAD_GATEWAY);
        let err_type = match upstream.status {
            400 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            404 => "not_found_error",
            413 => "request_too_large",
            429 => "rate_limit_error",
            s if (500..=599).contains(&s) => "api_error",
            _ => "api_error",
        };
        tracing::error!(
            upstream_status = upstream.status,
            mapped_status = mapped_status.as_u16(),
            "Kiro API 调用失败: {}",
            err
        );
        return (
            mapped_status,
            Json(ErrorResponse::new(
                err_type,
                format!(
                    "上游 API {}: {}",
                    upstream.status, upstream.body
                ),
            )),
        )
            .into_response();
    }

    // 兜底（无类型信息的旧错误 / 未知内部错误）
    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models() -> impl IntoResponse {
    tracing::debug!("Received GET /v1/models request");

    let models = vec![
        Model {
            id: "claude-fable-5".to_string(),
            object: "model".to_string(),
            created: 1781308800, // June 10, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Fable 5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128000,
        },
        Model {
            id: "claude-opus-4-8".to_string(),
            object: "model".to_string(),
            created: 1779494400, // May 20, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128000,
        },
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1778889600, // May 13, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
    ];

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    JsonExtractor(payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::debug!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        let binding_key = super::cache_tracker::extract_binding_key(&payload);
        return websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            state.binding_table.clone(),
            binding_key,
        )
        .await;
    }

    // 转换请求
    let conversion_result = match convert_request(&payload, provider.origin(), provider.is_cli_mode()) {
        Ok(result) => result,
        Err(e) => {
            let (status, error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => (
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    format!("模型不支持: {}", model),
                ),
                ConversionError::EmptyMessages => (
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "消息列表为空".to_string(),
                ),
                ConversionError::ImageTooLarge { .. } => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    e.to_string(),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            return (status, Json(ErrorResponse::new(error_type, message))).into_response();
        }
    };

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    if request_body.len() > KIRO_MAX_REQUEST_BYTES {
        tracing::warn!(
            bytes = request_body.len(),
            limit = KIRO_MAX_REQUEST_BYTES,
            "请求体超过上游 Kiro 限额，提前返回 413（绝大多数由历史图片/超长贴文堆积引起）"
        );
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse::new(
                "request_too_large",
                format!(
                    "请求体 {} 字节超过上游限额 {} 字节；请减少图片数量、压缩历史或截断超长文本",
                    request_body.len(),
                    KIRO_MAX_REQUEST_BYTES
                ),
            )),
        )
            .into_response();
    }

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 构建 cache profile（基于原始请求：含 cache_control 等元数据）
    let cache_tracker = state.cache_tracker.clone();
    let cache_profile = cache_tracker.build_profile(&payload, input_tokens);

    // 粘性绑定：解析 user_id → preferred 凭证
    let binding_key = cache_profile.binding_key();
    let binding_table = state.binding_table.clone();
    let preferred = resolve_sticky_preference(
        &cache_tracker,
        &binding_table,
        provider.as_ref(),
        binding_key,
        &payload.model,
    );

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            cache_tracker,
            cache_profile,
            binding_table,
            binding_key,
            preferred,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            extract_thinking,
            tool_name_map,
            cache_tracker,
            cache_profile,
            binding_table,
            binding_key,
            preferred,
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    cache_tracker: Arc<CacheTracker>,
    cache_profile: CacheProfile,
    binding_table: Arc<BindingTable>,
    binding_key: Option<u64>,
    preferred: Option<u64>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_result = match provider.call_api_stream(request_body, preferred).await {
        Ok(resp) => resp,
        Err(e) => {
            update_binding_after_call(
                &binding_table,
                provider.as_ref(),
                binding_key,
                preferred,
                None,
                model,
            );
            return map_provider_error(e);
        }
    };
    update_binding_after_call(
        &binding_table,
        provider.as_ref(),
        binding_key,
        preferred,
        Some(api_result.credential_id),
        model,
    );

    // 原子地计算缓存命中并更新 checkpoint 表
    let (cache_result, cache_writeback) =
        cache_tracker.compute_and_update(api_result.credential_id, &cache_profile);
    let cache_context = CacheUsageContext {
        cache_creation_input_tokens: cache_result.cache_creation_input_tokens,
        cache_read_input_tokens: cache_result.cache_read_input_tokens,
        cache_creation_5m_input_tokens: cache_result.cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens: cache_result.cache_creation_1h_input_tokens,
        uncached_input_tokens: cache_result.uncached_input_tokens,
        cache_read_billed: cache_result.cache_read_billed,
    };

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(model, input_tokens, thinking_enabled, tool_name_map);
    // TTFT 原点钉在「向上游发出请求」时刻，覆盖上游等首 token 的等待（见 ApiCallResult）。
    ctx.set_ttft_origin(api_result.upstream_request_at);
    ctx.set_cache_usage(cache_context);
    // 计费完成后（流末尾 contextUsageEvent）把缩放后的 billed 累计回写缓存，供下次命中守恒。
    ctx.set_billing_writeback(cache_tracker.clone(), cache_writeback);

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(api_result.response, ctx, initial_events);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 上游中途掐断（真截断，likely_complete=false）时发给下游的 SSE `error` 事件。
///
/// 为什么用 error 事件而不是补 message_stop：上游（AWS/Kiro）或中间代理对长回复
/// 施加了响应时长上限，会在 ~4 分钟处 abrupt close（peer 未发 close_notify），此时
/// 响应并未完成（saw_metering=false）。若照常补 `message_delta{stop_reason:end_turn}`
/// + `message_stop`，客户端（Claude Code/SDK）会把半截回复当成正常 end_turn 收下、
/// 不会重试，用户静默拿到残缺答案。改发 `error` 事件，SDK 才能识别异常并触发重试。
/// 类型用 `api_error`（上游连接失败，可重试），与 Anthropic 流式错误事件格式一致。
fn create_truncation_error_sse() -> Bytes {
    Bytes::from(
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"Upstream closed the connection before the response completed (likely an upstream/proxy response-duration limit). The reply was truncated; please retry.\"}}\n\n",
    )
}

/// 流式转发过程统计（仅用于诊断"回复中断"：耗时 / 字节 / 帧数）
///
/// 用于在流结束（正常 EOF 或读取出错）时，对比"中断"与"正常结束"两类样本的
/// 已耗时分布。若中断样本的 `elapsed_secs` 普遍贴近上游 client 的总超时（720s）
/// 且 `is_timeout=true`，即可坐实是 reqwest 总超时（含读 body 的总死线）截断了长回复。
#[derive(Debug)]
struct StreamStats {
    start: Instant,
    bytes: usize,
    frames: usize,
    /// 是否见过 meteringEvent（计费事件，上游生成全部完成后才发，是最可靠的收尾信号）
    saw_metering: bool,
    /// 是否见过 contextUsageEvent（上下文用量，同样在流末尾）
    saw_context_usage: bool,
}

impl StreamStats {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            bytes: 0,
            frames: 0,
            saw_metering: false,
            saw_context_usage: false,
        }
    }

    /// 记录终止类事件，用于 EOF 时判断是否为静默截断
    fn note_event(&mut self, event: &Event) {
        match event {
            Event::Metering(_) => self.saw_metering = true,
            Event::ContextUsage(_) => self.saw_context_usage = true,
            _ => {}
        }
    }
}

/// 展开 reqwest 错误的完整 source 链，便于区分 timeout / connect reset / 上游提前 EOF / h2 RST
fn describe_reqwest_error(e: &reqwest::Error) -> String {
    let mut chain = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        chain.push_str(" -> ");
        chain.push_str(&s.to_string());
        src = s.source();
    }
    chain
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), StreamStats::new()),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, mut stats)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            // [TTFT 埋点] 首个 body chunk：从“上游响应头到达”到“上游首字节”的间隔。
                            // stats.start 在 create_sse_stream 构造时（即拿到响应头后）起算。
                            // 配合 provider 的 acquire/send 即可拼出完整首字耗时分解。
                            if stats.bytes == 0 {
                                ctx.mark_first_byte();
                                tracing::debug!(
                                    "[TTFT] 上游首字节: header→first_byte={}ms",
                                    stats.start.elapsed().as_millis()
                                );
                            }
                            stats.bytes += chunk.len();
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!(
                                    elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                    bytes = stats.bytes,
                                    "缓冲区溢出（chunk 被丢弃，后续帧可能错位导致截断）: {}",
                                    e
                                );
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        stats.frames += 1;
                                        if let Ok(event) = Event::from_frame(frame) {
                                            stats.note_event(&event);
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, stats)))
                        }
                        Some(Err(e)) => {
                            // 流式响应中断：读取上游 body 失败。
                            // is_timeout=true + elapsed≈720s → reqwest 总超时；
                            // is_decode=true + "close_notify" → 上游/代理中途关 TLS 连接。
                            // likely_complete 用于区分"真截断"与"rustls 缺 close_notify 误判"：
                            //   收到 meteringEvent 且解码器无残留 → 响应其实已完整，换 native-tls 即可消除该错误。
                            let pending = decoder.pending_bytes();
                            let likely_complete = stats.saw_metering && pending == 0;
                            tracing::error!(
                                model = %ctx.model,
                                is_timeout = e.is_timeout(),
                                is_body = e.is_body(),
                                is_decode = e.is_decode(),
                                is_request = e.is_request(),
                                elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                bytes = stats.bytes,
                                frames = stats.frames,
                                pending_bytes = pending,
                                saw_metering = stats.saw_metering,
                                saw_context_usage = stats.saw_context_usage,
                                likely_complete = likely_complete,
                                output_tokens = ctx.output_tokens,
                                "流式响应中断：读取上游响应流失败（likely_complete=false→发 error 事件让客户端重试；likely_complete=true→疑似 rustls 缺 close_notify 误判、响应其实已完整，照常补干净收尾）: {}",
                                describe_reqwest_error(&e)
                            );
                            // likely_complete=true（误判，响应其实已完整）：照常补 message_stop 等干净收尾。
                            // likely_complete=false（真截断）：只发 error 事件，不补 message_stop——
                            //   否则客户端会把半截回复当成正常 end_turn 收下、不重试。
                            let bytes: Vec<Result<Bytes, Infallible>> = if likely_complete {
                                ctx.generate_final_events()
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect()
                            } else {
                                vec![Ok(create_truncation_error_sse())]
                            };
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, stats)))
                        }
                        None => {
                            // 流结束（上游 EOF）。区分"正常结束"与"静默截断"：
                            // 静默截断走的也是 None 分支（尤其 HTTP/1.1 close-delimited body 被提前关闭时），
                            // 不会报错，需靠应用层判据识别：
                            //   判据1 pending_bytes>0 → 切在半个帧中间；
                            //   判据2 没见过 meteringEvent → 收尾事件缺失，几乎可断定被提前截断。
                            let pending = decoder.pending_bytes();
                            let suspected_truncation = pending > 0 || !stats.saw_metering;
                            if suspected_truncation {
                                tracing::warn!(
                                    model = %ctx.model,
                                    elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                    bytes = stats.bytes,
                                    frames = stats.frames,
                                    pending_bytes = pending,
                                    saw_metering = stats.saw_metering,
                                    saw_context_usage = stats.saw_context_usage,
                                    output_tokens = ctx.output_tokens,
                                    "疑似静默截断：流以 EOF 正常结束但缺少收尾信号（残留半帧或未收到 meteringEvent），客户端会看到半截回复"
                                );
                            } else {
                                // 计费汇总由 generate_final_events 的「请求完成（流式）」承担；
                                // 此处仅记 transport 层 EOF/耗时，降级 debug 避免每请求双 info 行。
                                tracing::debug!(
                                    model = %ctx.model,
                                    elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                    output_tokens = ctx.output_tokens,
                                    "上游流正常结束（EOF）"
                                );
                                tracing::debug!(
                                    bytes = stats.bytes,
                                    frames = stats.frames,
                                    "上游流正常结束（EOF）"
                                );
                            }
                            // 输出中断处理：pending>0（解码器残留半帧）是流被切在帧中间的
                            // 铁证 = 真·输出中断。此时发 error 事件让客户端重试，**不**补
                            // message_stop——否则客户端会把半截回复当成正常 end_turn 收下、
                            // 不重试，用户静默拿到残缺答案。与 read-error 分支(likely_complete
                            // =false)行为一致。仅 !saw_metering(pending==0) 是弱信号（可能正常
                            // 完成但未带 metering），不据此报错以免误杀、放大重试加剧上游限速。
                            let bytes: Vec<Result<Bytes, Infallible>> = if pending > 0 {
                                vec![Ok(create_truncation_error_sse())]
                            } else {
                                ctx.generate_final_events()
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect()
                            };
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, stats)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, stats)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

use super::converter::{
    apply_usage_multiplier, credit_to_usd, get_context_window_size, official_price_usd,
};

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    cache_tracker: Arc<CacheTracker>,
    cache_profile: CacheProfile,
    binding_table: Arc<BindingTable>,
    binding_key: Option<u64>,
    preferred: Option<u64>,
) -> Response {
    let request_start = Instant::now();
    // 调用 Kiro API 并缓冲完整响应体（支持多凭据故障转移；body 中途被上游 RST/EOF
    // 时会重新发起整轮调用并换凭据，见 call_api_buffered）
    let api_result = match provider.call_api_buffered(request_body, preferred).await {
        Ok(resp) => resp,
        Err(e) => {
            update_binding_after_call(
                &binding_table,
                provider.as_ref(),
                binding_key,
                preferred,
                None,
                model,
            );
            return map_provider_error(e);
        }
    };
    update_binding_after_call(
        &binding_table,
        provider.as_ref(),
        binding_key,
        preferred,
        Some(api_result.credential_id),
        model,
    );

    // 原子地计算缓存命中并更新 checkpoint 表
    let (cache_result, cache_writeback) =
        cache_tracker.compute_and_update(api_result.credential_id, &cache_profile);
    let cache_context = CacheUsageContext {
        cache_creation_input_tokens: cache_result.cache_creation_input_tokens,
        cache_read_input_tokens: cache_result.cache_read_input_tokens,
        cache_creation_5m_input_tokens: cache_result.cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens: cache_result.cache_creation_1h_input_tokens,
        uncached_input_tokens: cache_result.uncached_input_tokens,
        cache_read_billed: cache_result.cache_read_billed,
    };

    let body_bytes = api_result.body;

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // contextUsageEvent 算出的上游实际输入 token 总量（含注入的 system prompt）。
    // 用于计费时重算未缓存 token，比基于用户请求的估算更贴近上游真实用量。
    let mut context_input_tokens: Option<i32> = None;
    // meteringEvent 给出的上游真实扣费（单位 credit），用于成本对账 / 诊断。
    let mut upstream_credit: Option<f64> = None;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;

                            // 累积工具的 JSON 输入
                            let buffer = tool_json_buffers
                                .entry(tool_use.tool_use_id.clone())
                                .or_insert_with(String::new);
                            buffer.push_str(&tool_use.input);

                            // 如果是完整的工具调用，添加到列表
                            if tool_use.stop {
                                let input: serde_json::Value = if buffer.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    serde_json::from_str(buffer)
                                        .unwrap_or_else(|e| {
                                            tracing::warn!(
                                                "工具输入 JSON 解析失败: {}, tool_use_id: {}",
                                                e, tool_use.tool_use_id
                                            );
                                            serde_json::json!({})
                                        })
                                };

                                let original_name = tool_name_map
                                    .get(&tool_use.name)
                                    .cloned()
                                    .unwrap_or_else(|| tool_use.name.clone());

                                // 规范化 tool_use_id：补 `toolu_` 前缀，与回传的 tool_result id 对齐
                                let client_tool_use_id =
                                    super::converter::normalize_tool_use_id_for_client(
                                        &tool_use.tool_use_id,
                                    );

                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": client_tool_use_id,
                                    "name": original_name,
                                    "input": input
                                }));
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比反推实际 input_tokens（round 精确还原，
                            // 上游百分比满精度，截断会丢 ~1 token）。
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens = (context_usage.context_usage_percentage
                                * (window_size as f64)
                                / 100.0)
                                .round() as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Metering(metering) => {
                            upstream_credit = Some(metering.usage);
                        }
                        Event::Exception { exception_type, .. } => {
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if thinking_enabled {
        // 从完整文本中提取 thinking 块
        let (thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        if let Some(thinking_text) = thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 计费口径的缓存使用量：有 contextUsageEvent 时把本地估算的缓存拆分按上游
    // 实际总量等比缩放，使 cache_* 与 uncached 全部落在上游计量体系（三者之和
    // = 上游实际总量）；否则用 cache_tracker 基于请求估算算出的本地拆分。
    let estimated_usage = CacheUsage {
        cache_creation_input_tokens: cache_context.cache_creation_input_tokens,
        cache_read_input_tokens: cache_context.cache_read_input_tokens,
        cache_creation_5m_input_tokens: cache_context.cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens: cache_context.cache_creation_1h_input_tokens,
        uncached_input_tokens: cache_context.uncached_input_tokens,
    };
    let billing = match context_input_tokens {
        Some(context_total) => {
            let billed = estimated_usage.billed_split(
                estimated_input_tokens,
                context_total,
                cache_context.cache_read_billed,
            );
            // 计费完成后把缩放后的 billed 累计回写缓存，供下次命中实现读写守恒。
            cache_tracker.apply_billing_writeback(
                &cache_writeback,
                billed.cache_read_input_tokens,
                billed.cache_creation_input_tokens,
            );
            billed
        }
        None => estimated_usage,
    };
    let billed_input_tokens = billing.uncached_input_tokens.max(1);

    let actual = credit_to_usd(upstream_credit.unwrap_or(0.0));
    let official = official_price_usd(
        model,
        billing.uncached_input_tokens,
        billing.cache_read_input_tokens,
        billing.cache_creation_5m_input_tokens,
        billing.cache_creation_1h_input_tokens,
        output_tokens,
    );
    let margin = ((official - actual) * 1_000_000.0).round() / 1_000_000.0;
    // 进程维度累计实际成本/官方价/毛利，供 admin 只读接口查询（无锁原子，零热路径开销）。
    let truncated = stop_reason == "max_tokens";
    super::billing_stats().record(actual, official, margin, truncated);
    tracing::info!(
        model = %model,
        input_tokens = billed_input_tokens,
        cache_read = billing.cache_read_input_tokens,
        cache_creation = billing.cache_creation_input_tokens,
        output_tokens,
        total_tokens = billing.cache_read_input_tokens
            + billing.cache_creation_input_tokens
            + billing.uncached_input_tokens,
        upstream_credit = upstream_credit.unwrap_or(0.0),
        actual_cost_usd = actual,
        official_price_usd = official,
        margin_usd = margin,
        stop_reason = %stop_reason,
        elapsed_secs = request_start.elapsed().as_secs_f64(),
        "请求完成（非流式）"
    );

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": apply_usage_multiplier(billed_input_tokens),
            "output_tokens": apply_usage_multiplier(output_tokens),
            "cache_creation_input_tokens": apply_usage_multiplier(billing.cache_creation_input_tokens),
            "cache_read_input_tokens": apply_usage_multiplier(billing.cache_read_input_tokens),
            "cache_creation": {
                "ephemeral_5m_input_tokens": apply_usage_multiplier(billing.cache_creation_5m_input_tokens),
                "ephemeral_1h_input_tokens": apply_usage_multiplier(billing.cache_creation_1h_input_tokens),
            }
        }
    });

    (StatusCode::OK, Json(response_body)).into_response()
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::debug!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    JsonExtractor(payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::debug!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        let binding_key = super::cache_tracker::extract_binding_key(&payload);
        return websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            state.binding_table.clone(),
            binding_key,
        )
        .await;
    }

    // 转换请求
    let conversion_result = match convert_request(&payload, provider.origin(), provider.is_cli_mode()) {
        Ok(result) => result,
        Err(e) => {
            let (status, error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => (
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    format!("模型不支持: {}", model),
                ),
                ConversionError::EmptyMessages => (
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "消息列表为空".to_string(),
                ),
                ConversionError::ImageTooLarge { .. } => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    e.to_string(),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            return (status, Json(ErrorResponse::new(error_type, message))).into_response();
        }
    };

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    if request_body.len() > KIRO_MAX_REQUEST_BYTES {
        tracing::warn!(
            bytes = request_body.len(),
            limit = KIRO_MAX_REQUEST_BYTES,
            "请求体超过上游 Kiro 限额，提前返回 413（绝大多数由历史图片/超长贴文堆积引起）"
        );
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse::new(
                "request_too_large",
                format!(
                    "请求体 {} 字节超过上游限额 {} 字节；请减少图片数量、压缩历史或截断超长文本",
                    request_body.len(),
                    KIRO_MAX_REQUEST_BYTES
                ),
            )),
        )
            .into_response();
    }

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 构建 cache profile（基于原始请求）
    let cache_tracker = state.cache_tracker.clone();
    let cache_profile = cache_tracker.build_profile(&payload, input_tokens);

    // 粘性绑定：解析 user_id → preferred 凭证
    let binding_key = cache_profile.binding_key();
    let binding_table = state.binding_table.clone();
    let preferred = resolve_sticky_preference(
        &cache_tracker,
        &binding_table,
        provider.as_ref(),
        binding_key,
        &payload.model,
    );

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应（缓冲模式）
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            cache_tracker,
            cache_profile,
            binding_table,
            binding_key,
            preferred,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            extract_thinking,
            tool_name_map,
            cache_tracker,
            cache_profile,
            binding_table,
            binding_key,
            preferred,
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    cache_tracker: Arc<CacheTracker>,
    cache_profile: CacheProfile,
    binding_table: Arc<BindingTable>,
    binding_key: Option<u64>,
    preferred: Option<u64>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_result = match provider.call_api_stream(request_body, preferred).await {
        Ok(resp) => resp,
        Err(e) => {
            update_binding_after_call(
                &binding_table,
                provider.as_ref(),
                binding_key,
                preferred,
                None,
                model,
            );
            return map_provider_error(e);
        }
    };
    update_binding_after_call(
        &binding_table,
        provider.as_ref(),
        binding_key,
        preferred,
        Some(api_result.credential_id),
        model,
    );

    // 原子地计算缓存命中并更新 checkpoint 表
    let (cache_result, cache_writeback) =
        cache_tracker.compute_and_update(api_result.credential_id, &cache_profile);
    let cache_context = CacheUsageContext {
        cache_creation_input_tokens: cache_result.cache_creation_input_tokens,
        cache_read_input_tokens: cache_result.cache_read_input_tokens,
        cache_creation_5m_input_tokens: cache_result.cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens: cache_result.cache_creation_1h_input_tokens,
        uncached_input_tokens: cache_result.uncached_input_tokens,
        cache_read_billed: cache_result.cache_read_billed,
    };

    // 创建缓冲流处理上下文
    let mut ctx = BufferedStreamContext::new(model, estimated_input_tokens, thinking_enabled, tool_name_map);
    // TTFT 原点钉在「向上游发出请求」时刻，覆盖上游等首 token 的等待（见 ApiCallResult）。
    ctx.set_ttft_origin(api_result.upstream_request_at);
    ctx.set_cache_usage(cache_context);
    // 计费完成后（流末尾 contextUsageEvent）把缩放后的 billed 累计回写缓存，供下次命中守恒。
    ctx.set_billing_writeback(cache_tracker.clone(), cache_writeback);

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(api_result.response, ctx);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            StreamStats::new(),
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, mut stats)| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, stats)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                if stats.bytes == 0 {
                                    ctx.mark_first_byte();
                                }
                                stats.bytes += chunk.len();
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!(
                                        elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                        bytes = stats.bytes,
                                        "缓冲区溢出（chunk 被丢弃，后续帧可能错位导致截断）: {}",
                                        e
                                    );
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            stats.frames += 1;
                                            if let Ok(event) = Event::from_frame(frame) {
                                                stats.note_event(&event);
                                                // 缓冲事件（复用 StreamContext 的处理逻辑）
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                // 缓冲模式（/cc/v1，Claude Code 端点）：全程只发 ping。
                                // likely_complete=true 表示响应其实已完整、疑似 rustls 缺 close_notify 误判。
                                let pending = decoder.pending_bytes();
                                let likely_complete = stats.saw_metering && pending == 0;
                                tracing::error!(
                                    model = %ctx.model(),
                                    is_timeout = e.is_timeout(),
                                    is_body = e.is_body(),
                                    is_decode = e.is_decode(),
                                    is_request = e.is_request(),
                                    elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                    bytes = stats.bytes,
                                    frames = stats.frames,
                                    pending_bytes = pending,
                                    saw_metering = stats.saw_metering,
                                    saw_context_usage = stats.saw_context_usage,
                                    likely_complete = likely_complete,
                                    output_tokens = ctx.output_tokens(),
                                    "流式响应中断（缓冲模式）：读取上游响应流失败（likely_complete=false→发已缓冲内容+error 事件让客户端重试；likely_complete=true→疑似 rustls 缺 close_notify 误判、响应其实已完整，照常补干净收尾）: {}",
                                    describe_reqwest_error(&e)
                                );
                                // likely_complete=true（误判，响应其实已完整）：照常补干净收尾。
                                // likely_complete=false（真截断）：只发 error 事件，不补 message_stop——
                                //   否则客户端会把半截回复当成正常 end_turn 收下、不重试。缓冲模式下
                                //   尚未向客户端发过任何内容，error 事件可作为首个事件直接交付。
                                let bytes: Vec<Result<Bytes, Infallible>> = if likely_complete {
                                    ctx.finish_and_get_all_events()
                                        .into_iter()
                                        .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                        .collect()
                                } else {
                                    vec![Ok(create_truncation_error_sse())]
                                };
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, stats)));
                            }
                            None => {
                                // 流结束（上游 EOF）。同样区分正常结束与静默截断（判据同直传路径）。
                                let pending = decoder.pending_bytes();
                                let suspected_truncation = pending > 0 || !stats.saw_metering;
                                if suspected_truncation {
                                    tracing::warn!(
                                        model = %ctx.model(),
                                        elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                        bytes = stats.bytes,
                                        frames = stats.frames,
                                        pending_bytes = pending,
                                        saw_metering = stats.saw_metering,
                                        saw_context_usage = stats.saw_context_usage,
                                        output_tokens = ctx.output_tokens(),
                                        "疑似静默截断（缓冲模式）：流以 EOF 正常结束但缺少收尾信号（残留半帧或未收到 meteringEvent）"
                                    );
                                } else {
                                    // 计费汇总由 inner 的「请求完成（流式）」承担；此处仅记
                                    // transport 层 EOF/耗时，降级 debug 避免每请求双 info 行。
                                    tracing::debug!(
                                        model = %ctx.model(),
                                        elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                        output_tokens = ctx.output_tokens(),
                                        "上游流正常结束（EOF，缓冲模式）"
                                    );
                                    tracing::debug!(
                                        bytes = stats.bytes,
                                        frames = stats.frames,
                                        "上游流正常结束（EOF，缓冲模式）"
                                    );
                                }
                                // 输出中断处理（缓冲模式）：pending>0 = 流被切在帧中间 =
                                // 真·输出中断。缓冲模式全程只发过 ping、尚未交付任何内容，
                                // 此时直接发 error 事件让客户端重试，而非把残缺内容当正常收尾
                                // 一次性 flush。与 read-error 分支(likely_complete=false)一致。
                                // 仅 !saw_metering(pending==0) 是弱信号，不据此报错以免误杀。
                                let bytes: Vec<Result<Bytes, Infallible>> = if pending > 0 {
                                    vec![Ok(create_truncation_error_sse())]
                                } else {
                                    ctx.finish_and_get_all_events()
                                        .into_iter()
                                        .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                        .collect()
                                };
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, stats)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}
