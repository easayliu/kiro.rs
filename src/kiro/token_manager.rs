//! Token 管理模块
//!
//! 负责 Token 过期检测和刷新，支持 Social 和 IdC 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::{bail, Context};
use chrono::{DateTime, Duration, Utc};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as TokioMutex;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::errors::NoAvailableCredentialsError;
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::token_refresh::{
    IdcRefreshRequest, IdcRefreshResponse, RefreshRequest, RefreshResponse,
};
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::model::config::{Config, ProxyGroupConfig};

/// 检查 Token 是否在指定时间内过期
pub(crate) fn is_token_expiring_within(
    credentials: &KiroCredentials,
    minutes: i64,
) -> Option<bool> {
    credentials
        .expires_at
        .as_ref()
        .and_then(|expires_at| DateTime::parse_from_rfc3339(expires_at).ok())
        .map(|expires| expires <= Utc::now() + Duration::minutes(minutes))
}

/// 检查 Token 是否已过期（提前 5 分钟判断）
pub(crate) fn is_token_expired(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 5).unwrap_or(true)
}

/// 检查 Token 是否即将过期（10分钟内）
pub(crate) fn is_token_expiring_soon(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 10).unwrap_or(false)
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// 验证 refreshToken 的基本有效性
pub(crate) fn validate_refresh_token(credentials: &KiroCredentials) -> anyhow::Result<()> {
    let refresh_token = credentials
        .refresh_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;

    if refresh_token.is_empty() {
        bail!("refreshToken 为空");
    }

    if refresh_token.len() < 100 || refresh_token.ends_with("...") || refresh_token.contains("...")
    {
        bail!(
            "refreshToken 已被截断（长度: {} 字符）。\n\
             这通常是 Kiro IDE 为了防止凭证被第三方工具使用而故意截断的。",
            refresh_token.len()
        );
    }

    Ok(())
}

/// Refresh Token 永久失效错误
///
/// 当服务端返回 400 + `invalid_grant` 时，表示 refreshToken 已被撤销或过期，
/// 不应重试，需立即禁用对应凭据。
#[derive(Debug)]
pub(crate) struct RefreshTokenInvalidError {
    pub message: String,
}

impl fmt::Display for RefreshTokenInvalidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RefreshTokenInvalidError {}

/// 刷新 Token
pub(crate) async fn refresh_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    // API Key 凭据不需要刷新，直接返回
    if credentials.is_api_key_credential() {
        return Ok(credentials.clone());
    }

    validate_refresh_token(credentials)?;

    // 根据 auth_method 选择刷新方式
    // 如果未指定 auth_method，根据是否有 clientId/clientSecret 自动判断
    let auth_method = credentials.auth_method.as_deref().unwrap_or_else(|| {
        if credentials.client_id.is_some() && credentials.client_secret.is_some() {
            "idc"
        } else {
            "social"
        }
    });

    if auth_method.eq_ignore_ascii_case("idc")
        || auth_method.eq_ignore_ascii_case("builder-id")
        || auth_method.eq_ignore_ascii_case("iam")
    {
        refresh_idc_token(credentials, config, proxy).await
    } else {
        refresh_social_token(credentials, config, proxy).await
    }
}

/// 刷新 Social Token
async fn refresh_social_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!(
        "正在刷新 Social Token... (凭据 #{})",
        credentials.id.map(|i| i.to_string()).unwrap_or_else(|| "?".to_string())
    );

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);

    let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);
    let refresh_domain = format!("prod.{}.auth.desktop.kiro.dev", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config)
        .unwrap_or_default();
    let mode = credentials.effective_client_mode(config);
    let refresh_ua = config.refresh_user_agent(&machine_id, mode);

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = RefreshRequest {
        refresh_token: refresh_token.to_string(),
    };

    let mut req = client
        .post(&refresh_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", &refresh_ua);

    if mode.is_cli() {
        // kiro-cli 风格: 精简 header，无 host/connection
        req = req
            .header("Accept", "*/*")
            .header("Accept-Encoding", "gzip");
    } else {
        // KiroIDE 风格: 原有行为
        req = req
            .header("Accept", "application/json, text/plain, */*")
            .header("Accept-Encoding", "gzip, compress, deflate, br")
            .header("host", &refresh_domain)
            .header("Connection", "close");
    }

    let response = req
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("Social refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "OAuth 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OAuth 服务暂时不可用",
            _ => "Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: RefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    Ok(new_credentials)
}

/// IdC 刷新 HTTP 错误（非 2xx），携带状态码供 region 兜底重试判断
#[derive(Debug)]
struct IdcRefreshHttpError {
    status: u16,
    message: String,
}

impl fmt::Display for IdcRefreshHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for IdcRefreshHttpError {}

/// Kiro / CodeWhisperer Q 实际提供 `ListAvailableProfiles` / `getUsageLimits` 服务的 region。
///
/// 实测仅 us-east-1、eu-central-1 两地有 `q.{region}.amazonaws.com` 端点（其余 region 连接即
/// EOF）。企业 IdC 的 profile 可能开在其中任意一地，而 SSO 认证 region 未必与之相同，故探测
/// profileArn 时需逐个 region 尝试。顺序：us-east-1 优先（最常见），再 eu-central-1。
const KIRO_PROFILE_REGIONS: [&str; 2] = ["us-east-1", "eu-central-1"];

/// 探测 profileArn 的候选 region（按尝试顺序，去重）：
/// 1. 凭据生效 API region（api_region > profileArn 内嵌 > config）——尊重用户/历史显式配置
/// 2. [`KIRO_PROFILE_REGIONS`] 中其余有服务的 region
///
/// 企业 IdC 的 profile 与 SSO 认证 region 经常不在同一地（如认证 us-east-1、profile eu-central-1），
/// 只探测单一 region 会漏掉 profile，导致 getUsageLimits 因缺 profileArn 失败。
fn profile_probe_region_candidates(credentials: &KiroCredentials, config: &Config) -> Vec<String> {
    let mut candidates = vec![credentials.effective_api_region(config).to_string()];
    for region in KIRO_PROFILE_REGIONS {
        if !candidates.iter().any(|c| c == region) {
            candidates.push(region.to_string());
        }
    }
    candidates
}

/// IdC 刷新候选 auth region（按尝试顺序，去重）：
/// 1. 凭据生效 auth region（authRegion > region > config）
/// 2. us-east-1（Kiro IdC / Builder ID 最常见的注册地）
/// 3. profileArn 内嵌 region
///
/// 企业 IdC 的 SSO 认证 region 与 Q API region 经常不同（如目录在 us-east-1、profile 在
/// eu-central-1），用户照 API 抓包把 region 填成 API region 时，刷新会打错 OIDC 端点，
/// 返回 400 invalid_request "Invalid token provided"。
fn idc_refresh_region_candidates(credentials: &KiroCredentials, config: &Config) -> Vec<String> {
    let mut candidates = vec![credentials.effective_auth_region(config).to_string()];
    let fallbacks = std::iter::once("us-east-1").chain(credentials.profile_arn_region());
    for region in fallbacks {
        if !candidates.iter().any(|c| c == region) {
            candidates.push(region.to_string());
        }
    }
    candidates
}

/// 刷新 IdC Token (AWS SSO OIDC)
///
/// auth region 配置错误导致的 400 按 [`idc_refresh_region_candidates`] 兜底重试，
/// 命中后把正确 region 写回凭据 `auth_region`（随凭据持久化，下次刷新直达）。
async fn refresh_idc_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    let candidates = idc_refresh_region_candidates(credentials, config);
    let last = candidates.len() - 1;
    for (i, region) in candidates.iter().enumerate() {
        match refresh_idc_token_at(credentials, config, proxy, region).await {
            Ok(new_credentials) => return Ok(new_credentials),
            // refreshToken 永久失效（invalid_grant）：换 region 也救不回来，立即返回
            Err(e) if e.is::<RefreshTokenInvalidError>() => return Err(e),
            // 400 多为 region 不匹配（token/client 注册在其他 region），还有候选就继续
            Err(e)
                if i < last
                    && e.downcast_ref::<IdcRefreshHttpError>()
                        .is_some_and(|he| he.status == 400) =>
            {
                tracing::warn!(
                    "IdC Token 在 region {} 刷新失败（{}），尝试候选 region {}",
                    region,
                    e,
                    candidates[i + 1]
                );
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("候选 region 列表非空，循环内必有返回")
}

/// 按指定 region 刷新 IdC Token
async fn refresh_idc_token_at(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
    region: &str,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!(
        "正在刷新 IdC Token... (凭据 #{}, region {})",
        credentials.id.map(|i| i.to_string()).unwrap_or_else(|| "?".to_string()),
        region
    );

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientId"))?;
    let client_secret = credentials
        .client_secret
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientSecret"))?;

    let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    let x_amz_user_agent = "aws-sdk-js/3.980.0 KiroIDE";
    let user_agent = format!(
        "aws-sdk-js/3.980.0 ua/2.1 os/{} lang/js md/nodejs#{} api/sso-oidc#3.980.0 m/E KiroIDE",
        os_name, node_version
    );

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = IdcRefreshRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        refresh_token: refresh_token.to_string(),
        grant_type: "refresh_token".to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("content-type", "application/json")
        .header("x-amz-user-agent", x_amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=4")
        .header("Connection", "close")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("IdC refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "IdC 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OIDC 服务暂时不可用",
            _ => "IdC Token 刷新失败",
        };
        return Err(IdcRefreshHttpError {
            status: status.as_u16(),
            message: format!("{}: {} {}", error_msg, status, body_text),
        }
        .into());
    }

    let data: IdcRefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    // 兜底命中的 region 与凭据原生效 auth region 不同时写回，随凭据持久化，下次刷新直达。
    // 须在下方 profileArn 探测前完成，让探测也用纠正后的 region。
    if credentials.effective_auth_region(config) != region {
        tracing::info!(
            "IdC auth region 已纠正为 {}（原配置 {}）",
            region,
            credentials.effective_auth_region(config)
        );
        new_credentials.auth_region = Some(region.to_string());
    }
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    // 同步更新 profile_arn（如果 IdC 响应中包含）
    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    // 企业 IdC / Builder ID 的 token 不携带 profileArn，但 getUsageLimits / 对话都需要账号自有
    // ARN。对齐真实 Kiro IDE：登录后调用 ListAvailableProfiles 探测并写回 profile_arn，之后随
    // 凭据持久化、后续请求复用。探测失败不阻断刷新（仅记日志，本次拿不到额度但 token 仍有效）。
    //
    // profile 可能开在与 SSO 认证 region 不同的地方（如认证 us-east-1、profile eu-central-1），
    // 故逐个候选 region 探测，命中即止（profileArn 内嵌 region 会驱动后续 getUsageLimits/对话）。
    if new_credentials
        .profile_arn
        .as_deref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
        && let Some(access_token) = new_credentials.access_token.as_deref()
    {
        let probe_regions = profile_probe_region_candidates(&new_credentials, config);
        for probe_region in &probe_regions {
            match list_available_profiles(
                &new_credentials,
                config,
                access_token,
                proxy,
                probe_region,
            )
            .await
            {
                Ok(Some(arn)) => {
                    tracing::info!("IdC 账号在 region {} 探测到 profileArn: {}", probe_region, arn);
                    new_credentials.profile_arn = Some(arn);
                    break;
                }
                Ok(None) => tracing::debug!(
                    "IdC 账号在 region {} 无可用 profile，尝试下一候选",
                    probe_region
                ),
                Err(e) => tracing::warn!(
                    "IdC 账号在 region {} ListAvailableProfiles 探测失败（不阻断刷新）: {}",
                    probe_region,
                    e
                ),
            }
        }
        if new_credentials.profile_arn.is_none() {
            tracing::warn!(
                "IdC 账号在所有候选 region {:?} 均未探测到 profileArn",
                probe_regions
            );
        }
    }

    Ok(new_credentials)
}

/// 探测 SSO OIDC（企业 IdC / Builder ID）账号自有的 profileArn。
///
/// 对齐真实 Kiro IDE：登录后 `POST https://q.{region}.amazonaws.com/ListAvailableProfiles`，
/// body `{}`，仅凭 Bearer token 鉴权（无需 profileArn），返回账号可用的 profile 列表，取第一个
/// `arn`。企业 IdC 的 token 不携带 profileArn，必须经此探测拿到，否则 getUsageLimits / 对话
/// 都会被上游 400 Invalid profileArn 拒绝。
///
/// 失败（网络/非 2xx/无 profile）返回 `Ok(None)`，由调用方决定是否致命——探测本身不应打断刷新。
pub(crate) async fn list_available_profiles(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    region: &str,
) -> anyhow::Result<Option<String>> {
    let host = format!("q.{}.amazonaws.com", region);
    let url = format!("https://{}/ListAvailableProfiles", host);
    let machine_id = machine_id::generate_from_credentials(credentials, config)
        .unwrap_or_default();
    let mode = credentials.effective_client_mode(config);
    let user_agent = config.runtime_user_agent(&machine_id, mode);
    let amz_user_agent = config.runtime_x_amz_user_agent(&machine_id, mode);

    let client = build_client(proxy, 60, config.tls_backend)?;
    let response = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-amz-user-agent", &amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", token))
        .header("Connection", "close")
        .body("{}")
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        tracing::warn!("ListAvailableProfiles 返回 {}：{}", status, body);
        return Ok(None);
    }

    let json: serde_json::Value = response.json().await?;
    Ok(extract_first_profile_arn(&json))
}

