//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use uuid::Uuid;

use crate::common::auth;
use crate::kiro::binding::BindingTable;
use crate::kiro::provider::KiroProvider;

use super::cache_tracker::CacheTracker;
use super::types::ErrorResponse;

/// Prompt cache 默认最大 TTL（1 小时）
const DEFAULT_PROMPT_CACHE_TTL_SECS: u64 = 3600;

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// API 密钥
    pub api_key: String,
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// Prompt caching 本地追踪器（按 credential_id 维度分片）
    pub cache_tracker: Arc<CacheTracker>,
    /// 用户 → 凭证粘性绑定表（内存版）
    ///
    /// 跨凭证场景下让同一用户的请求持续落在同一上游，避免上游 prompt cache
    /// 在多凭证间反复预热。进程重启会清空绑定（首轮请求会重新选凭证）。
    pub binding_table: Arc<BindingTable>,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new(
        api_key: impl Into<String>,
        extract_thinking: bool,
        cache_scope: super::CacheScope,
        cache_skip_rate: Option<f32>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            kiro_provider: None,
            extract_thinking,
            cache_tracker: Arc::new(CacheTracker::new(
                std::time::Duration::from_secs(DEFAULT_PROMPT_CACHE_TTL_SECS),
                cache_scope,
                cache_skip_rate,
            )),
            binding_table: Arc::new(BindingTable::new()),
        }
    }

    /// 设置 KiroProvider
    pub fn with_kiro_provider(mut self, provider: KiroProvider) -> Self {
        self.kiro_provider = Some(Arc::new(provider));
        self
    }
}

/// API Key 认证中间件
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    match auth::extract_api_key(&request) {
        Some(key) if auth::constant_time_eq(&key, &state.api_key) => next.run(request).await,
        _ => {
            let error = ErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}

/// 请求关联 id：优先沿用上游（如 newapi）带来的 request-id，否则生成新 UUID。
///
/// 由 [`request_id_middleware`] 注入到请求扩展，handler 用 `Extension<RequestId>` 取出，
/// 全链路日志据此与上游对齐排查。
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

/// 候选入站 request-id 头（按优先级）。newapi/one-api 系默认用 `x-request-id`。
const REQUEST_ID_HEADERS: &[&str] = &[
    "x-request-id",
    "x-oneapi-request-id",
    "x-new-api-request-id",
    "request-id",
    "x-trace-id",
];

/// 从入站头中解析 request-id：取首个非空、可打印、长度 ≤200 的候选头，否则生成 UUID。
fn resolve_request_id(headers: &HeaderMap) -> String {
    for name in REQUEST_ID_HEADERS {
        if let Some(value) = headers.get(*name).and_then(|v| v.to_str().ok()) {
            let value = value.trim();
            if !value.is_empty()
                && value.len() <= 200
                && value.chars().all(|c| !c.is_control())
            {
                return value.to_string();
            }
        }
    }
    Uuid::new_v4().to_string()
}

/// request-id 中间件：解析/生成关联 id 注入请求扩展，并在响应回写 `x-request-id`，
/// 实现上游 request-id 的透传保留（请求侧沿用、响应侧回显）。
pub async fn request_id_middleware(mut request: Request<Body>, next: Next) -> Response {
    let id = resolve_request_id(request.headers());
    request.extensions_mut().insert(RequestId(id.clone()));
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn preserves_upstream_request_id() {
        let h = headers(&[("x-request-id", "4c4f9fad-d444-430c-a502-2ea5535df668")]);
        assert_eq!(resolve_request_id(&h), "4c4f9fad-d444-430c-a502-2ea5535df668");
    }

    #[test]
    fn follows_header_priority() {
        let h = headers(&[("x-trace-id", "trace"), ("x-request-id", "primary")]);
        assert_eq!(resolve_request_id(&h), "primary");
    }

    #[test]
    fn falls_back_to_uuid_when_absent_or_invalid() {
        // 缺失 → 生成 UUID（36 字符）
        assert_eq!(resolve_request_id(&HeaderMap::new()).len(), 36);
        // 空白 → 回退
        assert_eq!(resolve_request_id(&headers(&[("x-request-id", "   ")])).len(), 36);
        // 超长 → 回退
        let long = "a".repeat(201);
        assert_eq!(resolve_request_id(&headers(&[("x-request-id", &long)])).len(), 36);
    }
}
