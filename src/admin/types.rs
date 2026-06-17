//! Admin API 类型定义

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::config::{ClientMode, ProxyGroupConfig};

// ============ 当前用户 ============

/// 当前调用方信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeResponse {
    /// 角色：`admin` 拥有完整权限，`guest` 仅可只读
    pub role: &'static str,
}

// ============ 凭据状态 ============

/// 所有凭据状态响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsStatusResponse {
    /// 凭据总数
    pub total: usize,
    /// 可用凭据数量（未禁用）
    pub available: usize,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 各凭据状态列表
    pub credentials: Vec<CredentialStatusItem>,
    /// 全局默认 RPM 上限（None=未配置）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_rpm_limit: Option<u32>,
    /// 全局默认并发上限（None=未配置）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_concurrency_limit: Option<u32>,
}

/// 单个凭据的状态信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatusItem {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级（数字越小优先级越高）
    pub priority: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 是否为当前活跃凭据
    pub is_current: bool,
    /// Token 过期时间（RFC3339 格式）
    pub expires_at: Option<String>,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// refreshToken 的 SHA-256 哈希（用于前端重复检测）
    pub refresh_token_hash: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// 凭据所属代理分组名（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 上游 429 冷却到期时间（RFC3339）；None=未在冷却
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throttled_until: Option<String>,
    /// 凭据级 RPM 上限覆盖（None=未单独配置；0=显式不限流）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,
    /// 最近 60s 滑动窗口内的请求数（用于前端展示 X/limit）
    #[serde(default)]
    pub rpm_current: u32,
    /// 凭据级并发上限覆盖（None=未单独配置；0=显式不限并发）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency_limit: Option<u32>,
    /// 当前在途请求数（用于前端展示 X/limit）
    #[serde(default)]
    pub concurrency_current: u32,
    /// overage（超额计费）上次下发状态（None=从未下发，前端显示为未知）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage: Option<bool>,
}

// ============ 操作请求 ============

/// 启用/禁用凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDisabledRequest {
    /// 是否禁用
    pub disabled: bool,
}

/// 修改优先级请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPriorityRequest {
    /// 新优先级值
    pub priority: u32,
}

/// 修改凭据级 RPM 上限请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRpmLimitRequest {
    /// 新 RPM 上限值
    /// - None：清除凭据级覆盖，回退到全局默认
    /// - Some(0)：显式不限流
    /// - Some(n>0)：限制为 n 次/分钟
    pub rpm_limit: Option<u32>,
}

/// 切换 overage（超额计费）开关请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetOverageRequest {
    /// true=ENABLED，false=DISABLED
    pub enabled: bool,
}

/// 批量切换 overage 开关请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetOverageRequest {
    pub credential_ids: Vec<u64>,
    /// true=ENABLED，false=DISABLED
    pub enabled: bool,
}

/// 批量切换 overage 开关响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetOverageResponse {
    pub total: usize,
    pub succeeded: Vec<u64>,
    pub failed: Vec<BatchSetCredentialGroupFailure>,
}

/// 添加凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialRequest {
    /// 刷新令牌（OAuth 凭据必填，API Key 凭据不需要）
    pub refresh_token: Option<String>,

    /// 认证方式（可选，默认 social）
    #[serde(default = "default_auth_method")]
    pub auth_method: String,

    /// OIDC Client ID（IdC 认证需要）
    pub client_id: Option<String>,

    /// OIDC Client Secret（IdC 认证需要）
    pub client_secret: Option<String>,

    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    pub api_region: Option<String>,

    /// 凭据级 Machine ID（可选，64 位字符串）
    /// 未配置时回退到 config.json 的 machineId
    pub machine_id: Option<String>,

    /// 用户邮箱（可选，用于前端显示）
    pub email: Option<String>,

    /// 凭据级代理 URL（可选，特殊值 "direct" 表示不使用代理）
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    pub proxy_password: Option<String>,

    /// 凭据所属代理分组名称（可选）
    pub group: Option<String>,

    /// Kiro API Key（API Key 凭据必填，格式: ksk_xxxxxxxx）
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 凭据级客户端模拟模式（可选，"kiro-ide" 或 "kiro-cli"）
    pub client_mode: Option<ClientMode>,
}

fn default_auth_method() -> String {
    "social".to_string()
}