/// 从 ListAvailableProfiles 响应里取第一个 profile 的 ARN（兼容字段名差异）。
fn extract_first_profile_arn(json: &serde_json::Value) -> Option<String> {
    let arr = json
        .get("profiles")
        .or_else(|| json.get("availableProfiles"))
        .and_then(|v| v.as_array())?;
    arr.iter().find_map(|p| {
        p.get("arn")
            .or_else(|| p.get("profileArn"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    })
}

/// 获取使用额度信息。
///
/// region 以 profileArn 内嵌的为准（企业 IdC 的 Q API region 未必等于 SSO 认证 region），
/// 缺失时回退到凭据生效 region。profileArn 三级优先级（见
/// [`KiroCredentials::effective_profile_arn`]）：凭据自带 → `ListAvailableProfiles` 探测写回
/// → 按 auth_method 兜底共享 ARN（企业 IdC 不兜，避免外来 ARN 触发 403 Invalid token）。
pub(crate) async fn get_usage_limits(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<UsageLimitsResponse> {
    tracing::debug!("正在获取使用额度信息...");

    let region = credentials.effective_api_region(config).to_string();
    let host = format!("q.{}.amazonaws.com", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config)
        .unwrap_or_default();
    let mode = credentials.effective_client_mode(config);

    // 构建 URL（对齐真实 Kiro IDE：isEmailRequired=true → origin → profileArn → resourceType）
    let mut url = format!(
        "https://{}/getUsageLimits?isEmailRequired=true&origin={}&resourceType=AGENTIC_REQUEST",
        host, mode.origin()
    );

    if let Some(profile_arn) = credentials.effective_profile_arn() {
        url.push_str(&format!("&profileArn={}", urlencoding::encode(&profile_arn)));
    }

    let user_agent = config.runtime_user_agent(&machine_id, mode);
    let amz_user_agent = config.runtime_x_amz_user_agent(&machine_id, mode);

    let client = build_client(proxy, 60, config.tls_backend)?;

    let mut request = client
        .get(&url)
        .header("x-amz-user-agent", &amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", token))
        .header("Connection", "close");

    if credentials.is_api_key_credential() {
        request = request.header("tokentype", "API_KEY");
    }

    let response = request.send().await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let error_msg = match status.as_u16() {
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法获取使用额度",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "获取使用额度失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    // 先读原始 body，便于排查上游真实返回了哪些字段（我们的模型会静默丢弃未知字段）
    let body_text = response.text().await?;
    tracing::debug!("getUsageLimits 原始响应: {}", body_text);

    let data: UsageLimitsResponse = serde_json::from_str(&body_text)
        .with_context(|| format!("解析 getUsageLimits 响应失败: {}", body_text))?;
    Ok(data)
}

/// 切换 overage（超额计费）开关：POST setUserPreference。
///
/// 对齐真实 Kiro IDE 与参考实现（main.py `kiro_set_overage`）：body 为
/// `{overageConfiguration:{overageStatus:ENABLED|DISABLED}, profileArn}`。
/// region / header / profileArn 来源与 [`get_usage_limits`] 一致（企业 IdC 的 Q API
/// region 以 profileArn 内嵌为准，缺失回退凭据生效 region）。无 profileArn 直接报错——
/// setUserPreference 必须带 profileArn，否则上游 400 Invalid profileArn。
pub(crate) async fn set_overage(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    enabled: bool,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<()> {
    let profile_arn = credentials
        .effective_profile_arn()
        .ok_or_else(|| anyhow::anyhow!("缺少 profileArn，无法切换 overage"))?;

    let region = credentials.effective_api_region(config).to_string();
    let host = format!("q.{}.amazonaws.com", region);
    let url = format!("https://{}/setUserPreference", host);
    let machine_id = machine_id::generate_from_credentials(credentials, config)
        .unwrap_or_default();
    let mode = credentials.effective_client_mode(config);

    let body = serde_json::json!({
        "overageConfiguration": {
            "overageStatus": if enabled { "ENABLED" } else { "DISABLED" },
        },
        "profileArn": profile_arn,
    });

    let user_agent = config.runtime_user_agent(&machine_id, mode);
    let amz_user_agent = config.runtime_x_amz_user_agent(&machine_id, mode);

    let client = build_client(proxy, 30, config.tls_backend)?;

    let mut request = client
        .post(&url)
        .header("x-amz-user-agent", &amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .header("Connection", "close")
        .json(&body);

    if credentials.is_api_key_credential() {
        request = request.header("tokentype", "API_KEY");
    }

    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        bail!("切换 overage 失败: {} {}", status, body_text);
    }
    Ok(())
}

/// 拉取指定凭据可用的上游模型列表：GET ListAvailableModels，返回 modelId 列表。
///
/// 请求构造对齐 [`crate::kiro::provider::KiroProvider::list_available_models`]
/// （老端点 `q.{region}` + origin=AI_EDITOR + 可选 profileArn，streaming UA），
/// 区别仅在凭据/token 由调用方按 id 指定，供 Admin API 按凭据查询其真实可用模型。
pub(crate) async fn list_available_models(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<Vec<String>> {
    let region = credentials.effective_api_region(config).to_string();
    let host = format!("q.{}.amazonaws.com", region);
    let url = format!("https://{}/ListAvailableModels", host);
    let machine_id =
        machine_id::generate_from_credentials(credentials, config).unwrap_or_default();
    let mode = credentials.effective_client_mode(config);

    // ListAvailableModels 用 GET + query 参数（对齐 Kiro IDE：origin=AI_EDITOR，带 profileArn 时附上）
    let mut params: Vec<(&str, String)> = vec![("origin", "AI_EDITOR".to_string())];
    if let Some(arn) = credentials.effective_profile_arn() {
        params.push(("profileArn", arn));
    }

    let user_agent = config.streaming_user_agent(&machine_id, mode);
    let amz_user_agent = config.streaming_x_amz_user_agent(&machine_id, mode);

    let client = build_client(proxy, 30, config.tls_backend)?;

    let mut request = client.get(&url).query(&params);
    if credentials.is_api_key_credential() {
        request = request.header("tokentype", "API_KEY");
    }
    let response = request
        .header("x-amz-user-agent", &amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("Authorization", format!("Bearer {}", token))
        .header("Connection", "close")
        .send()
        .await?;

    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        bail!("ListAvailableModels HTTP {}: {}", status, snippet);
    }

    let json: serde_json::Value = serde_json::from_str(&body)?;
    let Some(models) = json.get("models").and_then(|m| m.as_array()) else {
        bail!("ListAvailableModels 响应缺少 models 数组");
    };
    Ok(models
        .iter()
        .filter_map(|m| m.get("modelId").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .collect())
}

// ============================================================================
// 多凭据 Token 管理器
// ============================================================================

/// 单个凭据条目的状态
struct CredentialEntry {
    /// 凭据唯一 ID
    id: u64,
    /// 凭据信息
    credentials: KiroCredentials,
    /// API 调用连续失败次数
    failure_count: u32,
    /// Token 刷新连续失败次数
    refresh_failure_count: u32,
    /// 是否已禁用
    disabled: bool,
    /// 禁用原因（用于区分手动禁用 vs 自动禁用，便于自愈）
    disabled_reason: Option<DisabledReason>,
    /// API 调用成功次数
    success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    last_used_at: Option<String>,
    /// 上游 API 429 限流冷却到期时间（None=未限流，进程内状态不持久化）
    throttled_until: Option<DateTime<Utc>>,
    /// 连续 429 次数，用于指数退避；成功一次即清零
    throttle_count: u32,
    /// RPM 滑动窗口：最近 60s 内的请求时间戳，超过 60s 的会被惰性裁剪
    rpm_window: VecDeque<DateTime<Utc>>,
}

/// 禁用原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledReason {
    /// Admin API 手动禁用
    Manual,
    /// 连续失败达到阈值后自动禁用
    TooManyFailures,
    /// Token 刷新连续失败达到阈值后自动禁用
    TooManyRefreshFailures,
    /// 额度已用尽（如 MONTHLY_REQUEST_COUNT）
    QuotaExceeded,
    /// Refresh Token 永久失效（服务端返回 invalid_grant）
    InvalidRefreshToken,
    /// 凭据配置无效（如 authMethod=api_key 但缺少 kiroApiKey）
    InvalidConfig,
    /// 订阅等级为 Free（确认 subscription_title 含 FREE）后自动禁用；
    /// 升级到 PRO+ 等非 Free 等级时会自动解除（自愈），也可手动重新启用。
    FreeSubscription,
}

/// 统计数据持久化条目
#[derive(Serialize, Deserialize)]
struct StatsEntry {
    success_count: u64,
    last_used_at: Option<String>,
}

// ============================================================================
// Admin API 公开结构
// ============================================================================

/// 凭据条目快照（用于 Admin API 读取）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEntrySnapshot {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级
    pub priority: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// Token 过期时间
    pub expires_at: Option<String>,
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
    /// 凭据所属代理分组（用于前端展示）
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
    /// 凭据级 RPM 上限（None=未单独配置，回退到全局默认；0=显式不限流）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,
    /// 最近 60s 滑动窗口内的请求数
    #[serde(default)]
    pub rpm_current: u32,
    /// overage（超额计费）上次下发状态（None=从未下发，状态未知）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage: Option<bool>,
}

/// 凭据管理器状态快照
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSnapshot {
    /// 凭据条目列表
    pub entries: Vec<CredentialEntrySnapshot>,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 总凭据数量
    pub total: usize,
    /// 可用凭据数量
    pub available: usize,
    /// 全局默认 RPM 上限（None=未配置；0 等价于未配置，仅作显式禁用提示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_rpm_limit: Option<u32>,
}

/// 批量设置代理分组 - 单条失败信息
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetGroupFailure {
    pub id: u64,
    pub error: String,
}

/// 批量设置代理分组结果
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSetGroupResult {
    pub succeeded: Vec<u64>,
    pub failed: Vec<BatchSetGroupFailure>,
}

fn normalize_group_name(group: Option<String>) -> Option<String> {
    group.and_then(|g| {
        let trimmed = g.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// 多凭据 Token 管理器
///
/// 支持多个凭据的管理，实现固定优先级 + 故障转移策略
/// 故障统计基于 API 调用结果，而非 Token 刷新结果
pub struct MultiTokenManager {
    config: Config,
    proxy: Option<ProxyConfig>,
    /// 凭据条目列表
    entries: Mutex<Vec<CredentialEntry>>,
    /// 当前活动凭据 ID
    current_id: Mutex<u64>,
    /// Token 刷新锁表（按凭据 id 分桶）
    ///
    /// 每个凭据一把锁：同一凭据的并发刷新被串行化并去重，
    /// 不同凭据的刷新可并行进行，避免一把全局锁把所有凭据的刷新串成队列。
    /// 外层 `Mutex<HashMap>` 仅用于取/插入桶（短临界区、无 IO）；
    /// 真正跨网络刷新持有的是桶内的 `TokioMutex`。
    refresh_locks: Mutex<HashMap<u64, Arc<TokioMutex<()>>>>,
    /// 凭据文件路径（用于回写）
    credentials_path: Option<PathBuf>,
    /// 是否为多凭据格式（数组格式才回写）
    is_multiple_format: bool,
    /// 负载均衡模式（运行时可修改）
    load_balancing_mode: Mutex<String>,
    /// 全局默认 RPM 上限（运行时可变；初始化自 config，Admin API 修改后实时生效并持久化）
    default_rpm_limit: Mutex<Option<u32>>,
    /// 是否有任何 RPM 配置启用（凭据级 or 全局），用于热路径短路
    ///
    /// 单向 monotonic：一旦置 true 不再清零。误报（实际全关后仍为 true）
    /// 只会让 `record_request_for_rpm` 多走一次 effective_rpm_limit 检查，
    /// 不影响正确性；漏报（实际开启但仍为 false）才会导致漏记，所以保守置 true。
    rpm_feature_enabled: AtomicBool,
    /// 代理分组（运行时可修改，与 config.json 中 proxyGroups 同步）
    proxy_groups: RwLock<BTreeMap<String, ProxyGroupConfig>>,
    /// 最近一次统计持久化时间（用于 debounce）
    last_stats_save_at: Mutex<Option<Instant>>,
    /// 统计数据是否有未落盘更新
    stats_dirty: AtomicBool,
}

/// 每个凭据最大 API 调用失败次数
const MAX_FAILURES_PER_CREDENTIAL: u32 = 3;
/// 统计数据持久化防抖间隔
const STATS_SAVE_DEBOUNCE: StdDuration = StdDuration::from_secs(30);
/// RPM 滑动窗口长度（秒）
const RPM_WINDOW_SECS: i64 = 60;

/// 根据凭据级与全局默认计算生效的 RPM 上限
///
/// 优先级：凭据级 > 全局；任意一级显式为 0 视为"不限流"（停用该层级的限制）。
/// 返回 None 表示无 RPM 限制。
fn effective_rpm_limit(cred: &KiroCredentials, default: Option<u32>) -> Option<u32> {
    match cred.rpm_limit {
        Some(0) => None,
        Some(n) => Some(n),
        None => match default {
            Some(0) => None,
            Some(n) => Some(n),
            None => None,
        },
    }
}

/// 比较两个候选凭据，返回 new 是否优于 current
///
/// 规则：
/// - priority 数字越小越优
/// - 同 priority 时，balanced 模式按 last_used_at 升序（None 视为最早，优先派给新凭据）
/// - 同 priority 时，priority 模式保持稳定（不替换）
/// - current=None 时新候选总是更优
fn is_better(new: &CredentialEntry, current: Option<&CredentialEntry>, balanced: bool) -> bool {
    let Some(cur) = current else {
        return true;
    };
    if new.credentials.priority != cur.credentials.priority {
        return new.credentials.priority < cur.credentials.priority;
    }
    if balanced {
        // LRU within tier; None < Some (从未使用排最前)
        match (&new.last_used_at, &cur.last_used_at) {
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (Some(a), Some(b)) => a < b,
            (None, None) => false,
        }
    } else {
        false
    }
}

/// 裁剪窗口里早于 cutoff 的时间戳
fn prune_rpm_window(window: &mut VecDeque<DateTime<Utc>>, cutoff: DateTime<Utc>) {
    while let Some(front) = window.front() {
        if *front < cutoff {
            window.pop_front();
        } else {
            break;
        }
    }
}

/// 上游 API 429 限流冷却时长表（秒）：第 N 次 429 取第 N-1 个元素，
/// 超过表长后取最后一个（封顶）。
const THROTTLE_BACKOFF_SECS: &[i64] = &[10, 20, 30, 60];

/// API 调用上下文
///
/// 绑定特定凭据的调用上下文，确保 token、credentials 和 id 的一致性
/// 用于解决并发调用时 current_id 竞态问题
#[derive(Clone)]
pub struct CallContext {
    /// 凭据 ID（用于 report_success/report_failure）
    pub id: u64,
    /// 凭据信息（用于构建请求头）
    pub credentials: KiroCredentials,
    /// 访问 Token
    pub token: String,
}

impl MultiTokenManager {
    /// 创建多凭据 Token 管理器
    ///
    /// # Arguments
    /// * `config` - 应用配置
    /// * `credentials` - 凭据列表
    /// * `proxy` - 可选的代理配置
    /// * `credentials_path` - 凭据文件路径（用于回写）
    /// * `is_multiple_format` - 原始文件是否为数组格式；仅用于日志/兼容，
    ///   回写时一律使用数组格式（单对象格式会在首次持久化时自动升级）
    pub fn new(
        config: Config,
        credentials: Vec<KiroCredentials>,
        proxy: Option<ProxyConfig>,
        credentials_path: Option<PathBuf>,
        is_multiple_format: bool,
    ) -> anyhow::Result<Self> {
        // 计算当前最大 ID，为没有 ID 的凭据分配新 ID
        let max_existing_id = credentials.iter().filter_map(|c| c.id).max().unwrap_or(0);
        let mut next_id = max_existing_id + 1;
        let mut has_new_ids = false;
        let mut has_new_machine_ids = false;
        let config_ref = &config;

        let entries: Vec<CredentialEntry> = credentials
            .into_iter()
            .map(|mut cred| {
                cred.canonicalize_auth_method();
                let id = cred.id.unwrap_or_else(|| {
                    let id = next_id;
                    next_id += 1;
                    cred.id = Some(id);
                    has_new_ids = true;
                    id
                });
                if cred.machine_id.is_none() {
                    if let Some(machine_id) =
                        machine_id::generate_from_credentials(&cred, config_ref)
                    {
                        cred.machine_id = Some(machine_id);
                        has_new_machine_ids = true;
                    }
                }
                CredentialEntry {
                    id,
                    credentials: cred.clone(),
                    failure_count: 0,
                    refresh_failure_count: 0,
                    disabled: cred.disabled, // 从配置文件读取 disabled 状态
                    disabled_reason: if cred.disabled {
                        Some(DisabledReason::Manual)
                    } else {
                        None
                    },
                    success_count: 0,
                    last_used_at: None,
                    throttled_until: None,
                    throttle_count: 0,
                    rpm_window: VecDeque::new(),
                }
            })
            .collect();

        // 校验 API Key 凭据配置完整性：authMethod=api_key 时必须提供 kiroApiKey
        let mut entries = entries;
        for entry in &mut entries {
            if entry.credentials.kiro_api_key.is_none()
                && entry
                    .credentials
                    .auth_method
                    .as_deref()
                    .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                    .unwrap_or(false)
            {
                tracing::warn!(
                    "凭据 #{} 配置了 authMethod=api_key 但缺少 kiroApiKey 字段，已自动禁用",
                    entry.id
                );
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::InvalidConfig);
            }
        }

        // 检测重复 ID
        let mut seen_ids = std::collections::HashSet::new();
        let mut duplicate_ids = Vec::new();
        for entry in &entries {
            if !seen_ids.insert(entry.id) {
                duplicate_ids.push(entry.id);
            }
        }
        if !duplicate_ids.is_empty() {
            anyhow::bail!("检测到重复的凭据 ID: {:?}", duplicate_ids);
        }

        // 选择初始凭据：优先级最高（priority 最小）的可用凭据，无可用凭据时为 0
        let initial_id = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| e.credentials.priority)
            .map(|e| e.id)
            .unwrap_or(0);

        let load_balancing_mode = config.load_balancing_mode.clone();
        let proxy_groups = config.proxy_groups.clone();
        let default_rpm_limit = config.default_rpm_limit;
        let any_cred_rpm = entries
            .iter()
            .any(|e| e.credentials.rpm_limit.unwrap_or(0) > 0);
        let any_default_rpm = default_rpm_limit.unwrap_or(0) > 0;
        let manager = Self {
            config,
            proxy,
            entries: Mutex::new(entries),
            current_id: Mutex::new(initial_id),
            refresh_locks: Mutex::new(HashMap::new()),
            credentials_path,
            is_multiple_format,
            load_balancing_mode: Mutex::new(load_balancing_mode),
            default_rpm_limit: Mutex::new(default_rpm_limit),
            rpm_feature_enabled: AtomicBool::new(any_cred_rpm || any_default_rpm),
            proxy_groups: RwLock::new(proxy_groups),
            last_stats_save_at: Mutex::new(None),
            stats_dirty: AtomicBool::new(false),
        };

        // 如果有新分配的 ID 或新生成的 machineId，立即持久化到配置文件
        if has_new_ids || has_new_machine_ids {
            if let Err(e) = manager.persist_credentials() {
                tracing::warn!("补全凭据 ID/machineId 后持久化失败: {}", e);
            } else {
                tracing::info!("已补全凭据 ID/machineId 并写回配置文件");
            }
        }

        // 加载持久化的统计数据（success_count, last_used_at）
        manager.load_stats();

        Ok(manager)
    }

    /// 获取配置的引用
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 获取当前生效的代理分组配置（克隆快照）
    pub fn proxy_groups_snapshot(&self) -> BTreeMap<String, ProxyGroupConfig> {
        self.proxy_groups.read().clone()
    }

    /// 列出当前所有代理分组（Admin API）
    pub fn list_proxy_groups(&self) -> BTreeMap<String, ProxyGroupConfig> {
        self.proxy_groups_snapshot()
    }

    /// 新增或更新代理分组（Admin API）
    pub fn upsert_proxy_group(
        &self,
        name: String,
        group: ProxyGroupConfig,
    ) -> anyhow::Result<()> {
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            anyhow::bail!("代理分组名称不能为空");
        }
        if group.proxy_url.trim().is_empty() {
            anyhow::bail!("代理分组 '{}' 的 proxyUrl 不能为空", trimmed);
        }

        let previous = {
            let mut groups = self.proxy_groups.write();
            let prev = groups.insert(trimmed.clone(), group.clone());
            prev
        };

        if let Err(err) = self.persist_proxy_groups() {
            // 持久化失败时回滚
            let mut groups = self.proxy_groups.write();
            match previous {
                Some(p) => {
                    groups.insert(trimmed, p);
                }
                None => {
                    groups.remove(&trimmed);
                }
            }
            return Err(err);
        }
        Ok(())
    }

    /// 删除代理分组（Admin API）
    ///
    /// 引用该分组的凭据会回退到全局代理
    pub fn delete_proxy_group(&self, name: &str) -> anyhow::Result<()> {
        let previous = {
            let mut groups = self.proxy_groups.write();
            groups.remove(name)
        };
        if previous.is_none() {
            anyhow::bail!("代理分组不存在: {}", name);
        }
        if let Err(err) = self.persist_proxy_groups() {
            // 回滚
            if let Some(p) = previous {
                self.proxy_groups.write().insert(name.to_string(), p);
            }
            return Err(err);
        }
        Ok(())
    }

    fn persist_proxy_groups(&self) -> anyhow::Result<()> {
        use anyhow::Context;

        let config_path = match self.config.config_path() {
            Some(path) => path.to_path_buf(),
            None => {
                tracing::warn!("配置文件路径未知，代理分组变更仅在当前进程生效");
                return Ok(());
            }
        };

        let mut config = Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.proxy_groups = self.proxy_groups.read().clone();
        config
            .save()
            .with_context(|| format!("持久化代理分组失败: {}", config_path.display()))?;
        Ok(())
    }

    /// 获取凭据总数
    pub fn total_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// 获取可用凭据数量
    pub fn available_count(&self) -> usize {
        self.entries.lock().iter().filter(|e| !e.disabled).count()
    }

    /// 列出当前可用的凭据 id（供粘性绑定表作为候选池）
    ///
    /// 与 `select_next_credential` 的过滤条件一致：跳过 disabled；
    /// 当 model 为 opus 时还需要凭据支持 opus。
    pub fn available_credential_ids(&self, model: Option<&str>) -> Vec<u64> {
        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);
        let entries = self.entries.lock();
        entries
            .iter()
            .filter(|e| !e.disabled)
            .filter(|e| !is_opus || e.credentials.supports_opus())
            .map(|e| e.id)
            .collect()
    }

    /// 根据负载均衡模式选择下一个凭据
    ///
    /// - priority 模式：选择优先级最高（priority 最小）的可用凭据
    /// - balanced 模式：先按优先级分档，仅在最高优先级一档（priority 最小者）内
    ///   做 LRU 均衡。该档凭据全部 disabled / 不支持目标模型时，自然降级到下一档。
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    fn select_next_credential(&self, model: Option<&str>) -> Option<(u64, KiroCredentials)> {
        let mut entries = self.entries.lock();

        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);

        let now = Utc::now();
        let rpm_active = self.rpm_feature_enabled.load(Ordering::Relaxed);
        let rpm_cutoff = now - Duration::seconds(RPM_WINDOW_SECS);
        // 锁外读不到，但只读一次：在 entries 锁内额外拿一次 default_rpm_limit
        // 锁是短锁，且这两个锁的获取顺序在所有代码路径上一致（entries → default_rpm_limit）。
        let default_rpm = if rpm_active {
            *self.default_rpm_limit.lock()
        } else {
            None
        };
        let is_balanced = self.load_balancing_mode.lock().as_str() == "balanced";

        // 单次扫描同时维护两个候选：
        // - best_fresh：未冷却且 RPM 未耗尽的"鲜活"凭据中的最优解
        // - best_fallback：放宽冷却/RPM 限制的最优解，用于全员限流时的退路
        // 比较规则：低 priority 优先；balanced 模式下同档按 last_used_at 升序（None 最早）
        let mut best_fresh: Option<usize> = None;
        let mut best_fallback: Option<usize> = None;
        let mut had_eligible = false;

        for i in 0..entries.len() {
            let entry = &entries[i];
            if entry.disabled {
                continue;
            }
            if is_opus && !entry.credentials.supports_opus() {
                continue;
            }
            had_eligible = true;

            if is_better(entry, best_fallback.map(|j| &entries[j]), is_balanced) {
                best_fallback = Some(i);
            }

            // 鲜活性检查（throttled / RPM）：失败则不能更新 best_fresh，但 fallback 已记录
            if entry.throttled_until.is_some_and(|until| until > now) {
                continue;
            }
            if rpm_active {
                if let Some(limit) = effective_rpm_limit(&entry.credentials, default_rpm) {
                    // 用 filter().count() 即时计数，避免修改 rpm_window；
                    // 真正的窗口裁剪由 record_request_for_rpm 在选中后做。
                    let count = entry
                        .rpm_window
                        .iter()
                        .filter(|t| **t > rpm_cutoff)
                        .count();
                    if count >= limit as usize {
                        continue;
                    }
                }
            }

            if is_better(entry, best_fresh.map(|j| &entries[j]), is_balanced) {
                best_fresh = Some(i);
            }
        }

        if !had_eligible {
            return None;
        }

        let selected_index = match best_fresh {
            Some(i) => i,
            None => {
                tracing::warn!("所有可用凭据均处于 429 冷却/RPM 限制，回退选择 LRU 最早者");
                best_fallback?
            }
        };

        let entry = &mut entries[selected_index];
        let result = (entry.id, entry.credentials.clone());

        if is_balanced {
            entry.last_used_at = Some(now.to_rfc3339());
        }

        Some(result)
    }

    /// 是否存在至少一个"鲜活"凭据（未禁用、未冷却、RPM 未耗尽、支持目标模型）。
    ///
    /// 用于 429 退避决策：刚被限流的凭据已由 `report_throttled` 打上
    /// `throttled_until`，本方法会自然把它排除。若仍有其它鲜活凭据可立即切换，
    /// 则当前请求无需在首字（TTFT）关键路径上做退避 sleep，直接换号重试即可——
    /// 退避只对"无替补、只能等同一账号冷却结束"的情形才有意义。
    ///
    /// 鲜活性判定与 `select_next_credential` 的 `best_fresh` 分支保持一致。
    pub fn has_fresh_credential(&self, model: Option<&str>) -> bool {
        let entries = self.entries.lock();

        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);

        let now = Utc::now();
        let rpm_active = self.rpm_feature_enabled.load(Ordering::Relaxed);
        let rpm_cutoff = now - Duration::seconds(RPM_WINDOW_SECS);
        // 锁顺序与 select_next_credential 一致：entries → default_rpm_limit
        let default_rpm = if rpm_active {
            *self.default_rpm_limit.lock()
        } else {
            None
        };

        entries.iter().any(|entry| {
            if entry.disabled {
                return false;
            }
            if is_opus && !entry.credentials.supports_opus() {
                return false;
            }
            if entry.throttled_until.is_some_and(|until| until > now) {
                return false;
            }
            if rpm_active {
                if let Some(limit) = effective_rpm_limit(&entry.credentials, default_rpm) {
                    let count = entry
                        .rpm_window
                        .iter()
                        .filter(|t| **t > rpm_cutoff)
                        .count();
                    if count >= limit as usize {
                        return false;
                    }
                }
            }
            true
        })
    }

    /// 获取 API 调用上下文
    ///
    /// 返回绑定了 id、credentials 和 token 的调用上下文
    /// 确保整个 API 调用过程中使用一致的凭据信息
    ///
    /// 如果 Token 过期或即将过期，会自动刷新
    /// Token 刷新失败会累计到当前凭据，达到阈值后禁用并切换
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    /// - `preferred`: 优先使用的凭据 id（来自用户粘性绑定）。仅在第一轮尝试；
    ///   若该凭据不可用或本轮 Token 获取失败，后续轮次走默认选择逻辑。
    pub async fn acquire_context(
        &self,
        model: Option<&str>,
        preferred: Option<u64>,
    ) -> anyhow::Result<CallContext> {
        let total = self.total_count();
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize).max(1);
        let mut attempt_count = 0;
        let mut preferred_pending = preferred;

        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);

        loop {
            if attempt_count >= max_attempts {
                anyhow::bail!(
                    "所有凭据均无法获取有效 Token（可用: {}/{}）",
                    self.available_count(),
                    total
                );
            }

            let (id, credentials) = {
                let now = Utc::now();
                // 第一轮优先尝试粘性绑定凭据（只试一次，后续轮次 fallback）
                // 仍要校验 throttled_until：被上游限流的凭据若被粘性命中，
                // 会绕过冷却继续发送请求并累积 throttle_count，永远逃不出冷却。
                let preferred_hit = preferred_pending.take().and_then(|pid| {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| {
                            e.id == pid
                                && !e.disabled
                                && e.throttled_until.is_none_or(|until| until <= now)
                        })
                        .filter(|e| !is_opus || e.credentials.supports_opus())
                        .map(|e| (e.id, e.credentials.clone()))
                });

                if let Some(hit) = preferred_hit {
                    hit
                } else {
                let is_balanced = self.load_balancing_mode.lock().as_str() == "balanced";

                // balanced 模式：每次请求都重新均衡选择，不固定 current_id
                // priority 模式：优先使用 current_id 指向的凭据
                // 同上：current_id 也要排除冷却中的凭据，避免循环踩 429。
                let current_hit = if is_balanced {
                    None
                } else {
                    let entries = self.entries.lock();
                    let current_id = *self.current_id.lock();
                    entries
                        .iter()
                        .find(|e| {
                            e.id == current_id
                                && !e.disabled
                                && e.throttled_until.is_none_or(|until| until <= now)
                        })
                        .map(|e| (e.id, e.credentials.clone()))
                };

                if let Some(hit) = current_hit {
                    hit
                } else {
                    // 当前凭据不可用或 balanced 模式，根据负载均衡策略选择
                    let mut best = self.select_next_credential(model);

                    // 没有可用凭据：如果是"自动禁用导致全灭"，做一次类似重启的自愈
                    if best.is_none() {
                        let mut entries = self.entries.lock();
                        if entries.iter().any(|e| {
                            e.disabled && e.disabled_reason == Some(DisabledReason::TooManyFailures)
                        }) {
                            tracing::warn!(
                                "所有凭据均已被自动禁用，执行自愈：重置失败计数并重新启用（等价于重启）"
                            );
                            for e in entries.iter_mut() {
                                if e.disabled_reason == Some(DisabledReason::TooManyFailures) {
                                    e.disabled = false;
                                    e.disabled_reason = None;
                                    e.failure_count = 0;
                                }
                            }
                            drop(entries);
                            best = self.select_next_credential(model);
                        }
                    }

                    if let Some((new_id, new_creds)) = best {
                        // 更新 current_id
                        let mut current_id = self.current_id.lock();
                        *current_id = new_id;
                        (new_id, new_creds)
                    } else {
                        let entries = self.entries.lock();
                        // 注意：必须在 bail! 之前计算 available_count，
                        // 因为 available_count() 会尝试获取 entries 锁，
                        // 而此时我们已经持有该锁，会导致死锁
                        let available = entries.iter().filter(|e| !e.disabled).count();
                        anyhow::bail!(NoAvailableCredentialsError {
                            available,
                            total,
                        });
                    }
                }
                }
            };

            // 尝试获取/刷新 Token
            match self.try_ensure_token(id, &credentials).await {
                Ok(ctx) => {
                    self.record_request_for_rpm(ctx.id);
                    return Ok(ctx);
                }
                Err(e) => {
                    // refreshToken 永久失效 → 立即禁用，不累计重试
                    let has_available =
                        if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                            tracing::warn!("凭据 #{} refreshToken 永久失效: {}", id, e);
                            self.report_refresh_token_invalid(id)
                        } else {
                            tracing::warn!("凭据 #{} Token 刷新失败: {}", id, e);
                            self.report_refresh_failure(id)
                        };
                    attempt_count += 1;
                    if !has_available {
                        anyhow::bail!(NoAvailableCredentialsError {
                            available: 0,
                            total,
                        });
                    }
                }
            }
        }
    }

    /// 选择优先级最高的未禁用凭据作为当前凭据（内部方法）
    ///
    /// 纯粹按优先级选择，不排除当前凭据，用于优先级变更后立即生效
    fn select_highest_priority(&self) {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（不排除当前凭据）
        if let Some(best) = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| e.credentials.priority)
        {
            if best.id != *current_id {
                tracing::info!(
                    "优先级变更后切换凭据: #{} -> #{}（优先级 {}）",
                    *current_id,
                    best.id,
                    best.credentials.priority
                );
                *current_id = best.id;
            }
        }
    }

    /// 取指定凭据的刷新锁（不存在则创建）
    ///
    /// 返回 `Arc<TokioMutex>`，调用方需先绑定到局部变量再 `.lock().await`，
    /// 以保证锁对象在 guard 存活期间不被释放。不同 id 返回不同的锁，
    /// 因此不同凭据的刷新互不阻塞。
    fn refresh_lock_for(&self, id: u64) -> Arc<TokioMutex<()>> {
        self.refresh_locks
            .lock()
            .entry(id)
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    /// 尝试使用指定凭据获取有效 Token
    ///
    /// 使用双重检查锁定模式，确保同一凭据同一时间只有一个刷新操作
    ///
    /// # Arguments
    /// * `id` - 凭据 ID，用于更新正确的条目
    /// * `credentials` - 凭据信息
    async fn try_ensure_token(
        &self,
        id: u64,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<CallContext> {
        // API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需刷新
        if credentials.is_api_key_credential() {
            let token = credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            return Ok(CallContext {
                id,
                credentials: credentials.clone(),
                token,
            });
        }

        // 第一次检查（无锁）：快速判断是否需要刷新
        let needs_refresh = is_token_expired(credentials) || is_token_expiring_soon(credentials);

        let creds = if needs_refresh {
            // [TTFT 埋点] 获取刷新锁，确保同一时间只有一个刷新操作。
            // 计 lock_wait：若同一凭据有其它请求正在刷新，这里会排队，能反映锁竞争。
            let refresh_lock = self.refresh_lock_for(id);
            let lock_wait_start = Instant::now();
            let _guard = refresh_lock.lock().await;
            let lock_wait_ms = lock_wait_start.elapsed().as_millis();

            // 第二次检查：获取锁后重新读取凭据，因为其他请求可能已经完成刷新
            let current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据 #{} 不存在", id))?
            };

            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                // 确实需要刷新
                let effective_proxy = current_creds.effective_proxy(self.proxy.as_ref(), &self.proxy_groups.read());

                // [TTFT 埋点] 计 refresh：上游刷新网络往返（IdC 还含 ListAvailableProfiles 探测）
                let refresh_start = Instant::now();
                let new_creds =
                    refresh_token(&current_creds, &self.config, effective_proxy.as_ref()).await?;
                let refresh_ms = refresh_start.elapsed().as_millis();

                if is_token_expired(&new_creds) {
                    anyhow::bail!("刷新后的 Token 仍然无效或已过期");
                }

                // 更新凭据
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = new_creds.clone();
                    }
                }

                // [TTFT 埋点] 计 persist：整文件 O(N) 序列化 + 同步写盘（在请求关键路径上 await）
                let persist_start = Instant::now();
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                }
                let persist_ms = persist_start.elapsed().as_millis();

                tracing::debug!(
                    "[TTFT] 凭据 #{} 内联刷新 access token: lock_wait={}ms refresh={}ms persist={}ms",
                    id,
                    lock_wait_ms,
                    refresh_ms,
                    persist_ms
                );

                new_creds
            } else {
                // 其他请求已经完成刷新，直接使用新凭据
                tracing::debug!(
                    "[TTFT] 凭据 #{} 等刷新锁后命中他人已刷新: lock_wait={}ms",
                    id,
                    lock_wait_ms
                );
                current_creds
            }
        } else {
            credentials.clone()
        };

        let token = creds
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("没有可用的 accessToken"))?;

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.refresh_failure_count = 0;
            }
        }

        Ok(CallContext {
            id,
            credentials: creds,
            token,
        })
    }

    /// 将凭据列表回写到源文件。
    ///
    /// 格式选择：
    /// - 凭据数 > 1：一律数组（避免历史 bug —— 单对象格式多凭据时丢数据）
    /// - 凭据数 == 1 且原格式为数组：数组（尊重用户选择）
    /// - 凭据数 == 1 且原格式为单对象：保持单对象（不强制升级）
    /// - 凭据数 == 0：空数组
    ///
    /// # Returns
    /// - `Ok(true)` - 成功写入文件
    /// - `Ok(false)` - 跳过写入（未配置 credentials_path）
    /// - `Err(_)` - 写入失败
    fn persist_credentials(&self) -> anyhow::Result<bool> {
        use anyhow::Context;

        let path = match &self.credentials_path {
            Some(p) => p,
            None => return Ok(false),
        };

        let credentials: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    cred.disabled = e.disabled;
                    cred
                })
                .collect()
        };

        let json = if credentials.len() == 1 && !self.is_multiple_format {
            serde_json::to_string_pretty(&credentials[0]).context("序列化凭据失败")?
        } else {
            serde_json::to_string_pretty(&credentials).context("序列化凭据失败")?
        };

        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| std::fs::write(path, &json))
                .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        } else {
            std::fs::write(path, &json).with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        }

        tracing::debug!("已回写凭据到文件: {:?}", path);
        Ok(true)
    }

    /// 获取缓存目录（凭据文件所在目录）
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.credentials_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    /// 统计数据文件路径
    fn stats_path(&self) -> Option<PathBuf> {
        self.cache_dir().map(|d| d.join("kiro_stats.json"))
    }

    /// 从磁盘加载统计数据并应用到当前条目
    fn load_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };

        let stats: HashMap<String, StatsEntry> = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("解析统计缓存失败，将忽略: {}", e);
                return;
            }
        };

        let mut entries = self.entries.lock();
        for entry in entries.iter_mut() {
            if let Some(s) = stats.get(&entry.id.to_string()) {
                entry.success_count = s.success_count;
                entry.last_used_at = s.last_used_at.clone();
            }
        }
        *self.last_stats_save_at.lock() = Some(Instant::now());
        self.stats_dirty.store(false, Ordering::Relaxed);
        tracing::info!("已从缓存加载 {} 条统计数据", stats.len());
    }

    /// 将当前统计数据持久化到磁盘
    fn save_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let stats: HashMap<String, StatsEntry> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    (
                        e.id.to_string(),
                        StatsEntry {
                            success_count: e.success_count,
                            last_used_at: e.last_used_at.clone(),
                        },
                    )
                })
                .collect()
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!("保存统计缓存失败: {}", e);
                } else {
                    *self.last_stats_save_at.lock() = Some(Instant::now());
                    self.stats_dirty.store(false, Ordering::Relaxed);
                }
            }
            Err(e) => tracing::warn!("序列化统计数据失败: {}", e),
        }
    }

    /// 标记统计数据已更新，并按 debounce 策略决定是否立即落盘
    fn save_stats_debounced(&self) {
        self.stats_dirty.store(true, Ordering::Relaxed);

        let should_flush = {
            let last = *self.last_stats_save_at.lock();
            match last {
                Some(last_saved_at) => last_saved_at.elapsed() >= STATS_SAVE_DEBOUNCE,
                None => true,
            }
        };

        if should_flush {
            self.save_stats();
        }
    }

    /// 报告指定凭据 API 调用成功
    ///
    /// 重置该凭据的失败计数
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_success(&self, id: u64) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.throttled_until = None;
                entry.throttle_count = 0;
                entry.success_count += 1;
                entry.last_used_at = Some(Utc::now().to_rfc3339());
                tracing::debug!(
                    "凭据 #{} API 调用成功（累计 {} 次）",
                    id,
                    entry.success_count
                );
            }
        }
        self.save_stats_debounced();
    }

    /// 报告凭据被上游 API 限流（429），按 THROTTLE_BACKOFF_SECS 表标记冷却到期时间
    ///
    /// 冷却期间该凭据从可用集合中剔除，balanced 模式会自然降级到下一档。
    /// 冷却时长：第 N 次 429 取 THROTTLE_BACKOFF_SECS[N-1]，超出表长后封顶为表末值。
    /// 一次成功调用（report_success）会清零计数与冷却。
    pub fn report_throttled(&self, id: u64) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.throttle_count = entry.throttle_count.saturating_add(1);
            let idx = (entry.throttle_count as usize - 1).min(THROTTLE_BACKOFF_SECS.len() - 1);
            let delay_secs = THROTTLE_BACKOFF_SECS[idx];
            let now = Utc::now();
            entry.throttled_until = Some(now + Duration::seconds(delay_secs));
            entry.last_used_at = Some(now.to_rfc3339());
            tracing::warn!(
                "凭据 #{} 被上游限流，冷却 {}s（连续第 {} 次）",
                id,
                delay_secs,
                entry.throttle_count
            );
        }
    }

    /// 报告凭据被上游 API 长时间限流，按指定秒数标记冷却到期时间
    ///
    /// 用于处理上游 "suspicious activity" 等需要长时间退避的特殊 429 场景。
    /// 行为类似 `report_throttled`，但忽略 throttle_count，直接使用传入的冷却时长。
    pub fn report_throttled_for(&self, id: u64, secs: i64) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.throttle_count = entry.throttle_count.saturating_add(1);
            let now = Utc::now();
            entry.throttled_until = Some(now + Duration::seconds(secs));
            entry.last_used_at = Some(now.to_rfc3339());
            tracing::warn!(
                "凭据 #{} 被上游长时间限流，冷却 {}s（连续第 {} 次）",
                id,
                secs,
                entry.throttle_count
            );
        }
    }

    /// 记录一次本地请求计入 RPM 滑动窗口（仅当该凭据生效 RPM 上限非空时）
    ///
    /// 在 `acquire_context` 成功获取上下文后调用，统计实际派发到该凭据的请求数。
    /// 即使本次请求最终失败（401/429/网络等），也已计入——与上游对话单元一致。
    fn record_request_for_rpm(&self, id: u64) {
        // 热路径短路：从未启用过 RPM 时直接返回，避免无谓的锁与线性查找
        if !self.rpm_feature_enabled.load(Ordering::Relaxed) {
            return;
        }
        let default_rpm = *self.default_rpm_limit.lock();
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            if effective_rpm_limit(&entry.credentials, default_rpm).is_none() {
                return;
            }
            let now = Utc::now();
            prune_rpm_window(&mut entry.rpm_window, now - Duration::seconds(RPM_WINDOW_SECS));
            entry.rpm_window.push_back(now);
        }
    }

    /// 标记凭据最近被访问过（用于 LRU 轮转，但不视为成功）
    ///
    /// 场景：上游返回 429 / 408 等瞬态限流错误时调用。
    /// 不增加 success_count、不重置 failure_count，仅刷新 last_used_at，
    /// 让 balanced (LRU) 模式把该凭据排到队尾，下一次轮转到其他凭据。
    pub fn mark_accessed(&self, id: u64) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            tracing::debug!("凭据 #{} 标记为最近访问（瞬态错误，用于 LRU 轮转）", id);
        }
    }

    /// 报告指定凭据 API 调用失败
    ///
    /// 增加失败计数，达到阈值时禁用凭据并切换到优先级最高的可用凭据
    /// 返回是否还有可用凭据可以重试
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.failure_count += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            let failure_count = entry.failure_count;

            tracing::warn!(
                "凭据 #{} API 调用失败（{}/{}）",
                id,
                failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if failure_count >= MAX_FAILURES_PER_CREDENTIAL {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::TooManyFailures);
                tracing::error!("凭据 #{} 已连续失败 {} 次，已被禁用", id, failure_count);

                // 切换到优先级最高的可用凭据
                if let Some(next) = entries
                    .iter()
                    .filter(|e| !e.disabled)
                    .min_by_key(|e| e.credentials.priority)
                {
                    *current_id = next.id;
                    tracing::info!(
                        "已切换到凭据 #{}（优先级 {}）",
                        next.id,
                        next.credentials.priority
                    );
                } else {
                    tracing::error!("所有凭据均已禁用！");
                }
            }

            entries.iter().any(|e| !e.disabled)
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据额度已用尽
    ///
    /// 用于处理 402 Payment Required 且 reason 为 `MONTHLY_REQUEST_COUNT` 的场景：
    /// - 立即禁用该凭据（不等待连续失败阈值）
    /// - 切换到下一个可用凭据继续重试
    /// - 返回是否还有可用凭据
    pub fn report_quota_exhausted(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::QuotaExceeded);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;

            tracing::error!("凭据 #{} 额度已用尽（MONTHLY_REQUEST_COUNT），已被禁用", id);

            // 切换到优先级最高的可用凭据
            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据刷新 Token 失败。
    ///
    /// 连续刷新失败达到阈值后禁用凭据并切换，阈值内保持当前凭据不切换，
    /// 与 API 401/403 的累计失败策略保持一致。
    pub fn report_refresh_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.refresh_failure_count += 1;
            let refresh_failure_count = entry.refresh_failure_count;

            tracing::warn!(
                "凭据 #{} Token 刷新失败（{}/{}）",
                id,
                refresh_failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if refresh_failure_count < MAX_FAILURES_PER_CREDENTIAL {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);

            tracing::error!(
                "凭据 #{} Token 已连续刷新失败 {} 次，已被禁用",
                id,
                refresh_failure_count
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据的 refreshToken 永久失效（invalid_grant）。
    ///
    /// 立即禁用凭据，不累计、不重试。
    /// 返回是否还有可用凭据。
    pub fn report_refresh_token_invalid(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::InvalidRefreshToken);

            tracing::error!(
                "凭据 #{} refreshToken 已失效 (invalid_grant)，已立即禁用",
                id
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 切换到优先级最高的可用凭据
    ///
    /// 返回是否成功切换
    pub fn switch_to_next(&self) -> bool {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（排除当前凭据）
        if let Some(next) = entries
            .iter()
            .filter(|e| !e.disabled && e.id != *current_id)
            .min_by_key(|e| e.credentials.priority)
        {
            *current_id = next.id;
            tracing::info!(
                "已切换到凭据 #{}（优先级 {}）",
                next.id,
                next.credentials.priority
            );
            true
        } else {
            // 没有其他可用凭据，检查当前凭据是否可用
            entries.iter().any(|e| e.id == *current_id && !e.disabled)
        }
    }

    // ========================================================================
    // Admin API 方法
    // ========================================================================

    /// 获取管理器状态快照（用于 Admin API）
    pub fn snapshot(&self) -> ManagerSnapshot {
        let entries = self.entries.lock();
        let current_id = *self.current_id.lock();
        let available = entries.iter().filter(|e| !e.disabled).count();
        let now = Utc::now();
        let rpm_cutoff = now - Duration::seconds(RPM_WINDOW_SECS);
        // 仅读不修改：rpm_current 用 filter().count() 即时统计 60s 内的有效条目；
        // 真正的裁剪交给热路径（select_next_credential / record_request_for_rpm），
        // snapshot 不再争抢可变借用，admin 拉取与 API 请求的锁竞争更轻。

        ManagerSnapshot {
            entries: entries
                .iter()
                .map(|e| CredentialEntrySnapshot {
                    id: e.id,
                    priority: e.credentials.priority,
                    disabled: e.disabled,
                    failure_count: e.failure_count,
                    auth_method: if e.credentials.is_api_key_credential() {
                        Some("api_key".to_string())
                    } else {
                        e.credentials.auth_method.as_deref().map(|m| {
                            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam") {
                                "idc".to_string()
                            } else {
                                m.to_string()
                            }
                        })
                    },
                    has_profile_arn: e.credentials.profile_arn.is_some(),
                    expires_at: if e.credentials.is_api_key_credential() {
                        None // API Key 不过期
                    } else {
                        e.credentials.expires_at.clone()
                    },
                    refresh_token_hash: if e.credentials.is_api_key_credential() {
                        // API Key 凭据显示脱敏的 key
                        e.credentials.kiro_api_key.as_deref().map(|k| {
                            if k.is_ascii() && k.len() > 16 {
                                format!("{}...{}", &k[..4], &k[k.len()-4..])
                            } else {
                                "***".to_string()
                            }
                        })
                    } else {
                        e.credentials.refresh_token.as_deref().map(sha256_hex)
                    },
                    email: e.credentials.email.clone(),
                    success_count: e.success_count,
                    last_used_at: e.last_used_at.clone(),
                    has_proxy: e.credentials.proxy_url.is_some(),
                    proxy_url: e.credentials.proxy_url.clone(),
                    group: e.credentials.group.clone(),
                    refresh_failure_count: e.refresh_failure_count,
                    disabled_reason: e.disabled_reason.map(|r| match r {
                        DisabledReason::Manual => "Manual",
                        DisabledReason::TooManyFailures => "TooManyFailures",
                        DisabledReason::TooManyRefreshFailures => "TooManyRefreshFailures",
                        DisabledReason::QuotaExceeded => "QuotaExceeded",
                        DisabledReason::InvalidRefreshToken => "InvalidRefreshToken",
                        DisabledReason::InvalidConfig => "InvalidConfig",
                        DisabledReason::FreeSubscription => "FreeSubscription",
                    }.to_string()),
                    throttled_until: e
                        .throttled_until
                        .filter(|t| *t > now)
                        .map(|t| t.to_rfc3339()),
                    rpm_limit: e.credentials.rpm_limit,
                    rpm_current: e.rpm_window.iter().filter(|t| **t > rpm_cutoff).count() as u32,
                    overage: e.credentials.overage,
                })
                .collect(),
            current_id,
            total: entries.len(),
            available,
            default_rpm_limit: *self.default_rpm_limit.lock(),
        }
    }

    /// 设置凭据禁用状态（Admin API）
    pub fn set_disabled(&self, id: u64, disabled: bool) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.disabled = disabled;
            if !disabled {
                // 启用时重置失败计数
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.disabled_reason = None;
            } else {
                entry.disabled_reason = Some(DisabledReason::Manual);
            }
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据优先级（Admin API）
    ///
    /// 修改优先级后会立即按新优先级重新选择当前凭据。
    /// 即使持久化失败，内存中的优先级和当前凭据选择也会生效。
    pub fn set_priority(&self, id: u64, priority: u32) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.priority = priority;
        }
        // 立即按新优先级重新选择当前凭据（无论持久化是否成功）
        self.select_highest_priority();
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 批量设置凭据启用/禁用状态（Admin API）
    ///
    /// 合并 IO + 失败回滚；语义与单条 [`Self::set_disabled`] 一致：
    /// - 启用时清空失败计数与 disabled_reason
    /// - 禁用时设置 disabled_reason=Manual
    /// 已处于目标状态的凭据被视为成功（幂等）。
    pub fn set_disabled_batch(&self, ids: &[u64], disabled: bool) -> BatchSetGroupResult {
        let mut succeeded = Vec::new();
        let mut failed: Vec<BatchSetGroupFailure> = Vec::new();
        let mut previous: Vec<(u64, bool, u32, u32, Option<DisabledReason>)> = Vec::new();

        {
            let mut entries = self.entries.lock();
            for id in ids {
                match entries.iter_mut().find(|e| e.id == *id) {
                    Some(entry) => {
                        previous.push((
                            *id,
                            entry.disabled,
                            entry.failure_count,
                            entry.refresh_failure_count,
                            entry.disabled_reason,
                        ));
                        entry.disabled = disabled;
                        if disabled {
                            entry.disabled_reason = Some(DisabledReason::Manual);
                        } else {
                            entry.failure_count = 0;
                            entry.refresh_failure_count = 0;
                            entry.disabled_reason = None;
                        }
                        succeeded.push(*id);
                    }
                    None => failed.push(BatchSetGroupFailure {
                        id: *id,
                        error: format!("凭据不存在: {}", id),
                    }),
                }
            }
        }

        if !succeeded.is_empty() {
            if let Err(err) = self.persist_credentials() {
                let msg = err.to_string();
                let mut entries = self.entries.lock();
                for (id, prev_disabled, prev_fc, prev_rfc, prev_reason) in &previous {
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == *id) {
                        entry.disabled = *prev_disabled;
                        entry.failure_count = *prev_fc;
                        entry.refresh_failure_count = *prev_rfc;
                        entry.disabled_reason = *prev_reason;
                    }
                }
                for id in &succeeded {
                    failed.push(BatchSetGroupFailure {
                        id: *id,
                        error: format!("持久化失败: {}", msg),
                    });
                }
                return BatchSetGroupResult {
                    succeeded: vec![],
                    failed,
                };
            }
            // 禁用了当前活跃凭据时切换到下一个
            if disabled {
                let current_id = *self.current_id.lock();
                if succeeded.contains(&current_id) {
                    let _ = self.switch_to_next();
                }
            }
        }

        BatchSetGroupResult { succeeded, failed }
    }

    /// 批量设置凭据优先级（Admin API）
    ///
    /// 内存写入和持久化合并为单次磁盘 IO；持久化失败时回滚所有已修改条目，
    /// 整批转为失败。返回结构沿用 [`BatchSetGroupResult`] 的 succeeded/failed 形态。
    pub fn set_priority_batch(&self, ids: &[u64], priority: u32) -> BatchSetGroupResult {
        let mut succeeded = Vec::new();
        let mut failed: Vec<BatchSetGroupFailure> = Vec::new();
        let mut previous: Vec<(u64, u32)> = Vec::new();

        {
            let mut entries = self.entries.lock();
            for id in ids {
                match entries.iter_mut().find(|e| e.id == *id) {
                    Some(entry) => {
                        previous.push((*id, entry.credentials.priority));
                        entry.credentials.priority = priority;
                        succeeded.push(*id);
                    }
                    None => failed.push(BatchSetGroupFailure {
                        id: *id,
                        error: format!("凭据不存在: {}", id),
                    }),
                }
            }
        }

        if !succeeded.is_empty() {
            if let Err(err) = self.persist_credentials() {
                let msg = err.to_string();
                let mut entries = self.entries.lock();
                for (id, prev) in &previous {
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == *id) {
                        entry.credentials.priority = *prev;
                    }
                }
                for id in &succeeded {
                    failed.push(BatchSetGroupFailure {
                        id: *id,
                        error: format!("持久化失败: {}", msg),
                    });
                }
                return BatchSetGroupResult {
                    succeeded: vec![],
                    failed,
                };
            }
            // 持久化成功后按新优先级重新选择当前凭据
            self.select_highest_priority();
        }

        BatchSetGroupResult { succeeded, failed }
    }

    /// 设置凭据级 RPM 上限（Admin API）
    ///
    /// 传 None 表示清除凭据级覆盖，回退到全局默认；
    /// 传 Some(0) 表示显式不限流（即使全局有默认）。
    pub fn set_rpm_limit(&self, id: u64, rpm_limit: Option<u32>) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.rpm_limit = rpm_limit;
        }
        if rpm_limit.unwrap_or(0) > 0 {
            self.rpm_feature_enabled.store(true, Ordering::Relaxed);
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 批量设置凭据级 RPM 上限（Admin API）
    ///
    /// 行为与 [`Self::set_priority_batch`] 一致：合并 IO + 失败回滚。
    pub fn set_rpm_limit_batch(&self, ids: &[u64], rpm_limit: Option<u32>) -> BatchSetGroupResult {
        if rpm_limit.unwrap_or(0) > 0 {
            self.rpm_feature_enabled.store(true, Ordering::Relaxed);
        }
        let mut succeeded = Vec::new();
        let mut failed: Vec<BatchSetGroupFailure> = Vec::new();
        let mut previous: Vec<(u64, Option<u32>)> = Vec::new();

        {
            let mut entries = self.entries.lock();
            for id in ids {
                match entries.iter_mut().find(|e| e.id == *id) {
                    Some(entry) => {
                        previous.push((*id, entry.credentials.rpm_limit));
                        entry.credentials.rpm_limit = rpm_limit;
                        succeeded.push(*id);
                    }
                    None => failed.push(BatchSetGroupFailure {
                        id: *id,
                        error: format!("凭据不存在: {}", id),
                    }),
                }
            }
        }

        if !succeeded.is_empty() {
            if let Err(err) = self.persist_credentials() {
                let msg = err.to_string();
                let mut entries = self.entries.lock();
                for (id, prev) in &previous {
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == *id) {
                        entry.credentials.rpm_limit = *prev;
                    }
                }
                for id in &succeeded {
                    failed.push(BatchSetGroupFailure {
                        id: *id,
                        error: format!("持久化失败: {}", msg),
                    });
                }
                return BatchSetGroupResult {
                    succeeded: vec![],
                    failed,
                };
            }
        }

        BatchSetGroupResult { succeeded, failed }
    }

    /// 设置凭据所属代理分组（Admin API）
    ///
    /// 传 None 或空字符串表示清空分组绑定（回退到全局代理）
    pub fn set_group(&self, id: u64, group: Option<String>) -> anyhow::Result<()> {
        let normalized = normalize_group_name(group);

        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.group = normalized;
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 批量设置凭据所属代理分组（Admin API）
    ///
    /// 每个 id 单独处理：找不到的记为失败，其他正常修改；最后只 persist 一次。
    /// persist 本身失败时回滚所有内存改动。
    pub fn set_group_batch(
        &self,
        ids: &[u64],
        group: Option<String>,
    ) -> BatchSetGroupResult {
        let normalized = normalize_group_name(group);
        let mut succeeded = Vec::new();
        let mut failed: Vec<BatchSetGroupFailure> = Vec::new();
        let mut previous: Vec<(u64, Option<String>)> = Vec::new();

        {
            let mut entries = self.entries.lock();
            for id in ids {
                match entries.iter_mut().find(|e| e.id == *id) {
                    Some(entry) => {
                        previous.push((*id, entry.credentials.group.clone()));
                        entry.credentials.group = normalized.clone();
                        succeeded.push(*id);
                    }
                    None => failed.push(BatchSetGroupFailure {
                        id: *id,
                        error: format!("凭据不存在: {}", id),
                    }),
                }
            }
        }

        if !succeeded.is_empty() {
            if let Err(err) = self.persist_credentials() {
                let msg = err.to_string();
                // 回滚已修改的内存条目
                let mut entries = self.entries.lock();
                for (id, prev) in &previous {
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == *id) {
                        entry.credentials.group = prev.clone();
                    }
                }
                // 全部转为失败
                for id in &succeeded {
                    failed.push(BatchSetGroupFailure {
                        id: *id,
                        error: format!("持久化失败: {}", msg),
                    });
                }
                return BatchSetGroupResult {
                    succeeded: vec![],
                    failed,
                };
            }
        }

        BatchSetGroupResult { succeeded, failed }
    }

    /// 重置凭据失败计数并重新启用（Admin API）
    pub fn reset_and_enable(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            if entry.disabled_reason == Some(DisabledReason::InvalidConfig) {
                anyhow::bail!(
                    "凭据 #{} 因配置无效被禁用，请修正配置后重启服务",
                    id
                );
            }
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
            entry.disabled = false;
            entry.disabled_reason = None;
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 获取指定凭据的使用额度（Admin API）
    pub async fn get_usage_limits_for(&self, id: u64) -> anyhow::Result<UsageLimitsResponse> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据直接使用 kiro_api_key，无需刷新
        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            // 检查是否需要刷新 token
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);

            if needs_refresh {
                let refresh_lock = self.refresh_lock_for(id);
                let _guard = refresh_lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };

                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let effective_proxy = current_creds.effective_proxy(self.proxy.as_ref(), &self.proxy_groups.read());
                    let new_creds =
                        refresh_token(&current_creds, &self.config, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = new_creds.clone();
                        }
                    }
                    // 持久化失败只记录警告，不影响本次请求
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref(), &self.proxy_groups.read());
        let usage_limits = get_usage_limits(&credentials, &self.config, &token, effective_proxy.as_ref()).await?;

        // 更新订阅等级到凭据（仅在发生变化时持久化）
        if let Some(subscription_title) = usage_limits.subscription_title() {
            // 确认为 Free：title 非空且大写含 FREE（None/未知一律不算，避免误判）
            let is_free = subscription_title.to_uppercase().contains("FREE");
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    let old_title = entry.credentials.subscription_title.clone();
                    let mut changed = false;
                    if old_title.as_deref() != Some(subscription_title) {
                        entry.credentials.subscription_title =
                            Some(subscription_title.to_string());
                        tracing::info!(
                            "凭据 #{} 订阅等级已更新: {:?} -> {}",
                            id,
                            old_title,
                            subscription_title
                        );
                        changed = true;
                    }

                    // 严格模式：每次查询只要确认 Free 且当前启用就禁用（不看是否变化）；
                    // 手动重新启用的 Free 账号会在下次余额刷新时被再次禁用。
                    if is_free && !entry.disabled {
                        entry.disabled = true;
                        entry.disabled_reason = Some(DisabledReason::FreeSubscription);
                        tracing::info!("凭据 #{} 订阅为 Free，已自动禁用", id);
                        changed = true;
                    } else if !is_free
                        && entry.disabled
                        && entry.disabled_reason == Some(DisabledReason::FreeSubscription)
                    {
                        // 升级到非 Free：自愈解除之前因 Free 的自动禁用。
                        entry.disabled = false;
                        entry.disabled_reason = None;
                        entry.failure_count = 0;
                        tracing::info!("凭据 #{} 订阅已升级为非 Free，自动解除禁用", id);
                        changed = true;
                    }
                    changed
                } else {
                    false
                }
            };

            if changed {
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("订阅等级更新后持久化失败（不影响本次请求）: {}", e);
                }
            }
        }

        Ok(usage_limits)
    }

    /// 添加新凭据（Admin API）
    ///
    /// # 流程
    /// 1. 验证凭据基本字段（API Key: kiroApiKey 不为空; OAuth: refreshToken 不为空）
    /// 2. 基于 kiroApiKey 或 refreshToken 的 SHA-256 哈希检测重复
    /// 3. OAuth: 尝试刷新 Token 验证凭据有效性; API Key: 跳过
    /// 4. 分配新 ID（当前最大 ID + 1）
    /// 5. 添加到 entries 列表
    /// 6. 持久化到配置文件
    ///
    /// # 返回
    /// - `Ok(u64)` - 新凭据 ID
    /// - `Err(_)` - 验证失败或添加失败
    pub async fn add_credential(&self, new_cred: KiroCredentials) -> anyhow::Result<u64> {
        if new_cred.rpm_limit.unwrap_or(0) > 0 {
            self.rpm_feature_enabled.store(true, Ordering::Relaxed);
        }
        // 1. 基本验证
        if new_cred.is_api_key_credential() {
            let api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            if api_key.is_empty() {
                anyhow::bail!("kiroApiKey 为空");
            }
        } else {
            validate_refresh_token(&new_cred)?;
        }

        // 2. 基于哈希检测重复
        if new_cred.is_api_key_credential() {
            let new_api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 kiroApiKey"))?;
            let new_api_key_hash = sha256_hex(new_api_key);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .kiro_api_key
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_api_key_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（kiroApiKey 重复）");
            }
        } else {
            let new_refresh_token = new_cred
                .refresh_token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;
            let new_refresh_token_hash = sha256_hex(new_refresh_token);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .refresh_token
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_refresh_token_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（refreshToken 重复）");
            }
        }

        // 3. 验证凭据有效性（API Key 无需网络刷新）
        let mut validated_cred = if new_cred.is_api_key_credential() {
            new_cred.clone()
        } else {
            let effective_proxy = new_cred.effective_proxy(self.proxy.as_ref(), &self.proxy_groups.read());
            refresh_token(&new_cred, &self.config, effective_proxy.as_ref()).await?
        };

        // 4. 分配新 ID
        let new_id = {
            let entries = self.entries.lock();
            entries.iter().map(|e| e.id).max().unwrap_or(0) + 1
        };

        // 5. 设置 ID 并保留用户输入的元数据
        validated_cred.id = Some(new_id);
        validated_cred.priority = new_cred.priority;
        validated_cred.auth_method = new_cred.auth_method.map(|m| {
            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam") {
                "idc".to_string()
            } else {
                m
            }
        });
        validated_cred.client_id = new_cred.client_id;
        validated_cred.client_secret = new_cred.client_secret;
        validated_cred.region = new_cred.region;
        // auth_region 不从用户输入回拷：验活刷新可能已兜底纠正（见 refresh_idc_token），
        // validated_cred 始于 new_cred.clone()，未纠正时本就保留用户输入值
        validated_cred.api_region = new_cred.api_region;
        validated_cred.machine_id = new_cred.machine_id;
        validated_cred.email = new_cred.email;
        validated_cred.proxy_url = new_cred.proxy_url;
        validated_cred.proxy_username = new_cred.proxy_username;
        validated_cred.proxy_password = new_cred.proxy_password;
        validated_cred.group = new_cred.group;
        validated_cred.kiro_api_key = new_cred.kiro_api_key;

        {
            let mut entries = self.entries.lock();
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled: false,
                disabled_reason: None,
                success_count: 0,
                last_used_at: None,
                throttled_until: None,
                throttle_count: 0,
                rpm_window: VecDeque::new(),
            });
        }

        // 6. 持久化
        self.persist_credentials()?;

        tracing::info!("成功添加凭据 #{}", new_id);
        Ok(new_id)
    }

    /// 删除凭据（Admin API）
    ///
    /// # 前置条件
    /// - 凭据必须已禁用（disabled = true）
    ///
    /// # 行为
    /// 1. 验证凭据存在
    /// 2. 验证凭据已禁用
    /// 3. 从 entries 移除
    /// 4. 如果删除的是当前凭据，切换到优先级最高的可用凭据
    /// 5. 如果删除后没有凭据，将 current_id 重置为 0
    /// 6. 持久化到文件
    ///
    /// # 返回
    /// - `Ok(())` - 删除成功
    /// - `Err(_)` - 凭据不存在、未禁用或持久化失败
    pub fn delete_credential(&self, id: u64) -> anyhow::Result<()> {
        let was_current = {
            let mut entries = self.entries.lock();

            // 查找凭据
            let entry = entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            // 检查是否已禁用
            if !entry.disabled {
                anyhow::bail!("只能删除已禁用的凭据（请先禁用凭据 #{}）", id);
            }

            // 记录是否是当前凭据
            let current_id = *self.current_id.lock();
            let was_current = current_id == id;

            // 删除凭据
            entries.retain(|e| e.id != id);

            was_current
        };

        // 清理该凭据的刷新锁桶，避免锁表残留空条目
        self.refresh_locks.lock().remove(&id);

        // 如果删除的是当前凭据，切换到优先级最高的可用凭据
        if was_current {
            self.select_highest_priority();
        }

        // 如果删除后没有任何凭据，将 current_id 重置为 0（与初始化行为保持一致）
        {
            let entries = self.entries.lock();
            if entries.is_empty() {
                let mut current_id = self.current_id.lock();
                *current_id = 0;
                tracing::info!("所有凭据已删除，current_id 已重置为 0");
            }
        }

        // 持久化更改
        self.persist_credentials()?;

        // 立即回写统计数据，清除已删除凭据的残留条目
        self.save_stats();

        tracing::info!("已删除凭据 #{}", id);
        Ok(())
    }

    /// 强制刷新指定凭据的 Token（Admin API）
    ///
    /// 无条件调用上游 API 重新获取 access token，不检查是否过期。
    /// 适用于排查问题、Token 异常但未过期、主动更新凭据状态等场景。
    pub async fn force_refresh_token_for(&self, id: u64) -> anyhow::Result<()> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // 获取刷新锁防止并发刷新
        let refresh_lock = self.refresh_lock_for(id);
        let _guard = refresh_lock.lock().await;

        // 无条件调用 refresh_token
        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref(), &self.proxy_groups.read());
        let new_creds =
            refresh_token(&credentials, &self.config, effective_proxy.as_ref()).await?;

        // 更新 entries 中对应凭据
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials = new_creds;
                entry.refresh_failure_count = 0;
            }
        }

        // 持久化
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("强制刷新 Token 后持久化失败: {}", e);
        }

        tracing::info!("凭据 #{} Token 已强制刷新", id);
        Ok(())
    }

    /// 取指定凭据的有效 access_token（按需刷新），用于 Admin 侧需要上游鉴权的调用。
    ///
    /// API Key 凭据直接返回 kiroApiKey；OAuth 凭据在过期/临期时持刷新锁刷新并持久化。
    /// 逻辑与 [`Self::get_usage_limits_for`] 中的取 token 流程一致。
    async fn ensure_valid_token_for(&self, id: u64) -> anyhow::Result<String> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        if credentials.is_api_key_credential() {
            return credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"));
        }

        let needs_refresh = is_token_expired(&credentials) || is_token_expiring_soon(&credentials);
        if !needs_refresh {
            return credentials
                .access_token
                .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"));
        }

        let refresh_lock = self.refresh_lock_for(id);
        let _guard = refresh_lock.lock().await;
        let current_creds = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        // 双检：可能已被其它任务在持锁期间刷新
        if !(is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds)) {
            return current_creds
                .access_token
                .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"));
        }

        let effective_proxy =
            current_creds.effective_proxy(self.proxy.as_ref(), &self.proxy_groups.read());
        let new_creds =
            refresh_token(&current_creds, &self.config, effective_proxy.as_ref()).await?;
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials = new_creds.clone();
            }
        }
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
        }
        new_creds
            .access_token
            .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))
    }

    /// 切换指定凭据的 overage（超额计费）开关（Admin API）。
    ///
    /// 成功后把下发值记录到凭据的 `overage` 字段并持久化，作为前端展示/核对依据
    /// （上游无读接口）。失败不写入本地状态。
    pub async fn set_overage_for(&self, id: u64, enabled: bool) -> anyhow::Result<()> {
        let token = self.ensure_valid_token_for(id).await?;

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let effective_proxy =
            credentials.effective_proxy(self.proxy.as_ref(), &self.proxy_groups.read());
        set_overage(&credentials, &self.config, &token, enabled, effective_proxy.as_ref()).await?;

        // 下发成功后回写本地状态
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials.overage = Some(enabled);
            }
        }
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("overage 下发成功但持久化失败: {}", e);
        }
        tracing::info!(
            "凭据 #{} overage 已切换为 {}",
            id,
            if enabled { "ENABLED" } else { "DISABLED" }
        );
        Ok(())
    }

    /// 查询指定凭据上游可用的模型 id 列表（Admin API）。
    ///
    /// 按需取该凭据的有效 token，调用上游 `ListAvailableModels`（企业 IdC / 不同订阅
    /// 的账号可用模型可能不同，故按凭据各自查询，而非用全局列表）。
    pub async fn get_available_models_for(&self, id: u64) -> anyhow::Result<Vec<String>> {
        let token = self.ensure_valid_token_for(id).await?;

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let effective_proxy =
            credentials.effective_proxy(self.proxy.as_ref(), &self.proxy_groups.read());
        list_available_models(&credentials, &self.config, &token, effective_proxy.as_ref()).await
    }

    /// 获取负载均衡模式（Admin API）
    pub fn get_load_balancing_mode(&self) -> String {
        self.load_balancing_mode.lock().clone()
    }

    /// 获取全局默认 RPM 上限（Admin API）
    pub fn get_default_rpm_limit(&self) -> Option<u32> {
        *self.default_rpm_limit.lock()
    }

    /// 设置全局默认 RPM 上限（Admin API）；持久化到 config.json
    ///
    /// 传 None 表示清空；传 Some(0) 表示显式不限流。
    ///
    /// 锁全程持有以串行化并发写：读 previous → 写 value → persist →
    /// 失败时回滚，全在同一把锁内完成。Mutex 守的是 Option<u32>，
    /// 持锁期间的磁盘 IO 阻塞其它 set/get 是预期行为（admin 操作，低频）。
    pub fn set_default_rpm_limit(&self, value: Option<u32>) -> anyhow::Result<()> {
        let mut guard = self.default_rpm_limit.lock();
        let previous = *guard;
        *guard = value;

        if value.unwrap_or(0) > 0 {
            self.rpm_feature_enabled.store(true, Ordering::Relaxed);
        }

        if let Err(e) = self.persist_default_rpm_limit(value) {
            *guard = previous;
            return Err(e);
        }
        Ok(())
    }

    fn persist_default_rpm_limit(&self, value: Option<u32>) -> anyhow::Result<()> {
        use anyhow::Context;

        let config_path = match self.config.config_path() {
            Some(path) => path.to_path_buf(),
            None => {
                tracing::warn!(
                    "配置文件路径未知，全局默认 RPM 仅在当前进程生效: {:?}",
                    value
                );
                return Ok(());
            }
        };

        let mut config = Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.default_rpm_limit = value;
        config
            .save()
            .with_context(|| format!("持久化全局默认 RPM 失败: {}", config_path.display()))?;

        Ok(())
    }

    fn persist_load_balancing_mode(&self, mode: &str) -> anyhow::Result<()> {
        use anyhow::Context;

        let config_path = match self.config.config_path() {
            Some(path) => path.to_path_buf(),
            None => {
                tracing::warn!("配置文件路径未知，负载均衡模式仅在当前进程生效: {}", mode);
                return Ok(());
            }
        };

        let mut config = Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.load_balancing_mode = mode.to_string();
        config
            .save()
            .with_context(|| format!("持久化负载均衡模式失败: {}", config_path.display()))?;

        Ok(())
    }

    /// 设置负载均衡模式（Admin API）
    pub fn set_load_balancing_mode(&self, mode: String) -> anyhow::Result<()> {
        // 验证模式值
        if mode != "priority" && mode != "balanced" {
            anyhow::bail!("无效的负载均衡模式: {}", mode);
        }

        let previous_mode = self.get_load_balancing_mode();
        if previous_mode == mode {
            return Ok(());
        }

        *self.load_balancing_mode.lock() = mode.clone();

        if let Err(err) = self.persist_load_balancing_mode(&mode) {
            *self.load_balancing_mode.lock() = previous_mode;
            return Err(err);
        }

        tracing::info!("负载均衡模式已设置为: {}", mode);
        Ok(())
    }
}

