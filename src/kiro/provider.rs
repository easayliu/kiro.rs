//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试

use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use uuid::Uuid;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::errors::UpstreamHttpError;
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::{ClientMode, ProxyGroupConfig, TlsBackend};
use std::collections::BTreeMap;
use parking_lot::Mutex;

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 总重试次数硬上限（避免无限重试）
const MAX_TOTAL_RETRIES: usize = 9;

/// 成功调用 Kiro API 的结果
///
/// 除 HTTP 响应外，还携带实际使用的 credential_id，
/// 供 prompt caching 本地追踪器按 credential 维度分片存储 checkpoint。
pub struct ApiCallResult {
    pub response: reqwest::Response,
    pub credential_id: u64,
}

/// 已缓冲完整响应体的调用结果（非流式专用）
///
/// 与 [`ApiCallResult`] 的区别：body 已在重试范围内读完。"头 200 但 body 中途
/// 被上游 RST/EOF" 属于上游瞬态，[`crate::kiro::provider::KiroProvider::call_api`]
/// 收到响应头即返回、覆盖不到 body 读取；这里把 body 读进重试范围。
pub struct BufferedApiCallResult {
    pub body: bytes::Bytes,
    pub credential_id: u64,
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
}

impl KiroProvider {
    /// 获取客户端模式的 origin 值
    pub fn origin(&self) -> &'static str {
        self.token_manager.config().client_mode.origin()
    }

    /// 是否为 kiro-cli 模式
    pub fn is_cli_mode(&self) -> bool {
        self.token_manager.config().client_mode.is_cli()
    }

    /// 创建带代理配置的 KiroProvider 实例
    pub fn with_proxy(token_manager: Arc<MultiTokenManager>, proxy: Option<ProxyConfig>) -> Self {
        let tls_backend = token_manager.config().tls_backend;
        // 预热：构建全局代理对应的 Client
        let initial_client = build_client(proxy.as_ref(), 720, tls_backend)
            .expect("创建 HTTP 客户端失败");
        let mut cache = HashMap::new();
        cache.insert(proxy.clone(), initial_client);

        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(cache),
            tls_backend,
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let groups = self.token_manager.proxy_groups_snapshot();
        let effective = credentials.effective_proxy(self.global_proxy.as_ref(), &groups);
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client.clone());
        }

        // cache miss：首次为该代理组合构建 client，记一条 INFO 日志
        let source = resolve_proxy_source(credentials, &groups, self.global_proxy.is_some());
        match &effective {
            Some(p) => tracing::info!(
                "代理路由建立: credential #{} ({}) → {} [来源: {}]",
                credentials.id.unwrap_or(0),
                credentials.email.as_deref().unwrap_or("-"),
                p.url,
                source,
            ),
            None => tracing::info!(
                "代理路由建立: credential #{} ({}) → 直连 [来源: {}]",
                credentials.id.unwrap_or(0),
                credentials.email.as_deref().unwrap_or("-"),
                source,
            ),
        }

        let client = build_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 获取凭据级 API 基础 URL
    fn base_url_for(&self, credentials: &KiroCredentials) -> String {
        format!(
            "https://q.{}.amazonaws.com/generateAssistantResponse",
            credentials.effective_api_region(self.token_manager.config())
        )
    }

    /// 获取凭据级 MCP API URL
    fn mcp_url_for(&self, credentials: &KiroCredentials) -> String {
        format!(
            "https://q.{}.amazonaws.com/mcp",
            credentials.effective_api_region(self.token_manager.config())
        )
    }

    /// 获取凭据级 API 基础域名
    fn base_domain_for(&self, credentials: &KiroCredentials) -> String {
        format!(
            "q.{}.amazonaws.com",
            credentials.effective_api_region(self.token_manager.config())
        )
    }

    /// 从请求体中提取模型信息
    ///
    /// 尝试解析 JSON 请求体，提取 conversationState.currentMessage.userInputMessage.modelId
    fn extract_model_from_request(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        // 尝试提取 conversationState.currentMessage.userInputMessage.modelId
        json.get("conversationState")?
            .get("currentMessage")?
            .get("userInputMessage")?
            .get("modelId")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// 按凭据重写请求体：注入 `profileArn`、覆盖 `userInputMessage.origin`、
    /// 按模式增/删 `userInputMessageContext.envState`。一次 JSON 解析完成所有改写。
    ///
    /// `origin` 与 `envState` 的最终来源是凭据级 `effective_client_mode`，
    /// 用以修正 handlers 阶段以全局 `client_mode` 拼出的 body 与凭据级 headers 不一致的问题。
    fn apply_credential_overrides(
        request_body: &str,
        profile_arn: &Option<String>,
        mode: ClientMode,
    ) -> String {
        let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) else {
            return request_body.to_string();
        };

        // 有则写入，无则剥离已存在的 profileArn（effective_profile_arn 对企业 IdC / api_key
        // 返回 None，需保证 body 不残留 handlers 阶段拼入的旧 ARN，避免身份不匹配触发 403）。
        match profile_arn {
            Some(arn) => json["profileArn"] = serde_json::Value::String(arn.clone()),
            None => {
                if let Some(obj) = json.as_object_mut() {
                    obj.remove("profileArn");
                }
            }
        }

        if let Some(user_input) =
            json.pointer_mut("/conversationState/currentMessage/userInputMessage")
        {
            user_input["origin"] = serde_json::Value::String(mode.origin().to_string());

            if let Some(ctx) = user_input
                .get_mut("userInputMessageContext")
                .and_then(|v| v.as_object_mut())
            {
                if mode.is_cli() {
                    ctx.insert(
                        "envState".to_string(),
                        serde_json::json!({
                            "operatingSystem": "linux",
                            "currentWorkingDirectory": "/home/user",
                        }),
                    );
                } else {
                    ctx.remove("envState");
                }
            }
        }

        serde_json::to_string(&json).unwrap_or_else(|_| request_body.to_string())
    }

    /// 发送非流式 API 请求并缓冲完整响应体，body 读取失败时重新发起整轮调用（含换凭据）
    ///
    /// 状态级故障转移（400/401/403/402/429/5xx/网络瞬态）由内层 `call_api_with_retry`
    /// 处理；本方法在其外补一层 body 读取重试。
    ///
    /// 背景：`call_api_with_retry` 收到响应头（200）即返回，body 在 handler 里才读，
    /// 因此"头 200 但 body 中途被上游 HTTP/2 RST_STREAM(INTERNAL_ERROR) / EOF"这类
    /// **上游瞬态**错误落在重试范围之外，过去只能记日志返回 502、无重试无故障转移。
    /// 这里在外层补一层 body 读取重试：失败则重新发起整轮调用（`call_api_with_retry`
    /// 会经 LRU 重新选凭据，自然故障转移），对齐 5xx 瞬态语义。
    pub async fn call_api_buffered(
        &self,
        request_body: &str,
        preferred_credential: Option<u64>,
    ) -> anyhow::Result<BufferedApiCallResult> {
        const BODY_READ_MAX_ATTEMPTS: usize = 3;
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 0..BODY_READ_MAX_ATTEMPTS {
            // 仅首轮使用粘性绑定凭据；body 读失败后续轮次走默认选择以实现故障转移
            let preferred = if attempt == 0 { preferred_credential } else { None };

            // 状态级错误（连接/4xx/5xx）已由内层重试循环处理；这里拿到的是 200 响应头
            let result = self
                .call_api_with_retry(request_body, false, preferred)
                .await?;
            let credential_id = result.credential_id;

            match result.response.bytes().await {
                Ok(body) => {
                    return Ok(BufferedApiCallResult {
                        body,
                        credential_id,
                    });
                }
                Err(e) => {
                    // 拼接完整错误源链，区分 timeout / connect reset / 上游提前 EOF / h2 RST
                    let mut chain = e.to_string();
                    let mut src = std::error::Error::source(&e);
                    while let Some(s) = src {
                        chain.push_str(" -> ");
                        chain.push_str(&s.to_string());
                        src = s.source();
                    }
                    tracing::warn!(
                        cred_id = credential_id,
                        is_timeout = e.is_timeout(),
                        is_connect = e.is_connect(),
                        is_body = e.is_body(),
                        is_decode = e.is_decode(),
                        "读取响应体失败（上游瞬态，body 重试 {}/{}）: {}",
                        attempt + 1,
                        BODY_READ_MAX_ATTEMPTS,
                        chain
                    );
                    last_error = Some(anyhow::Error::new(e));
                    if attempt + 1 < BODY_READ_MAX_ATTEMPTS {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "非流式 API 读取响应体失败：已达到最大重试次数（{}次）",
                BODY_READ_MAX_ATTEMPTS
            )
        }))
    }

    /// 列出当前可用的凭据 id（供粘性绑定表作为候选池）
    pub fn available_credential_ids(&self, model: Option<&str>) -> Vec<u64> {
        self.token_manager.available_credential_ids(model)
    }

    /// 发送流式 API 请求
    ///
    /// 支持多凭据故障转移：
    /// - 400 Bad Request: 直接返回错误，不计入凭据失败
    ///   （例外：INVALID_MODEL_ID 仅 LRU 轮转到下一凭据，不计失败计数）
    /// - 401/403: 视为凭据/权限问题，计入失败次数并允许故障转移
    /// - 402 MONTHLY_REQUEST_COUNT / OVERAGE_REQUEST_LIMIT_EXCEEDED: 视为额度用尽，禁用凭据并切换
    /// - 429/5xx/网络等瞬态错误: 重试但不禁用或切换凭据（避免误把所有凭据锁死）
    ///
    /// # Arguments
    /// * `request_body` - JSON 格式的请求体字符串
    ///
    /// # Returns
    /// 返回原始的 HTTP Response，调用方负责处理流式数据
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        preferred_credential: Option<u64>,
    ) -> anyhow::Result<ApiCallResult> {
        self.call_api_with_retry(request_body, true, preferred_credential)
            .await
    }

    /// 拉取上游 `ListAvailableModels`，返回 `(modelId, maxInputTokens)` 列表。
    ///
    /// 用于动态校准 contextUsage 百分比反推 token 时用的上下文窗口，取代硬编码常量
    /// （上游 `q.{region}.amazonaws.com` 老端点提供该 API；新 runtime 端点不提供）。
    /// 任意可用凭据即可拉取（与具体模型无关），失败由调用方回退硬编码窗口。
    pub async fn list_available_models(&self) -> anyhow::Result<Vec<(String, i32)>> {
        let ctx = self.token_manager.acquire_context(None, None).await?;
        let config = self.token_manager.config();
        let machine_id =
            machine_id::generate_from_credentials(&ctx.credentials, config).unwrap_or_default();
        let mode = ctx.credentials.effective_client_mode(config);
        let region = ctx.credentials.effective_api_region(config).to_string();
        let url = format!("https://q.{}.amazonaws.com/ListAvailableModels", region);
        let x_amz_user_agent = config.streaming_x_amz_user_agent(&machine_id, mode);
        let user_agent = config.streaming_user_agent(&machine_id, mode);

        // ListAvailableModels 用 GET + query 参数（对齐 Kiro IDE：origin=AI_EDITOR，
        // 带 profileArn 时附上）。
        let mut params: Vec<(&str, String)> = vec![("origin", "AI_EDITOR".to_string())];
        if let Some(arn) = ctx.credentials.effective_profile_arn() {
            params.push(("profileArn", arn));
        }

        let mut request = self.client_for(&ctx.credentials)?.get(&url).query(&params);
        if ctx.credentials.is_api_key_credential() {
            request = request.header("tokentype", "API_KEY");
        }
        let response = request
            .header("x-amz-user-agent", &x_amz_user_agent)
            .header("user-agent", &user_agent)
            .header("host", format!("q.{}.amazonaws.com", region))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("Authorization", format!("Bearer {}", ctx.token))
            .header("Connection", "close")
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            let snippet: String = body.chars().take(200).collect();
            anyhow::bail!("ListAvailableModels HTTP {}: {}", status, snippet);
        }

        let json: serde_json::Value = serde_json::from_str(&body)?;
        let Some(models) = json.get("models").and_then(|m| m.as_array()) else {
            anyhow::bail!("ListAvailableModels 响应缺少 models 数组");
        };
        let mut out = Vec::new();
        for m in models {
            let Some(id) = m.get("modelId").and_then(|v| v.as_str()) else {
                continue;
            };
            let max = m
                .get("tokenLimits")
                .and_then(|t| t.get("maxInputTokens"))
                .and_then(|v| v.as_i64())
                .filter(|&x| x > 0);
            if let Some(max) = max {
                out.push((id.to_string(), max as i32));
            }
        }
        Ok(out)
    }

    /// 发送 MCP API 请求
    ///
    /// 用于 WebSearch 等工具调用
    ///
    /// # Arguments
    /// * `request_body` - JSON 格式的 MCP 请求体字符串
    /// * `preferred_credential` - 粘性绑定的凭据 id（仅首轮尝试）
    ///
    /// # Returns
    /// 返回原始的 HTTP Response 及实际使用的 credential_id
    pub async fn call_mcp(
        &self,
        request_body: &str,
        preferred_credential: Option<u64>,
    ) -> anyhow::Result<ApiCallResult> {
        self.call_mcp_with_retry(request_body, preferred_credential)
            .await
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(
        &self,
        request_body: &str,
        preferred_credential: Option<u64>,
    ) -> anyhow::Result<ApiCallResult> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();

        for attempt in 0..max_retries {
            // 仅首轮使用粘性绑定凭据；后续轮次 fallback 走默认选择
            let preferred = if attempt == 0 { preferred_credential } else { None };
            // 获取调用上下文
            // MCP 调用（WebSearch 等工具）不涉及模型选择，无需按模型过滤凭据
            let ctx = match self.token_manager.acquire_context(None, preferred).await {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config)
                .unwrap_or_default();
            let mode = ctx.credentials.effective_client_mode(config);

            let url = self.mcp_url_for(&ctx.credentials);
            let x_amz_user_agent = config.streaming_x_amz_user_agent(&machine_id, mode);
            let user_agent = config.streaming_user_agent(&machine_id, mode);

            // 发送请求
            let mut request = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(request_body.to_string())
                .header("content-type", "application/json");

            // MCP 请求按需携带 profile ARN：effective_profile_arn 自带优先，Builder ID/Social
            // 缺失时按 auth_method 兜底共享 ARN，企业 IdC / api_key 返回 None 则不附带。
            if let Some(arn) = ctx.credentials.effective_profile_arn() {
                request = request.header("x-amzn-kiro-profile-arn", arn);
            }

            if ctx.credentials.is_api_key_credential() {
                request = request.header("tokentype", "API_KEY");
            }

            let response = match request
                .header("x-amz-user-agent", &x_amz_user_agent)
                .header("user-agent", &user_agent)
                .header("host", &self.base_domain_for(&ctx.credentials))
                .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
                .header("amz-sdk-request", "attempt=1; max=3")
                .header("Authorization", format!("Bearer {}", ctx.token))
                .header("Connection", "close")
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                return Ok(ApiCallResult {
                    response,
                    credential_id: ctx.id,
                });
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽
            if status.as_u16() == 402 && Self::is_quota_exhausted(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                // INVALID_MODEL_ID: 当前凭据无此模型权限，仅 LRU 轮转到下一个凭据，
                // 不计失败计数（避免误把健康但缺少该模型的凭据打到禁用阈值）
                if Self::is_invalid_model_id(&body) {
                    tracing::warn!(
                        "MCP 请求 INVALID_MODEL_ID 轮转（cred #{}, 尝试 {}/{}）: {} {}",
                        ctx.id,
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    self.token_manager.mark_accessed(ctx.id);
                    last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                    continue;
                }
                tracing::error!(
                    cred_id = ctx.id,
                    status = %status,
                    response_body = %body,
                    request_body = %request_body,
                    "MCP 400 Bad Request - dump 上游请求/响应"
                );
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if Self::is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self.token_manager.force_refresh_token_for(ctx.id).await.is_ok() {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                // 429 被限流时刷新 last_used_at，让 balanced (LRU) 立即轮转到其他凭据
                if status.as_u16() == 429 {
                    self.token_manager.mark_accessed(ctx.id);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        preferred_credential: Option<u64>,
    ) -> anyhow::Result<ApiCallResult> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 尝试从请求体中提取模型信息
        let model = Self::extract_model_from_request(request_body);

        for attempt in 0..max_retries {
            // 仅首轮使用粘性绑定凭据；后续轮次 fallback 走默认选择
            let preferred = if attempt == 0 { preferred_credential } else { None };
            // [TTFT 埋点] 计 acquire：凭据选择 + 可能的内联 token 刷新（含落盘）。
            // 若这一段大，说明首字慢在“凭据准备/刷新”，而非上游。
            let acquire_start = Instant::now();
            let ctx = match self
                .token_manager
                .acquire_context(model.as_deref(), preferred)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };
            let acquire_ms = acquire_start.elapsed().as_millis();

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config)
                .unwrap_or_default();
            let mode = ctx.credentials.effective_client_mode(config);

            let url = self.base_url_for(&ctx.credentials);
            let x_amz_user_agent = config.streaming_x_amz_user_agent(&machine_id, mode);
            let user_agent = config.streaming_user_agent(&machine_id, mode);

            // 按实际凭据重写 body：profileArn / origin / envState 均按凭据级 mode 决定。
            // profileArn 由 effective_profile_arn 三级解析（自带 → 探测写回 → auth_method 兜底，
            // 企业 IdC / api_key 返回 None），apply_credential_overrides 会顺带剥离残留 ARN。
            let outbound_body = Self::apply_credential_overrides(
                request_body,
                &ctx.credentials.effective_profile_arn(),
                mode,
            );

            // 发送请求；clone 一份留给 400 等请求侧错误时回溯实际发往上游的 payload
            let mut request = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(outbound_body.clone())
                .header("content-type", "application/json")
                .header("x-amzn-kiro-agent-mode", "vibe")
                .header("x-amz-user-agent", &x_amz_user_agent)
                .header("user-agent", &user_agent)
                .header("host", &self.base_domain_for(&ctx.credentials))
                .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
                .header("amz-sdk-request", "attempt=1; max=3")
                .header("Authorization", format!("Bearer {}", ctx.token))
                .header("Connection", "close");

            if ctx.credentials.is_api_key_credential() {
                request = request.header("tokentype", "API_KEY");
            }

            // [TTFT 埋点] 计 send：从发出请求到上游返回响应头（流式下约等于上游开始吐字）。
            // 与 acquire_ms 对比即可定位首字慢在“本地凭据准备”还是“上游响应”。
            let send_start = Instant::now();
            let response = match request
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "API 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                    // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            let send_ms = send_start.elapsed().as_millis();

            // [TTFT 埋点] 补 model / 上游域名 / 代理主机，判断慢 send 是否聚集在
            // 某些代理或 region 上（代理只记 host，不含账号密码）。
            // acquire 大 → 本地刷新；send 大 → 上游/链路；这两段不含流式后续吐字。
            let proxy_host = {
                let groups = self.token_manager.proxy_groups_snapshot();
                ctx.credentials
                    .effective_proxy(self.global_proxy.as_ref(), &groups)
                    .and_then(|p| reqwest::Url::parse(&p.url).ok())
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_else(|| "直连".to_string())
            };
            tracing::debug!(
                "[TTFT] 凭据 #{} {} model={} attempt={}/{} status={} acquire={}ms send={}ms host={} proxy={}",
                ctx.id,
                api_type,
                model.as_deref().unwrap_or("-"),
                attempt + 1,
                max_retries,
                status.as_u16(),
                acquire_ms,
                send_ms,
                self.base_domain_for(&ctx.credentials),
                proxy_host,
            );

            // 成功响应
            if status.is_success() {
                // 截断诊断：打一次上游响应的 framing 头。
                // - Transfer-Encoding: chunked → hyper 能靠"缺结尾 0 长度块"检测出截断并报错，
                //   不会发生"静默截断"；
                // - 既无 chunked 也无 Content-Length（close-delimited）→ 连接被提前关闭会被当成
                //   正常 EOF，才是 HTTP/1.1 + Connection: close 下静默截断的土壤。
                let headers = response.headers();
                tracing::debug!(
                    version = ?response.version(),
                    transfer_encoding = ?headers.get(reqwest::header::TRANSFER_ENCODING),
                    content_length = ?headers.get(reqwest::header::CONTENT_LENGTH),
                    content_type = ?headers.get(reqwest::header::CONTENT_TYPE),
                    connection = ?headers.get(reqwest::header::CONNECTION),
                    "上游响应 framing（截断诊断）"
                );
                self.token_manager.report_success(ctx.id);
                return Ok(ApiCallResult {
                    response,
                    credential_id: ctx.id,
                });
            }

            // 失败响应：读取 body 用于日志/错误信息
            let body = response.text().await.unwrap_or_default();

            // 402 Payment Required 且额度用尽：禁用凭据并故障转移
            if status.as_u16() == 402 && Self::is_quota_exhausted(&body) {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    tracing::error!("所有凭据均已用尽额度");
                    anyhow::bail!(UpstreamHttpError {
                        status: status.as_u16(),
                        body,
                        api_type: api_type.to_string(),
                    });
                }

                last_error = Some(anyhow::Error::new(UpstreamHttpError {
                    status: status.as_u16(),
                    body,
                    api_type: api_type.to_string(),
                }));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                // INVALID_MODEL_ID: 当前凭据无此模型权限，仅 LRU 轮转到下一个凭据，
                // 不计失败计数（避免误把健康但缺少该模型的凭据打到禁用阈值）。
                // 仅在 balanced 模式下才能稳定轮转，priority 模式可能反复命中同凭据。
                if Self::is_invalid_model_id(&body) {
                    tracing::warn!(
                        "API 请求 INVALID_MODEL_ID 轮转（cred #{}, 尝试 {}/{}）: {} {}",
                        ctx.id,
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    self.token_manager.mark_accessed(ctx.id);
                    last_error = Some(anyhow::Error::new(UpstreamHttpError {
                        status: status.as_u16(),
                        body,
                        api_type: api_type.to_string(),
                    }));
                    continue;
                }

                // 其他 400：请求侧问题，重试/切换凭据无意义。
                // 仅当响应是请求格式 bug（"Improperly formed request" 或
                // "TOOL_USE_RESULT_MISMATCH"，都是我们这边发出的格式问题）时才 dump
                // 完整 outbound payload 离线复现；其他 400（如用户输入超长）只记简要日志。
                if Self::is_malformed_request(&body) {
                    tracing::error!(
                        cred_id = ctx.id,
                        status = %status,
                        api_type = api_type,
                        response_body = %body,
                        request_body = %outbound_body,
                        "API 400 请求格式错误 - dump 上游请求/响应"
                    );
                } else {
                    tracing::warn!(
                        cred_id = ctx.id,
                        status = %status,
                        api_type = api_type,
                        "API 400 Bad Request: {}",
                        body
                    );
                }
                anyhow::bail!(UpstreamHttpError {
                    status: status.as_u16(),
                    body,
                    api_type: api_type.to_string(),
                });
            }

            // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
                tracing::warn!(
                    "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if Self::is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self.token_manager.force_refresh_token_for(ctx.id).await.is_ok() {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    tracing::error!("所有凭据均已禁用（401/403 累计达上限）");
                    anyhow::bail!(UpstreamHttpError {
                        status: status.as_u16(),
                        body,
                        api_type: api_type.to_string(),
                    });
                }

                last_error = Some(anyhow::Error::new(UpstreamHttpError {
                    status: status.as_u16(),
                    body,
                    api_type: api_type.to_string(),
                }));
                continue;
            }

            // 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
            // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                // 429 被限流时打 throttle 冷却（指数退避），冷却期间该凭据
                // 不参与 balanced 轮转；最高优先级档全部冷却时自然降级到下一档
                if status.as_u16() == 429 {
                    // 上游 "suspicious activity" 限流：账号被临时封禁，
                    // 短间隔重试无意义，直接冷却 1 小时
                    if body.contains("suspicious activity") {
                        self.token_manager.report_throttled_for(ctx.id, 3600);
                    } else {
                        self.token_manager.report_throttled(ctx.id);
                    }
                }
                // 退避决策：429 退避只在"没有其它鲜活凭据、只能等同一账号冷却"时才付出。
                // 刚被限流的凭据已打 throttled_until，若仍有其它未冷却凭据可立即切换，
                // 则跳过 sleep 直接换号重试，避免把退避时间叠加进流式首字（TTFT）关键
                // 路径。408/5xx 多为上游整体瞬态，换号未必有用且退避能避免连环打爆上游，
                // 故仍保留退避。
                let will_retry = attempt + 1 < max_retries;
                let skip_backoff = will_retry
                    && status.as_u16() == 429
                    && self.token_manager.has_fresh_credential(model.as_deref());
                // 决策结果直接打进 WARN，生产可观测：是否换号、是否退避、用哪个凭据。
                // 连续 attempt 的凭据 # 若变化即说明已换号；"跳过退避换号"说明优化生效。
                let decision = if !will_retry {
                    "不再重试"
                } else if skip_backoff {
                    "有鲜活凭据→跳过退避直接换号"
                } else if status.as_u16() == 429 {
                    "无鲜活凭据→退避后重试"
                } else {
                    "退避后重试"
                };
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，凭据 #{}，尝试 {}/{}，{}）: {} {}",
                    ctx.id,
                    attempt + 1,
                    max_retries,
                    decision,
                    status,
                    body
                );
                last_error = Some(anyhow::Error::new(UpstreamHttpError {
                    status: status.as_u16(),
                    body,
                    api_type: api_type.to_string(),
                }));
                if will_retry && !skip_backoff {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                anyhow::bail!(UpstreamHttpError {
                    status: status.as_u16(),
                    body,
                    api_type: api_type.to_string(),
                });
            }

            // 兜底：当作可重试的瞬态错误处理（不切换凭据）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                body
            );
            last_error = Some(anyhow::Error::new(UpstreamHttpError {
                status: status.as_u16(),
                body,
                api_type: api_type.to_string(),
            }));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 判断 402 响应是否属于"额度用尽"场景，需要禁用凭据并故障转移：
    /// - `MONTHLY_REQUEST_COUNT`：月度免费额度用尽
    /// - `OVERAGE_REQUEST_LIMIT_EXCEEDED`：超额配额也用尽
    fn is_quota_exhausted(body: &str) -> bool {
        const REASONS: &[&str] = &["MONTHLY_REQUEST_COUNT", "OVERAGE_REQUEST_LIMIT_EXCEEDED"];

        if REASONS.iter().any(|r| body.contains(r)) {
            return true;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            return false;
        };

        let matches_reason = |v: &serde_json::Value| {
            v.get("reason")
                .and_then(|v| v.as_str())
                .is_some_and(|s| REASONS.contains(&s))
        };

        if matches_reason(&value) {
            return true;
        }

        value
            .pointer("/error")
            .is_some_and(matches_reason)
    }

    /// 判断 400 响应是否属于"凭据无此模型权限"场景：
    /// 不同凭据可能开通的模型集合不同，应允许轮转到其他凭据，但不应计入失败。
    fn is_invalid_model_id(body: &str) -> bool {
        const REASON: &str = "INVALID_MODEL_ID";

        if body.contains(REASON) {
            return true;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            return false;
        };

        let matches_reason = |v: &serde_json::Value| {
            v.get("reason")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == REASON)
        };

        if matches_reason(&value) {
            return true;
        }

        value.pointer("/error").is_some_and(matches_reason)
    }

    /// 检查响应体是否包含 bearer token 失效的特征消息
    ///
    /// 当上游已使 accessToken 失效但本地 expiresAt 未到期时，
    /// API 会返回 401/403 并携带此特征消息。
    fn is_bearer_token_invalid(body: &str) -> bool {
        body.contains("The bearer token included in the request is invalid")
    }

    /// 判断 400 响应是否为"我们发出去的请求格式有问题"：
    /// 形如 `{"message":"Improperly formed request.","reason":null}`，
    /// 这种才需要 dump 出 outbound payload 离线复现；其他 400（如用户输入超长）不 dump。
    fn is_improperly_formed_request(body: &str) -> bool {
        body.contains("Improperly formed request")
    }

    /// 判断 400 响应是否为 tool_use/tool_result 配对不一致：
    /// 形如 `{"message":"...toolResult blocks ... exceeds ... toolUse blocks...","reason":"TOOL_USE_RESULT_MISMATCH"}`。
    /// 同样是"我们发出去的请求格式有问题"，需要 dump outbound payload 离线复现。
    fn is_tool_use_result_mismatch(body: &str) -> bool {
        body.contains("TOOL_USE_RESULT_MISMATCH")
    }

    /// 判断 400 是否属于"请求格式 bug"（应 dump outbound payload 离线复现）。
    fn is_malformed_request(body: &str) -> bool {
        Self::is_improperly_formed_request(body) || Self::is_tool_use_result_mismatch(body)
    }
}

