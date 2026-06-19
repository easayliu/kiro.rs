//! Admin API 业务逻辑服务

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::anthropic::CacheTracker;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse,
    BatchSetCredentialGroupFailure, BatchSetCredentialGroupRequest,
    BatchSetCredentialGroupResponse, BatchSetDisabledRequest, BatchSetDisabledResponse,
    BatchSetPriorityRequest, BatchSetPriorityResponse, BatchSetRpmLimitRequest,
    BatchSetOverageRequest, BatchSetOverageResponse, BatchSetRpmLimitResponse,
    BatchSetConcurrencyLimitRequest, BatchSetConcurrencyLimitResponse,
    CacheSkipRateResponse, CredentialStatusItem,
    CredentialsStatusResponse, DefaultConcurrencyLimitResponse, DefaultRpmLimitResponse,
    GlobalCacheResponse, ModelsResponse,
    LoadBalancingModeResponse, ProxyGroupsResponse, SetCacheSkipRateRequest,
    SetCredentialGroupRequest, SetDefaultConcurrencyLimitRequest, SetDefaultRpmLimitRequest,
    SetGlobalCacheRequest,
    SetLoadBalancingModeRequest, UpsertProxyGroupRequest,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 打开 kiro.db 连接并建余额缓存表（data 为上游余额响应 JSON）。
fn open_balance_db(path: &std::path::Path) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS balance_cache (
            credential_id INTEGER PRIMARY KEY,
            cached_at     INTEGER NOT NULL,
            data          TEXT    NOT NULL
        ) STRICT;",
    )?;
    Ok(conn)
}

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    cache_tracker: Arc<CacheTracker>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    /// 余额缓存持久化连接（kiro.db）。None 时仅进程内缓存。
    db: Option<Mutex<rusqlite::Connection>>,
}