/// 添加凭据成功响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialResponse {
    pub success: bool,
    pub message: String,
    /// 新添加的凭据 ID
    pub credential_id: u64,
    /// 用户邮箱（如果获取成功）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

// ============ 余额查询 ============

/// 余额查询响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    /// 凭据 ID
    pub id: u64,
    /// 订阅类型
    pub subscription_title: Option<String>,
    /// 当前使用量
    pub current_usage: f64,
    /// 使用限额
    pub usage_limit: f64,
    /// 剩余额度
    pub remaining: f64,
    /// 使用百分比
    pub usage_percentage: f64,
    /// 下次重置时间（Unix 时间戳）
    pub next_reset_at: Option<f64>,
    /// 超额计费状态（ENABLED / DISABLED，上游真实下发）
    pub overage_status: Option<String>,
    /// 当前超额用量（已越过额度的部分）
    pub current_overages: f64,
    /// 已产生的超额费用
    pub overage_charges: f64,
    /// 超额单价（每单位费用）
    pub overage_rate: f64,
    /// 超额上限
    pub overage_cap: f64,
    /// 货币（如 USD）
    pub currency: Option<String>,
}

// ============ 可用模型查询 ============

/// 凭据可用模型查询响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsResponse {
    /// 凭据 ID
    pub id: u64,
    /// 上游 ListAvailableModels 返回的模型 id 列表（原样透传）
    pub models: Vec<String>,
}

// ============ 负载均衡配置 ============

/// 负载均衡模式响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancingModeResponse {
    /// 当前模式（"priority" 或 "balanced"）
    pub mode: String,
}

/// 设置负载均衡模式请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLoadBalancingModeRequest {
    /// 模式（"priority" 或 "balanced"）
    pub mode: String,
}

// ============ 全局缓存配置 ============

/// 全局缓存模式响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalCacheResponse {
    /// 是否启用全局缓存
    pub enabled: bool,
}

/// 设置全局缓存模式请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetGlobalCacheRequest {
    /// 是否启用全局缓存
    pub enabled: bool,
}

/// 缓存分桶策略响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheScopeResponse {
    /// `"global"` / `"per_credential"`
    pub scope: String,
}

/// 设置缓存分桶策略请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCacheScopeRequest {
    pub scope: String,
}

/// 缓存查找跳过率响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheSkipRateResponse {
    /// 当前跳过率（0.0-1.0），未设置时为 null
    pub rate: Option<f32>,
}

/// 设置缓存查找跳过率请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCacheSkipRateRequest {
    /// 目标跳过率（0.0-1.0）；传 null 表示关闭
    pub rate: Option<f32>,
}

// ============ 代理分组管理 ============

/// 单个代理分组（带 name，列表展示用）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyGroupItem {
    /// 分组名称（key）
    pub name: String,
    /// 代理 URL（支持 socks5/http/https，"direct" 表示显式不走代理）
    pub proxy_url: String,
    /// 代理认证用户名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,
    /// 代理认证密码（注意：明文返回，前端需要避免直接显示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,
    /// 分组说明
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ProxyGroupItem {
    pub fn from_config(name: String, group: ProxyGroupConfig) -> Self {
        Self {
            name,
            proxy_url: group.proxy_url,
            proxy_username: group.proxy_username,
            proxy_password: group.proxy_password,
            description: group.description,
        }
    }
}

/// 代理分组列表响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyGroupsResponse {
    pub groups: Vec<ProxyGroupItem>,
}

impl ProxyGroupsResponse {
    pub fn from_map(map: BTreeMap<String, ProxyGroupConfig>) -> Self {
        let groups = map
            .into_iter()
            .map(|(name, group)| ProxyGroupItem::from_config(name, group))
            .collect();
        Self { groups }
    }
}

/// 新增/更新代理分组请求体（PUT 路径中携带 name）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertProxyGroupRequest {
    pub proxy_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl UpsertProxyGroupRequest {
    pub fn into_config(self) -> ProxyGroupConfig {
        ProxyGroupConfig {
            proxy_url: self.proxy_url,
            proxy_username: self.proxy_username,
            proxy_password: self.proxy_password,
            description: self.description,
        }
    }
}

