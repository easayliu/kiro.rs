//! Admin API HTTP 处理器

use axum::{
    Extension, Json,
    extract::{Path, State},
    response::IntoResponse,
};

use super::{
    middleware::{AdminRole, AdminState},
    types::{
        AddCredentialRequest, BatchSetCredentialGroupRequest, BatchSetDisabledRequest,
        BatchSetOverageRequest, BatchSetPriorityRequest, BatchSetRpmLimitRequest, MeResponse,
        SetCacheSkipRateRequest,
        SetCredentialGroupRequest, SetDefaultRpmLimitRequest, SetDisabledRequest,
        SetGlobalCacheRequest, SetLoadBalancingModeRequest, SetOverageRequest, SetPriorityRequest,
        SetRpmLimitRequest, SuccessResponse, UpsertProxyGroupRequest,
    },
};

/// GET /api/admin/me
/// 返回当前调用方角色
pub async fn get_me(Extension(role): Extension<AdminRole>) -> impl IntoResponse {
    Json(MeResponse { role: role.as_str() })
}

/// GET /api/admin/credentials
/// 获取所有凭据状态
pub async fn get_all_credentials(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_all_credentials();
    Json(response)
}

/// POST /api/admin/credentials/:id/disabled
/// 设置凭据禁用状态
pub async fn set_credential_disabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetDisabledRequest>,
) -> impl IntoResponse {
    match state.service.set_disabled(id, payload.disabled) {
        Ok(_) => {
            let action = if payload.disabled { "禁用" } else { "启用" };
            Json(SuccessResponse::new(format!("凭据 #{} 已{}", id, action))).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/priority
/// 设置凭据优先级
pub async fn set_credential_priority(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetPriorityRequest>,
) -> impl IntoResponse {
    match state.service.set_priority(id, payload.priority) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 优先级已设置为 {}",
            id, payload.priority
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/rpm-limit
/// 设置凭据级 RPM 上限
pub async fn set_credential_rpm_limit(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetRpmLimitRequest>,
) -> impl IntoResponse {
    match state.service.set_rpm_limit(id, payload.rpm_limit) {
        Ok(_) => Json(SuccessResponse::new(match payload.rpm_limit {
            None => format!("凭据 #{} RPM 上限已清除（回退到全局默认）", id),
            Some(0) => format!("凭据 #{} 已显式不限流", id),
            Some(n) => format!("凭据 #{} RPM 上限已设置为 {} 次/分钟", id, n),
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/overage
/// 切换凭据的 overage（超额计费）开关
pub async fn set_credential_overage(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetOverageRequest>,
) -> impl IntoResponse {
    match state.service.set_overage(id, payload.enabled).await {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} overage 已切换为 {}",
            id,
            if payload.enabled { "ENABLED" } else { "DISABLED" }
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/overage/batch
/// 批量切换 overage（超额计费）开关（顺序排队处理）
pub async fn batch_set_overage(
    State(state): State<AdminState>,
    Json(payload): Json<BatchSetOverageRequest>,
) -> impl IntoResponse {
    match state.service.batch_set_overage(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/reset
/// 重置失败计数并重新启用
pub async fn reset_failure_count(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.reset_and_enable(id) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 失败计数已重置并重新启用",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/balance
/// 获取指定凭据的余额
pub async fn get_credential_balance(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.get_balance(id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials
/// 添加新凭据
pub async fn add_credential(
    State(state): State<AdminState>,
    Json(payload): Json<AddCredentialRequest>,
) -> impl IntoResponse {
    match state.service.add_credential(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/credentials/:id
/// 删除凭据
pub async fn delete_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_credential(id) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已删除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/refresh
/// 强制刷新凭据 Token
pub async fn force_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.force_refresh_token(id).await {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} Token 已强制刷新",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/load-balancing
/// 获取负载均衡模式
pub async fn get_load_balancing_mode(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_load_balancing_mode();
    Json(response)
}

/// PUT /api/admin/config/load-balancing
/// 设置负载均衡模式
pub async fn set_load_balancing_mode(
    State(state): State<AdminState>,
    Json(payload): Json<SetLoadBalancingModeRequest>,
) -> impl IntoResponse {
    match state.service.set_load_balancing_mode(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/global-cache
/// 获取全局缓存模式
pub async fn get_global_cache(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_global_cache();
    Json(response)
}

/// PUT /api/admin/config/global-cache
/// 设置全局缓存模式
pub async fn set_global_cache(
    State(state): State<AdminState>,
    Json(payload): Json<SetGlobalCacheRequest>,
) -> impl IntoResponse {
    match state.service.set_global_cache(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/cache-scope
/// 获取缓存分桶策略
pub async fn get_cache_scope(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_cache_scope();
    Json(response)
}

/// PUT /api/admin/config/cache-scope
/// 设置缓存分桶策略（"global" / "per_credential"）
pub async fn set_cache_scope(
    State(state): State<AdminState>,
    Json(payload): Json<crate::admin::types::SetCacheScopeRequest>,
) -> impl IntoResponse {
    match state.service.set_cache_scope(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/cache-skip-rate
/// 获取缓存查找跳过率
pub async fn get_cache_skip_rate(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_cache_skip_rate();
    Json(response)
}

/// PUT /api/admin/config/cache-skip-rate
/// 设置缓存查找跳过率（0.0-1.0，传 null 关闭）
pub async fn set_cache_skip_rate(
    State(state): State<AdminState>,
    Json(payload): Json<SetCacheSkipRateRequest>,
) -> impl IntoResponse {
    match state.service.set_cache_skip_rate(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/proxy-groups
/// 列出所有代理分组
pub async fn list_proxy_groups(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.list_proxy_groups())
}

/// PUT /api/admin/config/proxy-groups/:name
/// 新增或更新指定代理分组
pub async fn upsert_proxy_group(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(payload): Json<UpsertProxyGroupRequest>,
) -> impl IntoResponse {
    match state.service.upsert_proxy_group(name.clone(), payload) {
        Ok(_) => Json(SuccessResponse::new(format!("代理分组 '{}' 已保存", name)))
            .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/config/proxy-groups/:name
/// 删除指定代理分组（引用该分组的凭据回退到全局代理）
pub async fn delete_proxy_group(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.service.delete_proxy_group(&name) {
        Ok(_) => Json(SuccessResponse::new(format!("代理分组 '{}' 已删除", name)))
            .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/group
/// 设置凭据所属代理分组（传 null/空表示清空）
pub async fn set_credential_group(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetCredentialGroupRequest>,
) -> impl IntoResponse {
    let group_label = payload
        .group
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(String::from);
    match state.service.set_credential_group(id, payload) {
        Ok(_) => {
            let msg = match group_label {
                Some(g) => format!("凭据 #{} 已绑定到代理分组 '{}'", id, g),
                None => format!("凭据 #{} 已清空代理分组绑定", id),
            };
            Json(SuccessResponse::new(msg)).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/priority/batch
/// 批量设置凭据优先级
pub async fn batch_set_priority(
    State(state): State<AdminState>,
    Json(payload): Json<BatchSetPriorityRequest>,
) -> impl IntoResponse {
    match state.service.batch_set_priority(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/disabled/batch
/// 批量启用/禁用凭据
pub async fn batch_set_disabled(
    State(state): State<AdminState>,
    Json(payload): Json<BatchSetDisabledRequest>,
) -> impl IntoResponse {
    match state.service.batch_set_disabled(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/rpm-limit/batch
/// 批量设置凭据级 RPM 上限
pub async fn batch_set_rpm_limit(
    State(state): State<AdminState>,
    Json(payload): Json<BatchSetRpmLimitRequest>,
) -> impl IntoResponse {
    match state.service.batch_set_rpm_limit(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/default-rpm-limit
pub async fn get_default_rpm_limit(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_default_rpm_limit())
}

/// PUT /api/admin/config/default-rpm-limit
pub async fn set_default_rpm_limit(
    State(state): State<AdminState>,
    Json(payload): Json<SetDefaultRpmLimitRequest>,
) -> impl IntoResponse {
    match state.service.set_default_rpm_limit(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/group/batch
/// 批量设置凭据所属代理分组（传 null/空 group 表示清空）
pub async fn batch_set_credential_group(
    State(state): State<AdminState>,
    Json(payload): Json<BatchSetCredentialGroupRequest>,
) -> impl IntoResponse {
    match state.service.batch_set_credential_group(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}
