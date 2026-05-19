//! Admin API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use super::service::AdminService;
use super::types::AdminErrorResponse;
use crate::common::auth;

/// 调用方角色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminRole {
    /// 完整管理权限
    Admin,
    /// 只读游客
    Guest,
}

impl AdminRole {
    pub fn as_str(self) -> &'static str {
        match self {
            AdminRole::Admin => "admin",
            AdminRole::Guest => "guest",
        }
    }
}

/// Admin API 共享状态
#[derive(Clone)]
pub struct AdminState {
    /// Admin API 密钥
    pub admin_api_key: String,
    /// 游客 API 密钥列表（仅授予只读权限）
    pub guest_api_keys: Arc<Vec<String>>,
    /// Admin 服务
    pub service: Arc<AdminService>,
}

impl AdminState {
    pub fn new(
        admin_api_key: impl Into<String>,
        guest_api_keys: Vec<String>,
        service: AdminService,
    ) -> Self {
        Self {
            admin_api_key: admin_api_key.into(),
            guest_api_keys: Arc::new(guest_api_keys),
            service: Arc::new(service),
        }
    }
}

fn match_guest(state: &AdminState, key: &str) -> bool {
    state
        .guest_api_keys
        .iter()
        .any(|g| !g.is_empty() && auth::constant_time_eq(key, g))
}

/// Admin API 认证中间件
///
/// - admin key 命中：完整权限，任何方法
/// - guest key 命中：只允许只读方法（GET/HEAD），其他返回 403
/// - 不匹配：401
pub async fn admin_auth_middleware(
    State(state): State<AdminState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let api_key = auth::extract_api_key(&request);

    let role = match api_key.as_deref() {
        Some(key) if auth::constant_time_eq(key, &state.admin_api_key) => AdminRole::Admin,
        Some(key) if match_guest(&state, key) => AdminRole::Guest,
        _ => {
            let error = AdminErrorResponse::authentication_error();
            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
        }
    };

    // 游客只允许只读方法
    if role == AdminRole::Guest && !is_read_only_method(request.method()) {
        let error = AdminErrorResponse::forbidden("Guest 角色仅允许只读访问");
        return (StatusCode::FORBIDDEN, Json(error)).into_response();
    }

    // 把角色注入到 request extensions，供后续 handler 使用
    request.extensions_mut().insert(role);
    next.run(request).await
}

fn is_read_only_method(method: &Method) -> bool {
    matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS)
}