/// 解析当前 credential 的有效代理来自哪个配置层级（用于日志可读性）
fn resolve_proxy_source(
    credentials: &KiroCredentials,
    groups: &BTreeMap<String, ProxyGroupConfig>,
    has_global_proxy: bool,
) -> String {
    if let Some(url) = credentials.proxy_url.as_deref() {
        if url.eq_ignore_ascii_case(KiroCredentials::PROXY_DIRECT) {
            return "凭据自身 (direct)".to_string();
        }
        return "凭据自身".to_string();
    }
    if let Some(group_name) = credentials.group.as_deref() {
        if let Some(group) = groups.get(group_name) {
            if group.proxy_url.eq_ignore_ascii_case(KiroCredentials::PROXY_DIRECT) {
                return format!("分组 {} (direct)", group_name);
            }
            return format!("分组 {}", group_name);
        }
        // group 找不到 —— effective_proxy 已经 warn，这里回退到全局
        return format!("全局 (分组 {} 未定义)", group_name);
    }
    if has_global_proxy {
        "全局".to_string()
    } else {
        "无代理".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_quota_exhausted_monthly_reason() {
        let body = r#"{"message":"You have reached the limit.","reason":"MONTHLY_REQUEST_COUNT"}"#;
        assert!(KiroProvider::is_quota_exhausted(body));
    }

    #[test]
    fn test_is_quota_exhausted_overage_reason() {
        let body = r#"{"message":"You have reached the limit for overages.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#;
        assert!(KiroProvider::is_quota_exhausted(body));
    }

    #[test]
    fn test_is_quota_exhausted_nested_reason() {
        let body = r#"{"error":{"reason":"MONTHLY_REQUEST_COUNT"}}"#;
        assert!(KiroProvider::is_quota_exhausted(body));

        let body = r#"{"error":{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}}"#;
        assert!(KiroProvider::is_quota_exhausted(body));
    }

    #[test]
    fn test_is_quota_exhausted_false() {
        let body = r#"{"message":"nope","reason":"DAILY_REQUEST_COUNT"}"#;
        assert!(!KiroProvider::is_quota_exhausted(body));
    }

    #[test]
    fn test_is_invalid_model_id_detects_reason() {
        let body = r#"{"message":"Invalid model. Please select a different model to continue.","reason":"INVALID_MODEL_ID"}"#;
        assert!(KiroProvider::is_invalid_model_id(body));
    }

    #[test]
    fn test_is_invalid_model_id_nested_reason() {
        let body = r#"{"error":{"reason":"INVALID_MODEL_ID"}}"#;
        assert!(KiroProvider::is_invalid_model_id(body));
    }

    #[test]
    fn test_is_invalid_model_id_false() {
        let body = r#"{"message":"bad","reason":"VALIDATION_ERROR"}"#;
        assert!(!KiroProvider::is_invalid_model_id(body));
    }

    fn body_with_user_input(origin: &str, with_env_state: bool) -> String {
        let env_state = if with_env_state {
            r#","envState":{"operatingSystem":"darwin","currentWorkingDirectory":"/tmp"}"#
        } else {
            ""
        };
        format!(
            r#"{{"conversationState":{{"conversationId":"c1","currentMessage":{{"userInputMessage":{{"content":"hi","modelId":"m","origin":"{origin}","userInputMessageContext":{{"tools":[]{env_state}}}}}}}}}}}"#,
        )
    }

    #[test]
    fn test_apply_credential_overrides_injects_arn_and_ide_origin() {
        let body = body_with_user_input("AI_EDITOR", false);
        let arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC".to_string());
        let result = KiroProvider::apply_credential_overrides(&body, &arn, ClientMode::KiroIde);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        let user_input = &json["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(user_input["origin"], "AI_EDITOR");
        // KiroIde 模式不携带 envState
        assert!(user_input["userInputMessageContext"]
            .get("envState")
            .is_none());
    }

    #[test]
    fn test_apply_credential_overrides_cli_mode_rewrites_origin_and_injects_env_state() {
        // Body 由全局 ide 模式拼出，凭据是 cli：origin/envState 都需重写
        let body = body_with_user_input("AI_EDITOR", false);
        let result = KiroProvider::apply_credential_overrides(&body, &None, ClientMode::KiroCli);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        let user_input = &json["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(user_input["origin"], "KIRO_CLI");
        assert_eq!(
            user_input["userInputMessageContext"]["envState"]["operatingSystem"],
            "linux"
        );
        assert_eq!(
            user_input["userInputMessageContext"]["envState"]["currentWorkingDirectory"],
            "/home/user"
        );
        // profileArn 缺省时不注入
        assert!(json.get("profileArn").is_none());
    }

    #[test]
    fn test_apply_credential_overrides_ide_mode_strips_env_state() {
        // Body 由全局 cli 模式拼出，凭据是 ide：要把已注入的 envState 去掉，origin 改回 AI_EDITOR
        let body = body_with_user_input("KIRO_CLI", true);
        let result = KiroProvider::apply_credential_overrides(&body, &None, ClientMode::KiroIde);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        let user_input = &json["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(user_input["origin"], "AI_EDITOR");
        assert!(user_input["userInputMessageContext"]
            .get("envState")
            .is_none());
    }

    #[test]
    fn test_apply_credential_overrides_overwrites_existing_arn() {
        let body = format!(
            r#"{{"profileArn":"old-arn","conversationState":{{"currentMessage":{{"userInputMessage":{{"content":"x","modelId":"m","origin":"AI_EDITOR","userInputMessageContext":{{}}}}}}}}}}"#
        );
        let arn = Some("new-arn".to_string());
        let result = KiroProvider::apply_credential_overrides(&body, &arn, ClientMode::KiroIde);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_apply_credential_overrides_invalid_json_returns_input() {
        let body = "not-valid-json";
        let result = KiroProvider::apply_credential_overrides(
            body,
            &Some("arn:test".to_string()),
            ClientMode::KiroCli,
        );
        assert_eq!(result, "not-valid-json");
    }

    #[test]
    fn test_apply_credential_overrides_missing_user_input_keeps_arn_only() {
        // 没有 currentMessage 的 body 不应崩溃，仅注入 profileArn
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let arn = Some("arn-x".to_string());
        let result = KiroProvider::apply_credential_overrides(body, &arn, ClientMode::KiroCli);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "arn-x");
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }
}