impl AdminService {
    pub fn new(token_manager: Arc<MultiTokenManager>, cache_tracker: Arc<CacheTracker>) -> Self {
        let dir = token_manager.cache_dir();
        let db = dir.as_ref().and_then(|d| match open_balance_db(&d.join("kiro.db")) {
            Ok(c) => Some(Mutex::new(c)),
            Err(e) => {
                tracing::error!("余额缓存库打开失败，仅进程内缓存: {}", e);
                None
            }
        });

        let balance_cache = Self::load_balance_cache(&db, dir.as_deref());

        Self {
            token_manager,
            cache_tracker,
            balance_cache: Mutex::new(balance_cache),
            db,
        }
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();

        // 内联缓存余额：直接读 balance_cache 快照（不打上游），前端免逐个查询。
        let balances: HashMap<u64, (BalanceResponse, i64)> = {
            let cache = self.balance_cache.lock();
            cache
                .iter()
                .map(|(id, c)| (*id, (c.data.clone(), c.cached_at as i64)))
                .collect()
        };

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| {
                let cached_balance = balances.get(&entry.id);
                CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                is_current: entry.id == snapshot.current_id,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                email: entry.email,
                success_count: entry.success_count,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                group: entry.group,
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                throttled_until: entry.throttled_until,
                rpm_limit: entry.rpm_limit,
                rpm_current: entry.rpm_current,
                concurrency_limit: entry.concurrency_limit,
                concurrency_current: entry.concurrency_current,
                overage: entry.overage,
                balance: cached_balance.map(|(b, _)| b.clone()),
                balance_cached_at: cached_balance.map(|(_, t)| *t),
                }
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
            default_rpm_limit: snapshot.default_rpm_limit,
            default_concurrency_limit: snapshot.default_concurrency_limit,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 设置凭据级 RPM 上限
    pub fn set_rpm_limit(
        &self,
        id: u64,
        rpm_limit: Option<u32>,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_rpm_limit(id, rpm_limit)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 设置凭据级并发上限
    pub fn set_concurrency_limit(
        &self,
        id: u64,
        concurrency_limit: Option<u32>,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_concurrency_limit(id, concurrency_limit)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 切换凭据的 overage（超额计费）开关
    pub async fn set_overage(&self, id: u64, enabled: bool) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_overage_for(id, enabled)
            .await
            .map_err(|e| self.classify_error(e, id))
    }

    /// 批量切换 overage 开关。
    ///
    /// 每个凭据各是一次上游网络调用（可能附带 token 刷新），故**顺序排队**逐个处理，
    /// 不并发——避免同时打爆上游 setUserPreference、以及多凭据并发刷新争用刷新锁。
    /// 单个失败不影响后续，结果按 succeeded / failed 分别汇总返回。
    pub async fn batch_set_overage(
        &self,
        req: BatchSetOverageRequest,
    ) -> Result<BatchSetOverageResponse, AdminServiceError> {
        if req.credential_ids.is_empty() {
            return Err(AdminServiceError::InvalidParameter(
                "credentialIds 不能为空".to_string(),
            ));
        }
        let total = req.credential_ids.len();
        let mut succeeded = Vec::new();
        let mut failed = Vec::new();
        for id in req.credential_ids {
            match self.set_overage(id, req.enabled).await {
                Ok(_) => succeeded.push(id),
                Err(e) => failed.push(BatchSetCredentialGroupFailure {
                    id,
                    error: e.to_string(),
                }),
            }
        }
        Ok(BatchSetOverageResponse {
            total,
            succeeded,
            failed,
        })
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let remaining = (usage_limit - current_usage).max(0.0);
        let usage_percentage = if usage_limit > 0.0 {
            (current_usage / usage_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
            overage_status: usage.overage_status().map(|s| s.to_string()),
            current_overages: usage.current_overages(),
            overage_charges: usage.overage_charges(),
            overage_rate: usage.overage_rate(),
            overage_cap: usage.overage_cap(),
            currency: usage.currency().map(|s| s.to_string()),
        })
    }

    /// 查询指定凭据上游可用的模型列表
    ///
    /// 直接透传上游 `ListAvailableModels` 的 modelId（不缓存，admin 低频操作；
    /// 前端用 react-query staleTime 做客户端缓存即可）。
    pub async fn get_models(&self, id: u64) -> Result<ModelsResponse, AdminServiceError> {
        let models = self
            .token_manager
            .get_available_models_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;
        Ok(ModelsResponse { id, models })
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            group: req.group,
            client_mode: req.client_mode,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            rpm_limit: None,
            concurrency_limit: None,
            overage: None,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 获取全局缓存模式
    pub fn get_global_cache(&self) -> GlobalCacheResponse {
        GlobalCacheResponse {
            enabled: self.cache_tracker.is_global_cache(),
        }
    }

    /// 设置全局缓存模式
    pub fn set_global_cache(
        &self,
        req: SetGlobalCacheRequest,
    ) -> Result<GlobalCacheResponse, AdminServiceError> {
        self.cache_tracker.set_global_cache(req.enabled);

        // 持久化到 config.json
        if let Some(config_path) = self.token_manager.config().config_path() {
            match crate::model::config::Config::load(config_path) {
                Ok(mut config) => {
                    config.global_cache = req.enabled;
                    if let Err(e) = config.save() {
                        tracing::warn!("保存全局缓存配置失败: {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("加载配置文件失败: {}", e);
                }
            }
        }

        Ok(GlobalCacheResponse {
            enabled: req.enabled,
        })
    }

    /// 获取缓存分桶策略
    pub fn get_cache_scope(&self) -> crate::admin::types::CacheScopeResponse {
        use crate::anthropic::CacheScope;
        let scope = match self.cache_tracker.cache_scope() {
            CacheScope::Global => "global",
            CacheScope::PerCredential => "per_credential",
        };
        crate::admin::types::CacheScopeResponse {
            scope: scope.to_string(),
        }
    }

    /// 设置缓存分桶策略（同时持久化到 config.json）
    pub fn set_cache_scope(
        &self,
        req: crate::admin::types::SetCacheScopeRequest,
    ) -> Result<crate::admin::types::CacheScopeResponse, AdminServiceError> {
        use crate::anthropic::CacheScope;
        let scope = CacheScope::parse(&req.scope);
        self.cache_tracker.set_cache_scope(scope);

        if let Some(config_path) = self.token_manager.config().config_path() {
            match crate::model::config::Config::load(config_path) {
                Ok(mut config) => {
                    let canonical = match scope {
                        CacheScope::Global => "global",
                        CacheScope::PerCredential => "per_credential",
                    };
                    config.cache_scope = Some(canonical.to_string());
                    config.global_cache = matches!(scope, CacheScope::Global);
                    if let Err(e) = config.save() {
                        tracing::warn!("保存缓存分桶策略失败: {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("加载配置文件失败: {}", e);
                }
            }
        }

        Ok(self.get_cache_scope())
    }

    /// 获取缓存查找跳过率
    pub fn get_cache_skip_rate(&self) -> CacheSkipRateResponse {
        CacheSkipRateResponse {
            rate: self.cache_tracker.cache_skip_rate(),
        }
    }

    /// 设置缓存查找跳过率
    pub fn set_cache_skip_rate(
        &self,
        req: SetCacheSkipRateRequest,
    ) -> Result<CacheSkipRateResponse, AdminServiceError> {
        if let Some(r) = req.rate {
            if !r.is_finite() || !(0.0..=1.0).contains(&r) {
                return Err(AdminServiceError::InvalidParameter(format!(
                    "cache skip rate 必须在 0.0-1.0 之间，收到: {}",
                    r
                )));
            }
        }

        self.cache_tracker.set_cache_skip_rate(req.rate);

        if let Some(config_path) = self.token_manager.config().config_path() {
            match crate::model::config::Config::load(config_path) {
                Ok(mut config) => {
                    config.cache_skip_rate = req.rate;
                    if let Err(e) = config.save() {
                        tracing::warn!("保存缓存跳过率失败: {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("加载配置文件失败: {}", e);
                }
            }
        }

        Ok(CacheSkipRateResponse { rate: req.rate })
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    // ============ 代理分组管理 ============

    /// 列出所有代理分组
    pub fn list_proxy_groups(&self) -> ProxyGroupsResponse {
        ProxyGroupsResponse::from_map(self.token_manager.list_proxy_groups())
    }

    /// 新增/更新代理分组
    pub fn upsert_proxy_group(
        &self,
        name: String,
        req: UpsertProxyGroupRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .upsert_proxy_group(name, req.into_config())
            .map_err(|e| self.classify_proxy_group_error(e))
    }

    /// 删除代理分组
    pub fn delete_proxy_group(&self, name: &str) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_proxy_group(name)
            .map_err(|e| self.classify_proxy_group_error(e))
    }

    /// 设置凭据所属代理分组
    pub fn set_credential_group(
        &self,
        id: u64,
        req: SetCredentialGroupRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_group(id, req.group)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 批量设置凭据优先级
    pub fn batch_set_priority(
        &self,
        req: BatchSetPriorityRequest,
    ) -> Result<BatchSetPriorityResponse, AdminServiceError> {
        if req.credential_ids.is_empty() {
            return Err(AdminServiceError::InvalidParameter(
                "credentialIds 不能为空".to_string(),
            ));
        }
        let total = req.credential_ids.len();
        let result = self
            .token_manager
            .set_priority_batch(&req.credential_ids, req.priority);
        Ok(BatchSetPriorityResponse {
            total,
            succeeded: result.succeeded,
            failed: result
                .failed
                .into_iter()
                .map(|f| BatchSetCredentialGroupFailure {
                    id: f.id,
                    error: f.error,
                })
                .collect(),
        })
    }

    /// 批量启用/禁用凭据
    pub fn batch_set_disabled(
        &self,
        req: BatchSetDisabledRequest,
    ) -> Result<BatchSetDisabledResponse, AdminServiceError> {
        if req.credential_ids.is_empty() {
            return Err(AdminServiceError::InvalidParameter(
                "credentialIds 不能为空".to_string(),
            ));
        }
        let total = req.credential_ids.len();
        let result = self
            .token_manager
            .set_disabled_batch(&req.credential_ids, req.disabled);
        Ok(BatchSetDisabledResponse {
            total,
            succeeded: result.succeeded,
            failed: result
                .failed
                .into_iter()
                .map(|f| BatchSetCredentialGroupFailure {
                    id: f.id,
                    error: f.error,
                })
                .collect(),
        })
    }

    /// 批量设置凭据级 RPM 上限
    pub fn batch_set_rpm_limit(
        &self,
        req: BatchSetRpmLimitRequest,
    ) -> Result<BatchSetRpmLimitResponse, AdminServiceError> {
        if req.credential_ids.is_empty() {
            return Err(AdminServiceError::InvalidParameter(
                "credentialIds 不能为空".to_string(),
            ));
        }
        let total = req.credential_ids.len();
        let result = self
            .token_manager
            .set_rpm_limit_batch(&req.credential_ids, req.rpm_limit);
        Ok(BatchSetRpmLimitResponse {
            total,
            succeeded: result.succeeded,
            failed: result
                .failed
                .into_iter()
                .map(|f| BatchSetCredentialGroupFailure {
                    id: f.id,
                    error: f.error,
                })
                .collect(),
        })
    }

    /// 获取全局默认 RPM 上限
    pub fn get_default_rpm_limit(&self) -> DefaultRpmLimitResponse {
        DefaultRpmLimitResponse {
            rpm_limit: self.token_manager.get_default_rpm_limit(),
        }
    }

    /// 设置全局默认 RPM 上限
    pub fn set_default_rpm_limit(
        &self,
        req: SetDefaultRpmLimitRequest,
    ) -> Result<DefaultRpmLimitResponse, AdminServiceError> {
        self.token_manager
            .set_default_rpm_limit(req.rpm_limit)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        Ok(DefaultRpmLimitResponse {
            rpm_limit: req.rpm_limit,
        })
    }

    /// 批量设置凭据级并发上限
    pub fn batch_set_concurrency_limit(
        &self,
        req: BatchSetConcurrencyLimitRequest,
    ) -> Result<BatchSetConcurrencyLimitResponse, AdminServiceError> {
        if req.credential_ids.is_empty() {
            return Err(AdminServiceError::InvalidParameter(
                "credentialIds 不能为空".to_string(),
            ));
        }
        let total = req.credential_ids.len();
        let result = self
            .token_manager
            .set_concurrency_limit_batch(&req.credential_ids, req.concurrency_limit);
        Ok(BatchSetConcurrencyLimitResponse {
            total,
            succeeded: result.succeeded,
            failed: result
                .failed
                .into_iter()
                .map(|f| BatchSetCredentialGroupFailure {
                    id: f.id,
                    error: f.error,
                })
                .collect(),
        })
    }

    /// 获取全局默认并发上限
    pub fn get_default_concurrency_limit(&self) -> DefaultConcurrencyLimitResponse {
        DefaultConcurrencyLimitResponse {
            concurrency_limit: self.token_manager.get_default_concurrency_limit(),
        }
    }

    /// 设置全局默认并发上限
    pub fn set_default_concurrency_limit(
        &self,
        req: SetDefaultConcurrencyLimitRequest,
    ) -> Result<DefaultConcurrencyLimitResponse, AdminServiceError> {
        self.token_manager
            .set_default_concurrency_limit(req.concurrency_limit)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        Ok(DefaultConcurrencyLimitResponse {
            concurrency_limit: req.concurrency_limit,
        })
    }

    /// 批量设置凭据所属代理分组
    pub fn batch_set_credential_group(
        &self,
        req: BatchSetCredentialGroupRequest,
    ) -> Result<BatchSetCredentialGroupResponse, AdminServiceError> {
        if req.credential_ids.is_empty() {
            return Err(AdminServiceError::InvalidParameter(
                "credentialIds 不能为空".to_string(),
            ));
        }
        let total = req.credential_ids.len();
        let result = self
            .token_manager
            .set_group_batch(&req.credential_ids, req.group);
        Ok(BatchSetCredentialGroupResponse {
            total,
            succeeded: result.succeeded,
            failed: result
                .failed
                .into_iter()
                .map(|f| BatchSetCredentialGroupFailure {
                    id: f.id,
                    error: f.error,
                })
                .collect(),
        })
    }

    fn classify_proxy_group_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::InvalidParameter(msg)
        } else if msg.contains("不能为空") {
            AdminServiceError::InvalidParameter(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    // ============ 余额缓存持久化 ============

    /// 从 kiro.db 载入余额缓存（丢弃过期项）。表空时一次性从旧 kiro_balance_cache.json 迁移。
    fn load_balance_cache(
        db: &Option<Mutex<rusqlite::Connection>>,
        dir: Option<&std::path::Path>,
    ) -> HashMap<u64, CachedBalance> {
        let db = match db {
            Some(d) => d,
            None => return HashMap::new(),
        };
        let conn = db.lock();

        // 一次性迁移：表空 + 旧 JSON 存在 → 导入。
        let empty = conn
            .query_row("SELECT COUNT(*) FROM balance_cache", [], |r| r.get::<_, i64>(0))
            .map(|n| n == 0)
            .unwrap_or(true);
        if empty {
            if let Some(content) =
                dir.and_then(|d| std::fs::read_to_string(d.join("kiro_balance_cache.json")).ok())
            {
                if let Ok(map) = serde_json::from_str::<HashMap<String, CachedBalance>>(&content) {
                    for (k, v) in &map {
                        if let (Ok(id), Ok(data)) = (k.parse::<u64>(), serde_json::to_string(&v.data))
                        {
                            let _ = conn.execute(
                                "INSERT OR REPLACE INTO balance_cache (credential_id, cached_at, data) VALUES (?1,?2,?3)",
                                rusqlite::params![id as i64, v.cached_at as i64, data],
                            );
                        }
                    }
                    if !map.is_empty() {
                        tracing::info!("已从 kiro_balance_cache.json 迁移 {} 条余额缓存入库", map.len());
                    }
                }
            }
        }

        // 全部载入（**不**按 TTL 丢弃）：持久化的余额重启后仍要能内联显示「最后已知值」。
        // TTL 仅用于 get_balance 决定是否去上游重拉——过期项留在内存供显示，下次显式查询时刷新。
        let mut out = HashMap::new();
        if let Ok(mut stmt) = conn.prepare("SELECT credential_id, cached_at, data FROM balance_cache")
        {
            let rows = stmt.query_map([], |row| {
                let id: i64 = row.get(0)?;
                let cached_at: i64 = row.get(1)?;
                let data: String = row.get(2)?;
                Ok((id as u64, cached_at, data))
            });
            if let Ok(rows) = rows {
                for r in rows.flatten() {
                    let (id, cached_at, data) = r;
                    if let Ok(parsed) = serde_json::from_str::<BalanceResponse>(&data) {
                        out.insert(id, CachedBalance { cached_at: cached_at as f64, data: parsed });
                    }
                }
            }
        }
        out
    }

    /// 把当前余额缓存全量落库（事务内 DELETE + INSERT，数据量为凭据数级别）。
    fn save_balance_cache(&self) {
        let db = match &self.db {
            Some(d) => d,
            None => return,
        };

        let snapshot: Vec<(u64, i64, String)> = {
            let cache = self.balance_cache.lock();
            cache
                .iter()
                .filter_map(|(id, v)| {
                    serde_json::to_string(&v.data)
                        .ok()
                        .map(|data| (*id, v.cached_at as i64, data))
                })
                .collect()
        };

        let mut conn = db.lock();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("余额缓存写事务失败: {}", e);
                return;
            }
        };
        if tx.execute("DELETE FROM balance_cache", []).is_ok() {
            for (id, cached_at, data) in &snapshot {
                let _ = tx.execute(
                    "INSERT INTO balance_cache (credential_id, cached_at, data) VALUES (?1,?2,?3)",
                    rusqlite::params![*id as i64, cached_at, data],
                );
            }
        }
        if let Err(e) = tx.commit() {
            tracing::warn!("保存余额缓存失败: {}", e);
        }
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 3. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据") {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }
}