/// 设置凭据所属代理分组请求体
///
/// 传 `null` 或空字符串表示清空分组绑定
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCredentialGroupRequest {
    pub group: Option<String>,
}

/// 批量设置凭据所属代理分组请求体
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetCredentialGroupRequest {
    pub credential_ids: Vec<u64>,
    /// `null` 或空字符串表示清空分组绑定
    #[serde(default)]
    pub group: Option<String>,
}

/// 批量设置凭据所属代理分组响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetCredentialGroupResponse {
    pub total: usize,
    pub succeeded: Vec<u64>,
    pub failed: Vec<BatchSetCredentialGroupFailure>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetCredentialGroupFailure {
    pub id: u64,
    pub error: String,
}

/// 批量设置凭据优先级请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetPriorityRequest {
    pub credential_ids: Vec<u64>,
    pub priority: u32,
}

/// 批量设置凭据优先级响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetPriorityResponse {
    pub total: usize,
    pub succeeded: Vec<u64>,
    pub failed: Vec<BatchSetCredentialGroupFailure>,
}

/// 批量设置凭据启用/禁用请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetDisabledRequest {
    pub credential_ids: Vec<u64>,
    pub disabled: bool,
}

/// 批量设置凭据启用/禁用响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetDisabledResponse {
    pub total: usize,
    pub succeeded: Vec<u64>,
    pub failed: Vec<BatchSetCredentialGroupFailure>,
}

/// 批量设置凭据 RPM 上限请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetRpmLimitRequest {
    pub credential_ids: Vec<u64>,
    /// 同 `SetRpmLimitRequest.rpm_limit`：null=清除覆盖；0=显式不限流；正整数=上限
    pub rpm_limit: Option<u32>,
}

/// 批量设置凭据 RPM 上限响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetRpmLimitResponse {
    pub total: usize,
    pub succeeded: Vec<u64>,
    pub failed: Vec<BatchSetCredentialGroupFailure>,
}

/// 全局默认 RPM 上限响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultRpmLimitResponse {
    /// 当前全局默认值（None=未配置，等同于不限流）
    pub rpm_limit: Option<u32>,
}

/// 设置全局默认 RPM 上限请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDefaultRpmLimitRequest {
    /// null=清除；0=显式不限流；正整数=每分钟 n 次
    pub rpm_limit: Option<u32>,
}

/// 修改凭据级并发上限请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConcurrencyLimitRequest {
    /// 新并发上限值
    /// - None：清除凭据级覆盖，回退到全局默认
    /// - Some(0)：显式不限并发
    /// - Some(n>0)：最多 n 个同时在途请求
    pub concurrency_limit: Option<u32>,
}

/// 批量设置凭据并发上限请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetConcurrencyLimitRequest {
    pub credential_ids: Vec<u64>,
    /// 同 `SetConcurrencyLimitRequest.concurrency_limit`：null=清除覆盖；0=显式不限并发；正整数=上限
    pub concurrency_limit: Option<u32>,
}

/// 批量设置凭据并发上限响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetConcurrencyLimitResponse {
    pub total: usize,
    pub succeeded: Vec<u64>,
    pub failed: Vec<BatchSetCredentialGroupFailure>,
}

/// 全局默认并发上限响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultConcurrencyLimitResponse {
    /// 当前全局默认值（None=未配置，等同于不限并发）
    pub concurrency_limit: Option<u32>,
}

/// 设置全局默认并发上限请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDefaultConcurrencyLimitRequest {
    /// null=清除；0=显式不限并发；正整数=每个凭据最多 n 个同时在途
    pub concurrency_limit: Option<u32>,
}

// ============ 通用响应 ============

/// 操作成功响应
#[derive(Debug, Serialize)]
pub struct SuccessResponse {
    pub success: bool,
    pub message: String,
}

impl SuccessResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }
}

/// 错误响应
#[derive(Debug, Serialize)]
pub struct AdminErrorResponse {
    pub error: AdminError,
}

#[derive(Debug, Serialize)]
pub struct AdminError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

impl AdminErrorResponse {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: AdminError {
                error_type: error_type.into(),
                message: message.into(),
            },
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }

    pub fn authentication_error() -> Self {
        Self::new("authentication_error", "Invalid or missing admin API key")
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new("forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn api_error(message: impl Into<String>) -> Self {
        Self::new("api_error", message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }
}
