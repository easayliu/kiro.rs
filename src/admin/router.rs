//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post, put},
};

use super::{
    handlers::{
        add_credential, batch_set_concurrency_limit, batch_set_credential_group,
        batch_set_disabled, batch_set_overage,
        batch_set_priority, batch_set_rpm_limit, delete_credential, delete_proxy_group,
        force_refresh_token,
        get_all_credentials, get_billing_stats,
        get_cache_scope, get_cache_skip_rate, get_credential_balance, get_credential_models,
        get_default_concurrency_limit, get_default_rpm_limit,
        get_global_cache, get_load_balancing_mode, get_me, list_proxy_groups,
        reset_failure_count,
        set_cache_scope, set_cache_skip_rate, set_credential_concurrency_limit,
        set_credential_disabled, set_credential_group,
        set_credential_overage, set_credential_priority, set_credential_rpm_limit,
        set_default_concurrency_limit, set_default_rpm_limit, set_global_cache,
        set_load_balancing_mode, upsert_proxy_group,
    },
    middleware::{AdminState, admin_auth_middleware},
};

/// 创建 Admin API 路由
///
/// # 端点
/// - `GET /me` - 返回当前调用方角色（admin / guest）
/// - `GET /billing-stats` - 进程维度累计的实际成本 / 官方价 / 毛利汇总
/// - `GET /credentials` - 获取所有凭据状态
/// - `POST /credentials` - 添加新凭据
/// - `DELETE /credentials/:id` - 删除凭据
/// - `POST /credentials/:id/disabled` - 设置凭据禁用状态
/// - `POST /credentials/:id/priority` - 设置凭据优先级
/// - `POST /credentials/:id/reset` - 重置失败计数
/// - `POST /credentials/:id/refresh` - 强制刷新 Token
/// - `GET /credentials/:id/balance` - 获取凭据余额
/// - `GET /credentials/:id/models` - 查询凭据上游可用模型列表
/// - `GET /config/load-balancing` - 获取负载均衡模式
/// - `PUT /config/load-balancing` - 设置负载均衡模式
/// - `GET /config/global-cache` - 获取全局缓存模式
/// - `PUT /config/global-cache` - 设置全局缓存模式
/// - `GET /config/proxy-groups` - 列出所有代理分组
/// - `PUT /config/proxy-groups/:name` - 新增/更新代理分组
/// - `DELETE /config/proxy-groups/:name` - 删除代理分组
/// - `POST /credentials/:id/group` - 设置凭据所属代理分组
/// - `POST /credentials/group/batch` - 批量设置凭据所属代理分组
///
/// # 认证
/// 需要 Admin API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
pub fn create_admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/me", get(get_me))
        .route("/billing-stats", get(get_billing_stats))
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route("/credentials/{id}/rpm-limit", post(set_credential_rpm_limit))
        .route(
            "/credentials/{id}/concurrency-limit",
            post(set_credential_concurrency_limit),
        )
        .route("/credentials/{id}/overage", post(set_credential_overage))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        .route("/credentials/{id}/models", get(get_credential_models))
        .route("/credentials/{id}/group", post(set_credential_group))
        .route("/credentials/group/batch", post(batch_set_credential_group))
        .route("/credentials/priority/batch", post(batch_set_priority))
        .route("/credentials/rpm-limit/batch", post(batch_set_rpm_limit))
        .route(
            "/credentials/concurrency-limit/batch",
            post(batch_set_concurrency_limit),
        )
        .route("/credentials/overage/batch", post(batch_set_overage))
        .route("/credentials/disabled/batch", post(batch_set_disabled))
        .route(
            "/config/default-rpm-limit",
            get(get_default_rpm_limit).put(set_default_rpm_limit),
        )
        .route(
            "/config/default-concurrency-limit",
            get(get_default_concurrency_limit).put(set_default_concurrency_limit),
        )
        .route(
            "/config/load-balancing",
            get(get_load_balancing_mode).put(set_load_balancing_mode),
        )
        .route(
            "/config/global-cache",
            get(get_global_cache).put(set_global_cache),
        )
        .route(
            "/config/cache-scope",
            get(get_cache_scope).put(set_cache_scope),
        )
        .route(
            "/config/cache-skip-rate",
            get(get_cache_skip_rate).put(set_cache_skip_rate),
        )
        .route("/config/proxy-groups", get(list_proxy_groups))
        .route(
            "/config/proxy-groups/{name}",
            put(upsert_proxy_group).delete(delete_proxy_group),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ))
        .with_state(state)
}