impl Drop for MultiTokenManager {
    fn drop(&mut self) {
        if self.stats_dirty.load(Ordering::Relaxed) {
            self.save_stats();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_token_expired_with_expired_token() {
        let mut credentials = KiroCredentials::default();
        credentials.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_with_valid_token() {
        let mut credentials = KiroCredentials::default();
        let future = Utc::now() + Duration::hours(1);
        credentials.expires_at = Some(future.to_rfc3339());
        assert!(!is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_within_5_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(3);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_no_expires_at() {
        let credentials = KiroCredentials::default();
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_within_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(8);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_beyond_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(15);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(!is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_validate_refresh_token_missing() {
        let credentials = KiroCredentials::default();
        let result = validate_refresh_token(&credentials);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_refresh_token_valid() {
        let mut credentials = KiroCredentials::default();
        credentials.refresh_token = Some("a".repeat(150));
        let result = validate_refresh_token(&credentials);
        assert!(result.is_ok());
    }

    /// 凭据 region 填成 API region（eu-central-1）时，候选列表应兜底 us-east-1
    #[test]
    fn test_idc_refresh_region_candidates_fallback_us_east_1() {
        let config = Config::default();
        let mut cred = KiroCredentials::default();
        cred.region = Some("eu-central-1".to_string());
        cred.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:111122223333:profile/X".to_string());

        assert_eq!(
            idc_refresh_region_candidates(&cred, &config),
            vec!["eu-central-1".to_string(), "us-east-1".to_string()]
        );
    }

    /// auth region 已是 us-east-1 时，profileArn 内嵌 region 作为额外候选
    #[test]
    fn test_idc_refresh_region_candidates_profile_arn_region() {
        let config = Config::default();
        let mut cred = KiroCredentials::default();
        cred.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:111122223333:profile/X".to_string());

        // 生效 auth region 回退到 config 默认 us-east-1
        assert_eq!(
            idc_refresh_region_candidates(&cred, &config),
            vec!["us-east-1".to_string(), "eu-central-1".to_string()]
        );
    }

    /// 各来源 region 一致时去重，只保留一个候选
    #[test]
    fn test_idc_refresh_region_candidates_dedup() {
        let config = Config::default();
        let mut cred = KiroCredentials::default();
        cred.auth_region = Some("us-east-1".to_string());
        cred.profile_arn =
            Some("arn:aws:codewhisperer:us-east-1:111122223333:profile/X".to_string());

        assert_eq!(
            idc_refresh_region_candidates(&cred, &config),
            vec!["us-east-1".to_string()]
        );
    }

    /// 企业 IdC profile 在 eu-central-1、认证在 us-east-1：候选应同时含两地，先 us-east-1
    #[test]
    fn test_profile_probe_candidates_enterprise_cross_region() {
        let config = Config::default();
        let mut cred = KiroCredentials::default();
        cred.region = Some("us-east-1".to_string()); // SSO 认证 region
        cred.profile_arn = None; // 尚未探测

        // effective_api_region 在 profileArn 为空时回退 config.region=us-east-1
        assert_eq!(
            profile_probe_region_candidates(&cred, &config),
            vec!["us-east-1".to_string(), "eu-central-1".to_string()]
        );
    }

    /// 已知 profileArn 在 eu-central-1：eu 优先，再补 us-east-1
    #[test]
    fn test_profile_probe_candidates_known_eu_profile() {
        let config = Config::default();
        let mut cred = KiroCredentials::default();
        cred.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:111122223333:profile/X".to_string());

        assert_eq!(
            profile_probe_region_candidates(&cred, &config),
            vec!["eu-central-1".to_string(), "us-east-1".to_string()]
        );
    }

    /// 生效 region 已是受支持 region 时不重复
    #[test]
    fn test_profile_probe_candidates_dedup() {
        let config = Config::default();
        let cred = KiroCredentials::default(); // 回退 config.region=us-east-1

        assert_eq!(
            profile_probe_region_candidates(&cred, &config),
            vec!["us-east-1".to_string(), "eu-central-1".to_string()]
        );
    }

    #[test]
    fn test_sha256_hex() {
        let result = sha256_hex("test");
        assert_eq!(
            result,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_refresh_token() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.refresh_token = Some("a".repeat(150));

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("凭据已存在"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_success() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_test_key_123".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        let id = result.unwrap();
        assert!(id > 0);
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    /// 回归测试：即使 is_multiple_format=false（原文件是单对象格式），
    /// add_credential 也应回写为数组格式（自动升级），防止「重启后 Admin
    /// 新增凭据丢失 + 单对象格式停留在占位符」的历史 bug 回潮。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_single_format_upgrades_to_array_on_persist() {
        let tmp_dir = std::env::temp_dir().join(format!("kiro_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let creds_path = tmp_dir.join("credentials.json");

        // 模拟旧版 install.sh 生成的单对象占位文件
        std::fs::write(
            &creds_path,
            r#"{"kiroApiKey":"ksk_placeholder","authMethod":"api_key"}"#,
        )
        .unwrap();

        let mut placeholder = KiroCredentials::default();
        placeholder.kiro_api_key = Some("ksk_placeholder".to_string());
        placeholder.auth_method = Some("api_key".to_string());

        // is_multiple_format=false 模拟原文件是单对象格式
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![placeholder],
            None,
            Some(creds_path.clone()),
            false,
        )
        .unwrap();

        let mut new_cred = KiroCredentials::default();
        new_cred.kiro_api_key = Some("ksk_real_new".to_string());
        new_cred.auth_method = Some("api_key".to_string());
        manager.add_credential(new_cred).await.unwrap();

        let content = std::fs::read_to_string(&creds_path).unwrap();
        assert!(
            content.trim_start().starts_with('['),
            "回写后应为数组格式（自动升级），实际首字符: {:?}",
            content.chars().next()
        );
        assert!(
            content.contains("ksk_real_new"),
            "Admin 新增的凭据应被持久化到文件"
        );

        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    /// 回归测试：单凭据 + is_multiple_format=false → 持久化保持单对象格式，
    /// 不强制升级到数组。只有当凭据数 > 1 时才自动升级。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_single_credential_preserves_single_object_format() {
        let tmp_dir = std::env::temp_dir()
            .join(format!("kiro_test_preserve_{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let creds_path = tmp_dir.join("credentials.json");

        std::fs::write(
            &creds_path,
            r#"{"kiroApiKey":"ksk_initial","authMethod":"api_key"}"#,
        )
        .unwrap();

        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.kiro_api_key = Some("ksk_initial".to_string());
        cred.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(creds_path.clone()),
            false,
        )
        .unwrap();

        manager.set_priority(1, 5).unwrap();

        let content = std::fs::read_to_string(&creds_path).unwrap();
        assert!(
            content.trim_start().starts_with('{'),
            "单凭据 + is_multiple_format=false 时应保持单对象格式，实际: {}",
            content
        );
        assert!(content.contains("\"priority\": 5"));

        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_api_key() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing_key".to_string());
        existing.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.kiro_api_key = Some("ksk_existing_key".to_string());
        duplicate.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("kiroApiKey 重复"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_empty_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some(String::new());
        cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("kiroApiKey 为空"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_missing_key_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        // kiro_api_key is None

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("缺少 kiroApiKey"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_and_oauth_coexist() {
        let config = Config::default();

        let mut oauth_cred = KiroCredentials::default();
        oauth_cred.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![oauth_cred], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_new_key".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    // MultiTokenManager 测试

    #[test]
    fn test_multi_token_manager_new() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.priority = 0;
        let mut cred2 = KiroCredentials::default();
        cred2.priority = 1;

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    #[test]
    fn test_multi_token_manager_empty_credentials() {
        let config = Config::default();
        let result = MultiTokenManager::new(config, vec![], None, None, false);
        // 支持 0 个凭据启动（可通过管理面板添加）
        assert!(result.is_ok());
        let manager = result.unwrap();
        assert_eq!(manager.total_count(), 0);
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_duplicate_ids() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.id = Some(1);
        let mut cred2 = KiroCredentials::default();
        cred2.id = Some(1); // 重复 ID

        let result = MultiTokenManager::new(config, vec![cred1, cred2], None, None, false);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("重复的凭据 ID"),
            "错误消息应包含 '重复的凭据 ID'，实际: {}",
            err_msg
        );
    }

    #[test]
    fn test_multi_token_manager_api_key_missing_kiro_api_key_auto_disabled() {
        let config = Config::default();

        // auth_method=api_key 但缺少 kiro_api_key → 应被自动禁用
        let mut bad_cred = KiroCredentials::default();
        bad_cred.auth_method = Some("api_key".to_string());
        // kiro_api_key 保持 None

        let mut good_cred = KiroCredentials::default();
        good_cred.refresh_token = Some("valid_token".to_string());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 1); // bad_cred 被禁用，只剩 1 个可用
    }

    #[test]
    fn test_multi_token_manager_api_key_with_kiro_api_key_not_disabled() {
        let config = Config::default();

        // auth_method=api_key 且有 kiro_api_key → 不应被禁用
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("ksk_test123".to_string());

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_report_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        // 前两次失败不会禁用（使用 ID 1）
        assert!(manager.report_failure(1));
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 2);

        // 第三次失败会禁用第一个凭据
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 1);

        // 继续失败第二个凭据（使用 ID 2）
        assert!(manager.report_failure(2));
        assert!(manager.report_failure(2));
        assert!(!manager.report_failure(2)); // 所有凭据都禁用了
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_report_success() {
        let config = Config::default();
        let cred = KiroCredentials::default();

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // 失败两次（使用 ID 1）
        manager.report_failure(1);
        manager.report_failure(1);

        // 成功后重置计数（使用 ID 1）
        manager.report_success(1);

        // 再失败两次不会禁用
        manager.report_failure(1);
        manager.report_failure(1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_mark_accessed_updates_last_used_at_without_success_count() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 先失败一次，记一个基线 failure_count
        manager.report_failure(1);

        // mark_accessed：只动 last_used_at，不重置 failure_count 也不增 success_count
        manager.mark_accessed(1);

        let snap = manager.snapshot();
        let e1 = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(e1.last_used_at.is_some(), "last_used_at 应被刷新");
        assert_eq!(e1.success_count, 0, "mark_accessed 不应增加 success_count");
        assert_eq!(e1.failure_count, 1, "mark_accessed 不应重置 failure_count");

        // 未被标记的凭据 2 仍然是 None，balanced (LRU) 应优先选它
        let e2 = snap.entries.iter().find(|e| e.id == 2).unwrap();
        assert!(e2.last_used_at.is_none());
    }

    #[test]
    fn test_multi_token_manager_switch_to_next() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.refresh_token = Some("token1".to_string());
        let mut cred2 = KiroCredentials::default();
        cred2.refresh_token = Some("token2".to_string());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        let initial_id = manager.snapshot().current_id;

        // 切换到下一个
        assert!(manager.switch_to_next());
        assert_ne!(manager.snapshot().current_id, initial_id);
    }

    #[test]
    fn test_set_load_balancing_mode_persists_to_config_file() {
        let config_path = std::env::temp_dir().join(format!(
            "kiro-load-balancing-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&config_path, r#"{"loadBalancingMode":"priority"}"#).unwrap();

        let config = Config::load(&config_path).unwrap();
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        manager
            .set_load_balancing_mode("balanced".to_string())
            .unwrap();

        let persisted = Config::load(&config_path).unwrap();
        assert_eq!(persisted.load_balancing_mode, "balanced");
        assert_eq!(manager.get_load_balancing_mode(), "balanced");

        std::fs::remove_file(&config_path).unwrap();
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_auto_recovers_all_disabled() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(2);
        }

        assert_eq!(manager.available_count(), 0);

        // 应触发自愈：重置失败计数并重新启用，避免必须重启进程
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert!(ctx.token == "t1" || ctx.token == "t2");
        assert_eq!(manager.available_count(), 2);
    }

    #[test]
    fn test_balanced_select_filters_by_priority_tier_then_lru() {
        // balanced 模式下应仅在最高优先级一档内做 LRU；低优先级凭据
        // 即使更"久未使用"也不应被选中。
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut high_a = KiroCredentials::default();
        high_a.priority = 0;
        high_a.refresh_token = Some("ha".to_string());
        let mut high_b = KiroCredentials::default();
        high_b.priority = 0;
        high_b.refresh_token = Some("hb".to_string());
        let mut low = KiroCredentials::default();
        low.priority = 5;
        low.refresh_token = Some("low".to_string());

        let manager =
            MultiTokenManager::new(config, vec![high_a, high_b, low], None, None, false).unwrap();

        // 给低优先级凭据一个非常早的 last_used_at（"看起来最久未用"），
        // 验证它不会因此抢占高优先级档。
        manager.mark_accessed(3);
        // 立刻覆写为很早的时间戳，绕过自然时间差
        {
            let mut entries = manager.entries.lock();
            let low_entry = entries.iter_mut().find(|e| e.id == 3).unwrap();
            low_entry.last_used_at = Some("2000-01-01T00:00:00Z".to_string());
        }

        // 高优先级档两个凭据都从未使用过，按 LRU 应选 id=1（None 排最前，
        // 同样为 None 时按出现顺序选第一个）
        let (id, _) = manager.select_next_credential(None).expect("应选出一个凭据");
        assert!(id == 1 || id == 2, "必须从高优先级档（priority=0）内选取，得到 {}", id);
        assert_ne!(id, 3, "低优先级凭据不应抢占高优先级档");

        // 再选一次，high_a 已经被打上 last_used_at，high_b 仍是 None，应轮到 high_b
        let first = id;
        let (next_id, _) = manager.select_next_credential(None).expect("应选出第二个");
        assert_ne!(next_id, 3, "低优先级凭据仍不应被选");
        assert_ne!(next_id, first, "同档内 LRU 应轮到另一个");
    }

    #[test]
    fn test_report_throttled_excludes_credential_until_cooldown_expires() {
        // 单档内多个凭据：被 throttle 的凭据冷却期内不再被选中，
        // 同档其他凭据继续承担流量。
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut a = KiroCredentials::default();
        a.priority = 0;
        a.refresh_token = Some("a".to_string());
        let mut b = KiroCredentials::default();
        b.priority = 0;
        b.refresh_token = Some("b".to_string());

        let manager = MultiTokenManager::new(config, vec![a, b], None, None, false).unwrap();

        // 限流凭据 1
        manager.report_throttled(1);
        let snap = manager.snapshot();
        let entry1 = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(entry1.success_count == 0, "throttle 不应增加 success_count");

        // 连续多次选择都应落到凭据 2
        for _ in 0..5 {
            let (id, _) = manager.select_next_credential(None).expect("凭据 2 应可用");
            assert_eq!(id, 2, "凭据 1 处于冷却期，应只选凭据 2");
        }

        // 一次成功后应清零冷却
        manager.report_success(1);
        // 现在两者都可用：last_used_at 让 LRU 又能选到凭据 1
        let mut got_one = false;
        for _ in 0..4 {
            let (id, _) = manager.select_next_credential(None).unwrap();
            if id == 1 {
                got_one = true;
                break;
            }
        }
        assert!(got_one, "report_success 后凭据 1 应重新参与轮转");
    }

    #[test]
    fn test_select_falls_back_to_throttled_when_all_available_throttled() {
        // 当所有非 disabled 凭据都处于 429 冷却期时，select_next_credential
        // 不应返回 None；而应回退到 LRU 选最早被限流者（last_used_at 最早），
        // 让上层不会误报"所有凭据均已禁用"。
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut a = KiroCredentials::default();
        a.priority = 0;
        a.refresh_token = Some("a".to_string());
        let mut b = KiroCredentials::default();
        b.priority = 0;
        b.refresh_token = Some("b".to_string());

        let manager = MultiTokenManager::new(config, vec![a, b], None, None, false).unwrap();

        // 先限流 1（更早），再限流 2
        manager.report_throttled(1);
        std::thread::sleep(StdDuration::from_millis(20));
        manager.report_throttled(2);

        // 全员冷却中，select 不应 None；按 last_used_at 最早应选凭据 1
        let (id, _) = manager
            .select_next_credential(None)
            .expect("全员冷却时应回退选择，而不是返回 None");
        assert_eq!(id, 1, "应选到最早被限流的凭据 1（其 last_used_at 最早）");
    }

    #[test]
    fn test_report_throttled_backoff_schedule() {
        // 连续多次 429 的冷却时长按 THROTTLE_BACKOFF_SECS 表 [10, 20, 30, 60] 取值，
        // 超过表长后封顶为 60s。
        let config = Config::default();
        let cred = KiroCredentials::default();
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        let read_remaining = || -> i64 {
            let entries = manager.entries.lock();
            let until = entries.iter().find(|e| e.id == 1).unwrap().throttled_until.unwrap();
            (until - Utc::now()).num_seconds()
        };

        let expected: &[i64] = &[10, 20, 30, 60];
        for (i, want) in expected.iter().enumerate() {
            manager.report_throttled(1);
            let got = read_remaining();
            assert!(
                (got - want).abs() <= 2,
                "第 {} 次 cooldown 期望 ~{}s，实际 {}s",
                i + 1,
                want,
                got
            );
        }

        // 继续追加几次，应稳定封顶在 60s
        for _ in 0..3 {
            manager.report_throttled(1);
            let got = read_remaining();
            assert!(
                (got - 60).abs() <= 2,
                "封顶后 cooldown 期望 ~60s，实际 {}s",
                got
            );
        }
    }

    #[test]
    fn test_balanced_select_falls_back_when_high_priority_tier_disabled() {
        // 高优先级档全部 disabled 时，balanced 模式应自然降级到下一档。
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut high = KiroCredentials::default();
        high.priority = 0;
        high.refresh_token = Some("h".to_string());
        let mut low = KiroCredentials::default();
        low.priority = 5;
        low.refresh_token = Some("l".to_string());

        let manager =
            MultiTokenManager::new(config, vec![high, low], None, None, false).unwrap();

        // 禁用唯一的高优先级凭据
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }

        let (id, _) = manager.select_next_credential(None).expect("应降级到低优先级档");
        assert_eq!(id, 2, "高优先级全部 disabled 后应降级到低优先级凭据");
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_balanced_retries_until_bad_credential_disabled() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut bad_cred = KiroCredentials::default();
        bad_cred.priority = 0;
        bad_cred.refresh_token = Some("bad".to_string());

        let mut good_cred = KiroCredentials::default();
        good_cred.priority = 1;
        good_cred.access_token = Some("good-token".to_string());
        good_cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();

        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "good-token");
    }

    #[test]
    fn test_multi_token_manager_report_refresh_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert_eq!(manager.available_count(), 2);
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL - 1) {
            assert!(manager.report_refresh_failure(1));
        }
        assert_eq!(manager.available_count(), 2);

        assert!(manager.report_refresh_failure(1));
        assert_eq!(manager.available_count(), 1);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(first.disabled);
        assert_eq!(first.refresh_failure_count, MAX_FAILURES_PER_CREDENTIAL);
        assert_eq!(snapshot.current_id, 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_refresh_failure_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_refresh_failure(1);
            manager.report_refresh_failure(2);
        }
        assert_eq!(manager.available_count(), 0);

        let err = manager.acquire_context(None, None).await.err().unwrap().to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
    }

    #[test]
    fn test_multi_token_manager_report_quota_exhausted() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        assert_eq!(manager.available_count(), 2);
        assert!(manager.report_quota_exhausted(1));
        assert_eq!(manager.available_count(), 1);

        // 再禁用第二个后，无可用凭据
        assert!(!manager.report_quota_exhausted(2));
        assert_eq!(manager.available_count(), 0);
    }

    #[tokio::test]
    async fn test_multi_token_manager_quota_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        manager.report_quota_exhausted(1);
        manager.report_quota_exhausted(2);
        assert_eq!(manager.available_count(), 0);

        let err = manager.acquire_context(None, None).await.err().unwrap().to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
        assert_eq!(manager.available_count(), 0);
    }

    // ============ 凭据级 Region 优先级测试 ============

    #[test]
    fn test_credential_region_priority_uses_credential_auth_region() {
        // 凭据配置了 auth_region 时，应使用凭据的 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-west-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-west-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_credential_region() {
        // 凭据未配置 auth_region 但配置了 region 时，应回退到凭据.region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-central-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_config() {
        // 凭据未配置 auth_region 和 region 时，应回退到 config
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials::default();
        assert!(credentials.auth_region.is_none());
        assert!(credentials.region.is_none());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "us-west-2");
    }

    #[test]
    fn test_multiple_credentials_use_respective_regions() {
        // 多凭据场景下，不同凭据使用各自的 auth_region
        let mut config = Config::default();
        config.region = "ap-northeast-1".to_string();

        let mut cred1 = KiroCredentials::default();
        cred1.auth_region = Some("us-east-1".to_string());

        let mut cred2 = KiroCredentials::default();
        cred2.region = Some("eu-west-1".to_string());

        let cred3 = KiroCredentials::default(); // 无 region，使用 config

        assert_eq!(cred1.effective_auth_region(&config), "us-east-1");
        assert_eq!(cred2.effective_auth_region(&config), "eu-west-1");
        assert_eq!(cred3.effective_auth_region(&config), "ap-northeast-1");
    }

    #[test]
    fn test_idc_oidc_endpoint_uses_credential_auth_region() {
        // 验证 IdC OIDC endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);

        assert_eq!(refresh_url, "https://oidc.eu-central-1.amazonaws.com/token");
    }

    #[test]
    fn test_social_refresh_endpoint_uses_credential_auth_region() {
        // 验证 Social refresh endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("ap-southeast-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);

        assert_eq!(
            refresh_url,
            "https://prod.ap-southeast-1.auth.desktop.kiro.dev/refreshToken"
        );
    }

    #[test]
    fn test_api_call_uses_effective_api_region() {
        // 验证 API 调用使用 effective_api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-west-1".to_string());

        // 凭据.region 不参与 api_region 回退链
        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.us-west-2.amazonaws.com");
    }

    #[test]
    fn test_api_call_uses_credential_api_region() {
        // 凭据配置了 api_region 时，API 调用应使用凭据的 api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.api_region = Some("eu-central-1".to_string());

        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.eu-central-1.amazonaws.com");
    }

    #[test]
    fn test_credential_region_empty_string_treated_as_set() {
        // 空字符串 auth_region 被视为已设置（虽然不推荐，但行为应一致）
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("".to_string());

        let region = credentials.effective_auth_region(&config);
        // 空字符串被视为已设置，不会回退到 config
        assert_eq!(region, "");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("auth-only".to_string());
        credentials.api_region = Some("api-only".to_string());

        assert_eq!(credentials.effective_auth_region(&config), "auth-only");
        assert_eq!(credentials.effective_api_region(&config), "api-only");
    }
}
