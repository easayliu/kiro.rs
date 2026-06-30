//! Anthropic API Handler 函数

use std::convert::Infallible;

use anyhow::Error;
use crate::kiro::errors::{NoAvailableCredentialsError, UpstreamBodyError, UpstreamHttpError};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use axum::{
    Extension,
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
use super::converter::{ConversionError, convert_request, injected_prompt_tokens};
use super::injection_scan;
use super::middleware::{AppState, RequestId};
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
    /// 是否处于无缓存模式（CacheScope::Off）。为 true 时计费不再扣除 Kiro 服务端
    /// 注入提示词基线（strip_injected_prompt），按上游反推总量原样计费。
    pub cache_disabled: bool,
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
/// 把 provider 错误映射为下游响应，并记一条失败请求时序统计。
///
/// `model` 仅用于统计归类。失败统计口径只含「上游 API 错误」（call_api 返回 Err），
/// 流中途截断不计（见 create_truncation_error_sse 分支）。status_code 取上游 HTTP
/// 状态码；无 HTTP 响应的内部错误用映射码（无可用凭据 503、其它 502）。失败行的
/// token/成本/延迟均为 0，只进错误率统计，不污染成功侧聚合。
fn map_provider_error(err: Error, model: &str) -> Response {
    // 失败请求时序统计：状态码按错误类型归类（凭据全禁 503 / 上游 HTTP 透传 / 兜底 502）。
    // credential_id 取上游错误携带的实际凭据（多凭据重试时为最后一次尝试的凭据）；
    // 无关联凭据的错误（凭据全禁 503 / 请求未发到上游）记 0。
    let (stat_status, stat_cred): (i64, i64) =
        if err.downcast_ref::<NoAvailableCredentialsError>().is_some() {
            (503, 0)
        } else if let Some(upstream) = err.downcast_ref::<UpstreamHttpError>() {
            (upstream.status as i64, upstream.credential_id.unwrap_or(0) as i64)
        } else if let Some(body_err) = err.downcast_ref::<UpstreamBodyError>() {
            // 上游已回 200 但 body 读取失败：映射 502，归类到实际凭据
            (502, body_err.credential_id as i64)
        } else {
            (502, 0)
        };
    crate::stats::record(crate::stats::RequestStat {
        ts: 0,
        model: model.to_string(),
        credential_id: stat_cred,
        actual_micro: 0,
        official_micro: 0,
        margin_micro: 0,
        input_tokens: 0,
        cache_read: 0,
        cache_creation: 0,
        output_tokens: 0,
        ttft_ms: 0,
        elapsed_ms: 0,
        status_code: stat_status,
    });

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

/// 扫描入站内容的 prompt injection 启发式签名并按结构化字段告警。
///
/// 只记录、不拦截：命中通常意味着可疑指令（外发/隐瞒/伪装系统提示/密钥外泄等）藏在
/// 客户端的工具输出里，而非中转注入。日志带 `request_id` 与 `content_sha`，排查时可凭此
/// 定位具体消息块并证明可疑内容来自入站数据。每请求最多记录 8 条，避免日志爆量。
fn log_injection_scan(request_id: &str, payload: &MessagesRequest) {
    if !injection_scan::is_enabled() {
        return;
    }
    let findings = injection_scan::scan_request(payload);
    if findings.is_empty() {
        return;
    }
    tracing::warn!(
        request_id = %request_id,
        hit_count = findings.len(),
        "检测到潜在 prompt injection（来自入站内容，非中转注入）"
    );
    for f in findings.iter().take(8) {
        tracing::warn!(
            request_id = %request_id,
            rule = %f.rule,
            msg_index = f.msg_index,
            role = %f.role,
            block_kind = %f.block_kind,
            tool_use_id = f.tool_use_id.as_deref().unwrap_or("-"),
            content_sha = %f.content_sha,
            snippet = %f.snippet,
            "潜在 prompt injection 命中"
        );
    }
    if findings.len() > 8 {
        tracing::warn!(
            request_id = %request_id,
            omitted = findings.len() - 8,
            "命中过多，已省略后续条目"
        );
    }
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    JsonExtractor(payload): JsonExtractor<MessagesRequest>,
) -> Response {
    // 关联 id 由 request_id_middleware 注入：优先沿用上游（newapi）带来的 request-id。
    tracing::debug!(
        request_id = %request_id,
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );

    // 入站内容的 prompt injection 启发式扫描（只记录、不拦截）。
    log_injection_scan(&request_id, &payload);

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
                ConversionError::DocumentTooLarge { .. } => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    e.to_string(),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            return (status, Json(ErrorResponse::new(error_type, message))).into_response();
        }
    };

    // 注入溯源：记录中转层实际加进上游请求的固定内容，便于排查时证明
    // 中转只加了这些（系统提示词基线 + cli 模式 env_state + origin），未参与可疑指令。
    tracing::info!(
        request_id = %request_id,
        injected_system_tokens = injected_prompt_tokens(),
        inject_env_state = provider.is_cli_mode(),
        origin = %provider.origin(),
        "中转注入溯源（仅固定内容）"
    );

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        agent_mode: Some("vibe".to_string()),
        additional_model_request_fields: conversion_result.additional_model_request_fields,
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
            return map_provider_error(e, model);
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
        cache_disabled: matches!(cache_tracker.cache_scope(), CacheScope::Off),
    };

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(model, input_tokens, thinking_enabled, tool_name_map);
    // TTFT 原点钉在「向上游发出请求」时刻，覆盖上游等首 token 的等待（见 ApiCallResult）。
    ctx.set_ttft_origin(api_result.upstream_request_at);
    ctx.set_cache_usage(cache_context);
    ctx.set_credential_id(api_result.credential_id as i64);
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
                                bytes_skipped = decoder.bytes_skipped(),
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
                            // 流结束（上游 EOF）。按「传输层如何终止」分类，避免把"干净收尾但
                            // 漏发 metering"误判成截断（上游无显式完成事件，metering 缺失只是弱信号）：
                            //   pending_bytes>0           → 切在半个帧中间 = 真截断（传输层铁证）；
                            //   pending==0 && !metering    → 已收到协议结束符、解码器无残留 = 传输完整，
                            //                                仅上游漏发 meteringEvent，内容应完整（非截断）；
                            //   pending==0 && metering     → 正常完成。
                            let pending = decoder.pending_bytes();
                            if pending > 0 {
                                tracing::warn!(
                                    model = %ctx.model,
                                    termination = "half_frame_truncated",
                                    elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                    bytes = stats.bytes,
                                    frames = stats.frames,
                                    pending_bytes = pending,
                                    saw_metering = stats.saw_metering,
                                    saw_context_usage = stats.saw_context_usage,
                                    output_tokens = ctx.output_tokens,
                                    "输出被截断：流在事件帧中间中断（解码器残留半帧），客户端会拿到半截回复，已发 error 事件触发重试"
                                );
                            } else if !stats.saw_metering {
                                tracing::warn!(
                                    model = %ctx.model,
                                    termination = "clean_eof_no_metering",
                                    elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                    bytes = stats.bytes,
                                    frames = stats.frames,
                                    pending_bytes = pending,
                                    saw_metering = stats.saw_metering,
                                    saw_context_usage = stats.saw_context_usage,
                                    output_tokens = ctx.output_tokens,
                                    "流已规整结束（收到协议结束符、无残留半帧）但上游漏发 meteringEvent：传输完整、内容应完整，仅计费元数据缺失（非截断）"
                                );
                            } else {
                                // 计费汇总由 generate_final_events 的「请求完成（流式）」承担；
                                // 此处仅记 transport 层 EOF/耗时。
                                // bytes_skipped>0：解码器在 CRC/帧长错误时跳过过字节/整帧 —— 即便
                                // 传输层干净结束，下游也可能丢了事件帧（含 tool 入参分片），是
                                // “客户端拿到残缺工具调用” 的代理侧成因；=0 则排除丢帧、指向模型收笔。
                                let skipped = decoder.bytes_skipped();
                                if skipped > 0 {
                                    tracing::warn!(
                                        model = %ctx.model,
                                        termination = "clean_eof_with_skips",
                                        elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                        bytes = stats.bytes,
                                        frames = stats.frames,
                                        frames_decoded = decoder.frames_decoded(),
                                        bytes_skipped = skipped,
                                        output_tokens = ctx.output_tokens,
                                        "上游流干净结束但解码期间跳过过字节/帧（CRC/帧长错误）：下游可能丢了事件帧，疑似代理侧截断而非模型收笔"
                                    );
                                } else {
                                    tracing::debug!(
                                        model = %ctx.model,
                                        termination = "clean_eof",
                                        elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                        bytes = stats.bytes,
                                        frames = stats.frames,
                                        bytes_skipped = skipped,
                                        output_tokens = ctx.output_tokens,
                                        "上游流正常结束（EOF）"
                                    );
                                }
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

use super::converter::{credit_to_usd, get_context_window_size, official_price_usd};

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
            return map_provider_error(e, model);
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
        cache_disabled: matches!(cache_tracker.cache_scope(), CacheScope::Off),
    };

    let body_bytes = api_result.body;

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    // 新 Kiro runtime 端点的独立思考流（reasoningContentEvent）累计：文本 + 签名。
    let mut reasoning_text = String::new();
    let mut reasoning_signature: Option<String> = None;
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
                        Event::ReasoningContent(reasoning) => {
                            reasoning_text.push_str(&reasoning.text);
                            if let Some(sig) = reasoning.signature {
                                reasoning_signature = Some(sig);
                            }
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

    if !reasoning_text.is_empty() {
        // 新端点：思考经独立 reasoningContentEvent 下发，正文是纯文本，无需标签提取。
        let mut block = json!({
            "type": "thinking",
            "thinking": reasoning_text
        });
        if let Some(sig) = &reasoning_signature {
            block["signature"] = json!(sig);
        }
        content.push(block);

        if !text_content.is_empty() {
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }
    } else if thinking_enabled {
        // 老端点：从完整文本中提取内联 `<thinking>` 块
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

    // 估算输出 tokens：tokenizer 计数（output 无上游真值兜底，本地即计费值），
    // 与流式 count_tokens 口径一致。
    let true_output_tokens = token::estimate_output_tokens(&content);
    // 套用输出上报倍率：放大后用于下游 usage 与计费口径；真实切分数仅留作日志对账。
    let output_tokens = super::converter::apply_output_token_multiplier(true_output_tokens);

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
            // 计费前扣掉 Kiro 服务端注入的固定提示词基线（与流式路径同口径）。
            // 无缓存模式（CacheScope::Off）下不扣除，按上游反推总量原样计费。
            let content_total = if matches!(cache_tracker.cache_scope(), CacheScope::Off) {
                context_total
            } else {
                super::converter::strip_injected_prompt(context_total, estimated_input_tokens)
            };
            let billed = estimated_usage.billed_split(
                estimated_input_tokens,
                content_total,
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

    // [token-diag] 临时诊断：量化「上游反推总量」与「本地仅客户端内容估算」的差值。
    // diff = 上游(含 Kiro 服务端注入 + Claude/DeepSeek 分词偏差) − 本地(纯客户端内容)。
    // 与流式路径同一口径；验证完毕后整段删除。
    if let Some(upstream_total) = context_input_tokens {
        let window = get_context_window_size(model);
        let diff = upstream_total - estimated_input_tokens;
        let diff_pct = if estimated_input_tokens > 0 {
            (diff as f64) / (estimated_input_tokens as f64) * 100.0
        } else {
            0.0
        };
        tracing::debug!(
            target: "token-diag",
            model = %model,
            window = window,
            local_estimate = estimated_input_tokens,
            upstream_total = upstream_total,
            diff = diff,
            diff_pct = format!("{:.1}%", diff_pct),
            billed_uncached = billing.uncached_input_tokens,
            billed_cache_read = billing.cache_read_input_tokens,
            billed_cache_creation = billing.cache_creation_input_tokens,
            "[token-diag] 上游反推 vs 本地估算（非流式）"
        );
    }

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
    super::billing_stats().record(actual, official, margin);
    // 请求级时序统计（非流式无 TTFT，记 0）。
    crate::stats::record(crate::stats::RequestStat {
        ts: 0,
        model: model.to_string(),
        credential_id: api_result.credential_id as i64,
        actual_micro: (actual * 1_000_000.0).round() as i64,
        official_micro: (official * 1_000_000.0).round() as i64,
        margin_micro: (margin * 1_000_000.0).round() as i64,
        input_tokens: billing.uncached_input_tokens.max(1) as i64,
        cache_read: billing.cache_read_input_tokens as i64,
        cache_creation: billing.cache_creation_input_tokens as i64,
        output_tokens: output_tokens as i64,
        ttft_ms: 0,
        elapsed_ms: request_start.elapsed().as_millis() as i64,
        status_code: 0,
    });
    tracing::info!(
        model = %model,
        input_tokens = billed_input_tokens,
        cache_read = billing.cache_read_input_tokens,
        cache_creation = billing.cache_creation_input_tokens,
        output_tokens,
        true_output_tokens,
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

    // 构建 Anthropic 响应（上报上游返回的真实 token 计数）
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": billed_input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": billing.cache_creation_input_tokens,
            "cache_read_input_tokens": billing.cache_read_input_tokens,
            "cache_creation": {
                "ephemeral_5m_input_tokens": billing.cache_creation_5m_input_tokens,
                "ephemeral_1h_input_tokens": billing.cache_creation_1h_input_tokens,
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
    Extension(RequestId(request_id)): Extension<RequestId>,
    JsonExtractor(payload): JsonExtractor<MessagesRequest>,
) -> Response {
    // 关联 id 由 request_id_middleware 注入：优先沿用上游（newapi）带来的 request-id。
    tracing::debug!(
        request_id = %request_id,
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 入站内容的 prompt injection 启发式扫描（只记录、不拦截）。
    log_injection_scan(&request_id, &payload);

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
                ConversionError::DocumentTooLarge { .. } => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    e.to_string(),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            return (status, Json(ErrorResponse::new(error_type, message))).into_response();
        }
    };

    // 注入溯源：记录中转层实际加进上游请求的固定内容，便于排查时证明
    // 中转只加了这些（系统提示词基线 + cli 模式 env_state + origin），未参与可疑指令。
    tracing::info!(
        request_id = %request_id,
        injected_system_tokens = injected_prompt_tokens(),
        inject_env_state = provider.is_cli_mode(),
        origin = %provider.origin(),
        "中转注入溯源（仅固定内容）"
    );

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        agent_mode: Some("vibe".to_string()),
        additional_model_request_fields: conversion_result.additional_model_request_fields,
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
            return map_provider_error(e, model);
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
        cache_disabled: matches!(cache_tracker.cache_scope(), CacheScope::Off),
    };

    // 创建缓冲流处理上下文
    let mut ctx = BufferedStreamContext::new(model, estimated_input_tokens, thinking_enabled, tool_name_map);
    // TTFT 原点钉在「向上游发出请求」时刻，覆盖上游等首 token 的等待（见 ApiCallResult）。
    ctx.set_ttft_origin(api_result.upstream_request_at);
    ctx.set_cache_usage(cache_context);
    ctx.set_credential_id(api_result.credential_id as i64);
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
                                // 流结束（上游 EOF）。按传输层终止方式分类（判据同直传路径）：
                                //   pending>0 = 真截断；pending==0 && !metering = 传输完整仅漏发
                                //   metering（非截断）；pending==0 && metering = 正常完成。
                                let pending = decoder.pending_bytes();
                                if pending > 0 {
                                    tracing::warn!(
                                        model = %ctx.model(),
                                        termination = "half_frame_truncated",
                                        elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                        bytes = stats.bytes,
                                        frames = stats.frames,
                                        pending_bytes = pending,
                                        saw_metering = stats.saw_metering,
                                        saw_context_usage = stats.saw_context_usage,
                                        output_tokens = ctx.output_tokens(),
                                        "输出被截断（缓冲模式）：流在事件帧中间中断（解码器残留半帧），已发 error 事件触发重试"
                                    );
                                } else if !stats.saw_metering {
                                    tracing::warn!(
                                        model = %ctx.model(),
                                        termination = "clean_eof_no_metering",
                                        elapsed_secs = stats.start.elapsed().as_secs_f64(),
                                        bytes = stats.bytes,
                                        frames = stats.frames,
                                        pending_bytes = pending,
                                        saw_metering = stats.saw_metering,
                                        saw_context_usage = stats.saw_context_usage,
                                        output_tokens = ctx.output_tokens(),
                                        "流已规整结束（收到协议结束符、无残留半帧）但上游漏发 meteringEvent：传输完整、内容应完整，仅计费元数据缺失（非截断）"
                                    );
                                } else {
                                    // 计费汇总由 inner 的「请求完成（流式）」承担；此处仅记
                                    // transport 层 EOF/耗时，降级 debug 避免每请求双 info 行。
                                    tracing::debug!(
                                        model = %ctx.model(),
                                        termination = "clean_eof",
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

#[cfg(test)]
mod truncation_tests {
    //! 复现客户端 “Write Failed / 会话卡死”：上游在事件帧中间断流时，代理应发
    //! `event: error`（[`create_truncation_error_sse`]）让客户端重试，而**不是**伪造
    //! 一个干净的 `message_stop`。靠提示词无法触发（模型不会在单个 tool 入参里吐
    //! 几 MB 文本），故用本地 mock 上游：发「一个完整帧 + 半个帧」后直接关连接，
    //! 走真实的 [`create_sse_stream`] EOF-残留半帧（`pending_bytes>0`）路径。

    use super::*;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 编码一个 String 类型的 event-stream 头部：`[name_len][name][type=7][val_len_be16][val]`
    fn header_str(name: &str, value: &str) -> Vec<u8> {
        let mut h = Vec::new();
        h.push(name.len() as u8);
        h.extend_from_slice(name.as_bytes());
        h.push(7u8); // HeaderValueType::String
        h.extend_from_slice(&(value.len() as u16).to_be_bytes());
        h.extend_from_slice(value.as_bytes());
        h
    }

    /// 构造一个合法的 AWS event-stream 帧（prelude + headers + payload + msg_crc，CRC 均正确）。
    fn build_event_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        use crate::kiro::parser::crc::crc32;
        use crate::kiro::parser::frame::PRELUDE_SIZE;

        let mut headers = Vec::new();
        headers.extend_from_slice(&header_str(":event-type", event_type));
        headers.extend_from_slice(&header_str(":content-type", "application/json"));
        headers.extend_from_slice(&header_str(":message-type", "event"));

        let header_length = headers.len() as u32;
        let total_length = (PRELUDE_SIZE + headers.len() + payload.len() + 4) as u32;

        let mut msg = Vec::new();
        msg.extend_from_slice(&total_length.to_be_bytes());
        msg.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&msg[..8]);
        msg.extend_from_slice(&prelude_crc.to_be_bytes());
        msg.extend_from_slice(&headers);
        msg.extend_from_slice(payload);
        let message_crc = crc32(&msg);
        msg.extend_from_slice(&message_crc.to_be_bytes());
        msg
    }

    /// 起一个只服务一次的 mock 上游：写完 `body` 后立刻关闭连接（无 Content-Length，
    /// 以 connection-close 界定 body → reqwest 读到干净 EOF）。返回监听地址。
    async fn spawn_mock_upstream(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // 先把请求读掉，避免未读数据导致 close 发出 RST。
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let mut resp = Vec::new();
            resp.extend_from_slice(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: application/vnd.amazon.eventstream\r\n\
                  Connection: close\r\n\r\n",
            );
            resp.extend_from_slice(&body);
            let _ = socket.write_all(&resp).await;
            let _ = socket.flush().await;
            // socket drop → FIN，制造流中途的 EOF。
        });
        format!("http://{addr}/")
    }

    /// 收集 SSE 输出流为字符串。
    async fn collect_sse(
        stream: impl Stream<Item = Result<Bytes, Infallible>>,
    ) -> String {
        futures::pin_mut!(stream);
        let mut out = String::new();
        while let Some(Ok(bytes)) = stream.next().await {
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
        out
    }

    /// 上游在事件帧中间断流（EOF 时解码器残留半帧）→ 代理应发 `event: error`、
    /// 不发 `message_stop`，即客户端看到的 “Write Failed / 回复中断、需重试”。
    #[tokio::test]
    async fn write_failed_on_mid_frame_truncation() {
        // body = 一个完整的 assistantResponseEvent（产生部分正文）+ 第二帧的前半截。
        let full = build_event_frame(
            "assistantResponseEvent",
            br#"{"content":"Hello, this is a partial reply"}"#,
        );
        let frame2 = build_event_frame(
            "assistantResponseEvent",
            br#"{"content":" that gets cut off mid-stream"}"#,
        );
        let half = &frame2[..frame2.len() - 12]; // 切在帧中间，prelude 之后缺尾

        let mut body = full.clone();
        body.extend_from_slice(half);

        let url = spawn_mock_upstream(body).await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();

        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-5", 10, false, HashMap::new());
        let initial = ctx.generate_initial_events();
        let out = collect_sse(create_sse_stream(resp, ctx, initial)).await;

        // 1) 部分正文应已下发给客户端
        assert!(out.contains("Hello, this is a partial reply"), "缺少已下发的部分正文:\n{out}");
        // 2) 断流应转成 error 事件（Write Failed 的触发源）
        assert!(out.contains("event: error"), "未发 error 事件:\n{out}");
        assert!(out.contains("truncated"), "error 文案应说明被截断:\n{out}");
        // 3) 关键：绝不能补干净收尾，否则客户端把半截当成功收下、不重试
        assert!(!out.contains("message_stop"), "真截断时不应发 message_stop:\n{out}");
        assert!(
            !out.contains("\"stop_reason\":\"end_turn\""),
            "真截断时不应伪造 end_turn:\n{out}"
        );
    }

    /// 构造一个 toolUseEvent 帧（name/toolUseId/input 分片/stop）。
    fn build_tool_frame(name: &str, id: &str, input_fragment: &str, stop: bool) -> Vec<u8> {
        let payload = serde_json::json!({
            "name": name,
            "toolUseId": id,
            "input": input_fragment,
            "stop": stop,
        })
        .to_string();
        build_event_frame("toolUseEvent", payload.as_bytes())
    }

    /// 忠实性验证：上游把一段**完整**的 tool 入参 JSON 切成多个 toolUseEvent 分片
    /// 逐个下发（最后一个 stop=true），代理必须把所有分片原样拼回——一个字节都不能丢。
    /// 若本测试通过，则“客户端拿到残缺入参”不是代理在转发链路上丢分片造成的，
    /// 而是上游/模型本身就只发了残缺入参。
    #[tokio::test]
    async fn fragmented_tool_input_is_forwarded_intact() {
        // 一段完整、合法的写文件入参，含中文路径与较长 content（模拟真实写文件调用）。
        let full_input = r#"{"file_path": "d:/AI文档设计/M1-退件列表-PC.html", "content": "<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head><body><h1>退件列表</h1><table><tr><td>1</td></tr></table></body></html>"}"#;

        // 按字符边界切成 11 段（含可能切在转义序列、中文字节、JSON token 中间的情况）。
        let chunks: Vec<&str> = {
            let mut v = Vec::new();
            let bytes = full_input.as_bytes();
            let mut start = 0;
            let step = bytes.len() / 11;
            while start < full_input.len() {
                let mut end = (start + step).min(full_input.len());
                while end < full_input.len() && !full_input.is_char_boundary(end) {
                    end += 1;
                }
                v.push(&full_input[start..end]);
                start = end;
            }
            v
        };

        let mut body = Vec::new();
        let id = "tooluse_w23qiWBrnqCYuzKmftW2xv";
        for (i, frag) in chunks.iter().enumerate() {
            let stop = i == chunks.len() - 1;
            body.extend_from_slice(&build_tool_frame("Write", id, frag, stop));
        }

        let url = spawn_mock_upstream(body).await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();

        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-5", 10, false, HashMap::new());
        let initial = ctx.generate_initial_events();
        let out = collect_sse(create_sse_stream(resp, ctx, initial)).await;

        let reassembled = reassemble_tool_input(&out);
        assert_eq!(
            reassembled, full_input,
            "分片转发必须无损：拼回的入参与原始入参逐字节一致"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&reassembled).is_ok(),
            "拼回的入参应是合法 JSON"
        );
        assert!(out.contains("\"stop_reason\":\"tool_use\""), "应为 tool_use 收尾:\n{out}");
        assert!(out.contains("message_stop"));
    }

    /// 对照组：完整帧 + 协议结束符（无残留半帧）→ 正常收尾，应有 message_stop、无 error。
    #[tokio::test]
    async fn clean_finish_emits_message_stop() {
        let full = build_event_frame(
            "assistantResponseEvent",
            br#"{"content":"complete reply"}"#,
        );
        let url = spawn_mock_upstream(full).await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();

        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-5", 10, false, HashMap::new());
        let initial = ctx.generate_initial_events();
        let out = collect_sse(create_sse_stream(resp, ctx, initial)).await;

        assert!(out.contains("complete reply"), "缺少正文:\n{out}");
        assert!(out.contains("message_stop"), "正常结束应有 message_stop:\n{out}");
        assert!(!out.contains("event: error"), "正常结束不应有 error 事件:\n{out}");
    }

    /// 把 SSE 输出里某个 tool_use 块的 input_json_delta 拼接还原成入参 JSON 字符串。
    fn reassemble_tool_input(sse: &str) -> String {
        sse.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|j| serde_json::from_str::<serde_json::Value>(j).ok())
            .filter(|v| v["type"] == "content_block_delta" && v["delta"]["type"] == "input_json_delta")
            .filter_map(|v| v["delta"]["partial_json"].as_str().map(String::from))
            .collect()
    }

    /// 修复验证 —— “Write failed / 工具参数缺失” 的正解：上游发来一个 **入参残缺**
    /// （只有 file_path、缺必填 content、JSON 未闭合）但 **stop=true** 的 toolUseEvent，
    /// 随后干净结束。代理收尾时检测到该 tool 块入参非空且非法 JSON，按官方契约把
    /// stop_reason 改为 **max_tokens**（而非 tool_use）——流仍干净收尾，但客户端据此
    /// 知道该工具调用不完整、不会拿去执行。
    #[tokio::test]
    async fn incomplete_tool_input_sets_max_tokens_stop() {
        // toolUseEvent.input 是“入参 JSON 的字符串片段”。这里给一段残缺的：缺 content、未闭合。
        let tool_payload = serde_json::json!({
            "name": "Write",
            "toolUseId": "tooluse_w23qiWBrnqCYuzKmftW2xv",
            "input": r#"{"file_path": "d:/demo/M1-退件列表-PC.html""#,
            "stop": true
        })
        .to_string();
        let body = build_event_frame("toolUseEvent", tool_payload.as_bytes());

        let url = spawn_mock_upstream(body).await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();

        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-5", 10, false, HashMap::new());
        let initial = ctx.generate_initial_events();
        let out = collect_sse(create_sse_stream(resp, ctx, initial)).await;

        // 前提：客户端拿到的入参确实是“非法 JSON、缺 content”
        let tool_input = reassemble_tool_input(&out);
        assert_eq!(tool_input, r#"{"file_path": "d:/demo/M1-退件列表-PC.html""#);
        assert!(serde_json::from_str::<serde_json::Value>(&tool_input).is_err());

        // 修复后的行为：干净收尾，但 stop_reason=max_tokens（不是 tool_use）→ 客户端不执行该残缺调用。
        assert!(out.contains("\"name\":\"Write\""), "应有 Write 工具块:\n{out}");
        assert!(out.contains("content_block_stop"), "工具块仍被干净关闭:\n{out}");
        assert!(
            out.contains("\"stop_reason\":\"max_tokens\""),
            "残缺入参收尾应置 stop_reason=max_tokens:\n{out}"
        );
        assert!(
            !out.contains("\"stop_reason\":\"tool_use\""),
            "不应再标成 tool_use（那会让客户端执行残缺调用）:\n{out}"
        );
        assert!(out.contains("message_stop"), "干净 message_stop:\n{out}");
        assert!(!out.contains("event: error"), "干净 EOF 不应发断流 error:\n{out}");
    }
}
