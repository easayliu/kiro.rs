//! Kiro OAuth 凭证数据模型
//!
//! 支持从 Kiro IDE 的凭证文件加载，使用 Social 认证方式
//! 支持单凭据和多凭据配置格式

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::http_client::ProxyConfig;
use crate::model::config::{ClientMode, Config, ProxyGroupConfig};
use std::collections::BTreeMap;

/// Kiro OAuth 凭证
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KiroCredentials {
    /// 凭据唯一标识符（自增 ID）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,

    /// 访问令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,

    /// 刷新令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,

    /// Profile ARN
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,

    /// 过期时间 (RFC3339 格式)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,

    /// 认证方式 (social / idc)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,

    /// OIDC Client ID (IdC 认证需要)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// OIDC Client Secret (IdC 认证需要)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,

    /// 凭据优先级（数字越小优先级越高，默认为 0）
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: u32,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    /// 凭据级 Machine ID 配置（可选）
    /// 未配置时回退到 config.json 的 machineId；都未配置时由 refreshToken 派生
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,

    /// 用户邮箱（从 Anthropic API 获取）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// 订阅等级（KIRO PRO+ / KIRO FREE 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub subscription_title: Option<String>,

    /// 凭据级代理 URL（可选）
    /// 支持 http/https/socks5 协议
    /// 特殊值 "direct" 表示显式不使用代理（即使全局/分组配置了代理）
    /// 未配置时按 group → 全局 顺序回退
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,

    /// 凭据所属代理分组名称（可选）
    /// 凭据未单独配置 proxyUrl 时，回退到 config.json 中 `proxyGroups[group]` 的配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,

    /// 凭据级客户端模拟模式（可选）
    /// 未配置时回退到 config.json 的 clientMode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_mode: Option<ClientMode>,

    /// 凭据是否被禁用（默认为 false）
    #[serde(default)]
    pub disabled: bool,

    /// Kiro API Key（headless 模式）
    /// 格式: ksk_xxxxxxxx
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 凭据级 RPM 上限（每分钟请求数）
    /// 未配置时回退到 config.json 的 `defaultRpmLimit`；都未配置则不限流。
    /// 设置为 0 表示该凭据不限流，即使全局有默认值也强制不限。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,
}

/// 判断是否为零（用于跳过序列化）
fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Social 登录（Google/Github）账号的共享 profileArn。
/// Kiro IDE 对 social 免费账号统一使用这个 profile（抓包确认）。
pub const SOCIAL_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";

/// AWS Builder ID 账号的共享 profileArn。
/// Kiro IDE 对 Builder ID 账号统一使用这个 profile（抓包确认）。
/// 也用于 `auth_method=idc/builder-id` 凭据 `ListAvailableProfiles` 探测失败时的兜底。
pub const BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";

/// AWS Builder ID 的固定 SSO 登录目录 host。
/// 企业 IdC 的登录目录是 `d-*.awsapps.com`（或自定义域名），据此与 Builder ID 区分。
const BUILDER_ID_START_HOST: &str = "view.awsapps.com";

fn canonicalize_auth_method_value(value: &str) -> &str {
    if value.eq_ignore_ascii_case("builder-id") || value.eq_ignore_ascii_case("iam") {
        "idc"
    } else if value.eq_ignore_ascii_case("api_key") || value.eq_ignore_ascii_case("apikey") {
        "api_key"
    } else {
        value
    }
}

/// 凭据配置（支持单对象或数组格式）
///
/// 自动识别配置文件格式：
/// - 单对象格式（旧格式，向后兼容）
/// - 数组格式（新格式，支持多凭据）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CredentialsConfig {
    /// 单个凭据（旧格式）
    Single(KiroCredentials),
    /// 多凭据数组（新格式）
    Multiple(Vec<KiroCredentials>),
}

impl CredentialsConfig {
    /// 从文件加载凭据配置
    ///
    /// - 如果文件不存在，返回空数组
    /// - 如果文件内容为空，返回空数组
    /// - 支持单对象或数组格式
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();

        // 文件不存在时返回空数组
        if !path.exists() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let content = fs::read_to_string(path)?;

        // 文件为空时返回空数组
        if content.trim().is_empty() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// 转换为按优先级排序的凭据列表
    pub fn into_sorted_credentials(self) -> Vec<KiroCredentials> {
        match self {
            CredentialsConfig::Single(mut cred) => {
                cred.canonicalize_auth_method();
                vec![cred]
            }
            CredentialsConfig::Multiple(mut creds) => {
                // 按优先级排序（数字越小优先级越高）
                creds.sort_by_key(|c| c.priority);
                for cred in &mut creds {
                    cred.canonicalize_auth_method();
                }
                creds
            }
        }
    }

    /// 判断是否为多凭据格式（数组格式）
    pub fn is_multiple(&self) -> bool {
        matches!(self, CredentialsConfig::Multiple(_))
    }
}

impl KiroCredentials {
    /// 特殊值：显式不使用代理
    pub const PROXY_DIRECT: &'static str = "direct";

    /// 获取默认凭证文件路径
    pub fn default_credentials_path() -> &'static str {
        "credentials.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    pub fn effective_auth_region<'a>(&'a self, config: &'a Config) -> &'a str {
        self.auth_region
            .as_deref()
            .or(self.region.as_deref())
            .unwrap_or(config.effective_auth_region())
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先级：凭据.api_region（显式覆盖）> profileArn 内嵌 region > config.api_region > config.region
    ///
    /// 企业 IdC 的 Q API region 以账号自有 profileArn 内嵌的为准（探测写回后生效），未必等于
    /// SSO 认证 region，也未必等于 config 默认 region。
    pub fn effective_api_region<'a>(&'a self, config: &'a Config) -> &'a str {
        self.api_region
            .as_deref()
            .or_else(|| self.profile_arn_region())
            .unwrap_or(config.effective_api_region())
    }

    /// 获取有效的客户端模拟模式
    /// 优先级：凭据.client_mode > config.client_mode
    pub fn effective_client_mode(&self, config: &Config) -> ClientMode {
        self.client_mode.unwrap_or(config.client_mode)
    }

    /// 获取有效的代理配置
    /// 优先级：凭据自身代理 > 凭据所属分组代理 > 全局代理 > 无代理
    /// 特殊值 "direct" 在任何一层都会短路为不使用代理
    pub fn effective_proxy(
        &self,
        global_proxy: Option<&ProxyConfig>,
        groups: &BTreeMap<String, ProxyGroupConfig>,
    ) -> Option<ProxyConfig> {
        // 1. 凭据自身代理（最高优先级）
        if let Some(url) = self.proxy_url.as_deref() {
            if url.eq_ignore_ascii_case(Self::PROXY_DIRECT) {
                return None;
            }
            let mut proxy = ProxyConfig::new(url);
            if let (Some(username), Some(password)) =
                (&self.proxy_username, &self.proxy_password)
            {
                proxy = proxy.with_auth(username, password);
            }
            return Some(proxy);
        }

        // 2. 凭据所属分组代理
        if let Some(group_name) = self.group.as_deref() {
            if let Some(group) = groups.get(group_name) {
                if group.proxy_url.eq_ignore_ascii_case(Self::PROXY_DIRECT) {
                    return None;
                }
                let mut proxy = ProxyConfig::new(&group.proxy_url);
                if let (Some(username), Some(password)) =
                    (&group.proxy_username, &group.proxy_password)
                {
                    proxy = proxy.with_auth(username, password);
                }
                return Some(proxy);
            }
            // 命名了分组但分组不存在：记录警告，回退到全局
            tracing::warn!(
                "凭据 #{} 引用了未定义的代理分组 '{}'，回退到全局代理",
                self.id.unwrap_or(0),
                group_name
            );
        }

        // 3. 全局代理
        global_proxy.cloned()
    }

    pub fn canonicalize_auth_method(&mut self) {
        let auth_method = match &self.auth_method {
            Some(m) => m,
            None => return,
        };

        let canonical = canonicalize_auth_method_value(auth_method);
        if canonical != auth_method {
            self.auth_method = Some(canonical.to_string());
        }
    }

    /// 检查凭据是否支持 Opus 模型
    ///
    /// Free 账号不支持 Opus 模型，需要 PRO 或更高等级订阅
    pub fn supports_opus(&self) -> bool {
        match &self.subscription_title {
            Some(title) => {
                let title_upper = title.to_uppercase();
                // 如果包含 FREE，则不支持 Opus
                !title_upper.contains("FREE")
            }
            // 如果还没有获取订阅信息，暂时允许（首次使用时会获取）
            None => true,
        }
    }

    /// 从 client_secret(JWT) 中解出 SSO 登录目录的 host。
    ///
    /// IdC 凭据的 client_secret 是一段 JWT：payload 里有 `serialized` 字段（再次转义的 JSON 字符串），
    /// 其中 `initiateLoginUri` 指向该账号的 SSO 起始页：
    /// - AWS Builder ID：`https://view.awsapps.com/start`
    /// - 企业 IdC：`https://d-XXXXXXXXXX.awsapps.com/start`（企业 SSO 目录）或自定义域名
    ///
    /// 仅读取 payload、不验签（只为区分账号类型，不用于鉴权）。
    fn sso_login_host(&self) -> Option<String> {
        use base64::Engine;

        let secret = self.client_secret.as_deref()?;
        let payload_b64 = secret.split('.').nth(1)?;
        // JWT 用 base64url 无填充
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .ok()?;
        let outer: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
        let serialized = outer.get("serialized")?.as_str()?;
        let inner: serde_json::Value = serde_json::from_str(serialized).ok()?;
        let uri = inner.get("initiateLoginUri")?.as_str()?;
        let host = uri.split("://").nth(1)?.split('/').next()?;
        if host.is_empty() {
            return None;
        }
        Some(host.to_string())
    }

    /// 是否为企业 IdC 账号。
    ///
    /// 企业账号的 SSO 登录目录是 `d-*.awsapps.com`（或自定义域名），而非 Builder ID 固定的
    /// `view.awsapps.com`。企业账号有自己的 profile，若注入共享的 Builder ID ARN，上游会以
    /// `403 Invalid token` 拒绝（token 属企业 SSO 实例、ARN 属另一 AWS 账户，身份不匹配）。
    ///
    /// 判定保守：仅当能从 client_secret 解出登录 host、且该 host 不是 Builder ID 的
    /// `view.awsapps.com` 时才视为企业；解析失败一律按非企业处理（保留 Builder ID 兜底行为）。
    pub fn is_enterprise_idc(&self) -> bool {
        match self.sso_login_host() {
            Some(host) => !host.eq_ignore_ascii_case(BUILDER_ID_START_HOST),
            None => false,
        }
    }

    /// 凭据缺失 profileArn 时，按 auth_method 推断默认共享 profileArn。
    ///
    /// - 企业 IdC → 不注入（企业账号有自己的 profile，外来 ARN 会被 403 Invalid token 拒绝）
    /// - social → SOCIAL_PROFILE_ARN
    /// - idc / builder-id / iam → BUILDER_ID_PROFILE_ARN
    /// - api_key → 不注入（走另一套鉴权）
    fn default_profile_arn(&self) -> Option<&'static str> {
        if self.is_api_key_credential() {
            return None;
        }
        if self.is_enterprise_idc() {
            return None;
        }
        match self.auth_method.as_deref() {
            Some(m) if m.eq_ignore_ascii_case("social") => Some(SOCIAL_PROFILE_ARN),
            _ => Some(BUILDER_ID_PROFILE_ARN),
        }
    }

    /// 获取实际应使用的 profileArn：自带优先，缺失则按 auth_method 兜底共享 ARN。
    ///
    /// 三级优先级：
    /// 1. **凭据自带 `profile_arn`**（Social/Pro 刷新会返回并写入；企业 IdC 则由
    ///    [`token_manager::list_available_profiles`] 探测后写回）。
    /// 2. **`default_profile_arn()` 兜底**：Builder ID / Social 用各自的共享 ARN；
    ///    企业 IdC（SSO 目录非 `view.awsapps.com`）返回 None，避免塞入外来 ARN 触发
    ///    上游 403 Invalid token。
    /// 3. 仍为 None：调用方自行决定是否带（getUsageLimits 会失败但不致命）。
    pub fn effective_profile_arn(&self) -> Option<String> {
        if let Some(arn) = self
            .profile_arn
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(arn.to_string());
        }
        self.default_profile_arn().map(|s| s.to_string())
    }

    /// 从 `profile_arn` 解析出所属 region（`arn:aws:codewhisperer:{region}:acct:profile/x`）。
    ///
    /// 企业 IdC 账号的 Q API region 以 profileArn 内嵌的为准，未必等于 SSO 认证 region。
    pub fn profile_arn_region(&self) -> Option<&str> {
        self.profile_arn
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|arn| arn.split(':').nth(3))
            .filter(|s| !s.is_empty())
    }

    /// 检查是否为 API Key 凭据
    ///
    /// API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需 refreshToken
    pub fn is_api_key_credential(&self) -> bool {
        self.kiro_api_key.is_some()
            || self
                .auth_method
                .as_deref()
                .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                .unwrap_or(false)
    }
}

#[cfg(test)]
impl KiroCredentials {
    fn from_json(json_string: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json_string)
    }

    fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Config;

    #[test]
    fn test_from_json() {
        let json = r#"{
            "accessToken": "test_token",
            "refreshToken": "test_refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2024-01-01T00:00:00Z",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2024-01-01T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("social".to_string()));
    }

    #[test]
    fn test_from_json_with_unknown_keys() {
        let json = r#"{
            "accessToken": "test_token",
            "unknownField": "should be ignored"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
    }

    /// 构造一段 client_secret(JWT) 用于测试 `is_enterprise_idc()` 判定。
    /// payload 段是 base64url 无填充编码的 `{"serialized":"{\"initiateLoginUri\":\"<uri>\"}"}`。
    fn make_client_secret(initiate_login_uri: &str) -> String {
        use base64::Engine;
        let serialized = serde_json::json!({ "initiateLoginUri": initiate_login_uri }).to_string();
        let outer = serde_json::json!({ "serialized": serialized }).to_string();
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(outer.as_bytes());
        format!("header.{}.sig", payload_b64)
    }

    #[test]
    fn test_effective_profile_arn_fallback() {
        // 1) auth_method=idc 缺失 ARN，且不是企业 IdC（无 client_secret 解不出 host，保守按非企业）→ Builder ID 共享 ARN
        let cred = KiroCredentials {
            auth_method: Some("idc".to_string()),
            ..Default::default()
        };
        assert_eq!(
            cred.effective_profile_arn().as_deref(),
            Some(BUILDER_ID_PROFILE_ARN),
            "idc 缺失 ARN 应回退到 Builder ID 共享 ARN"
        );

        // 2) auth_method=social 缺失 ARN → Social 共享 ARN
        let cred = KiroCredentials {
            auth_method: Some("social".to_string()),
            ..Default::default()
        };
        assert_eq!(
            cred.effective_profile_arn().as_deref(),
            Some(SOCIAL_PROFILE_ARN),
            "social 缺失 ARN 应回退到 Social 共享 ARN"
        );

        // 3) 凭据自带 ARN 优先（即使是 social 也用自带的）
        let cred = KiroCredentials {
            auth_method: Some("social".to_string()),
            profile_arn: Some(
                "arn:aws:codewhisperer:us-east-1:607416644019:profile/SELF".to_string(),
            ),
            ..Default::default()
        };
        assert_eq!(
            cred.effective_profile_arn().as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:607416644019:profile/SELF"),
            "自带 ARN 应优先于共享兜底"
        );

        // 4) 企业 IdC（SSO 目录 d-*.awsapps.com）缺失 ARN → 不兜底
        let cred = KiroCredentials {
            auth_method: Some("idc".to_string()),
            client_secret: Some(make_client_secret("https://d-1234567890.awsapps.com/start")),
            ..Default::default()
        };
        assert!(cred.is_enterprise_idc(), "d-*.awsapps.com 应判为企业 IdC");
        assert_eq!(
            cred.effective_profile_arn(),
            None,
            "企业 IdC 缺失 ARN 不应兜底 Builder ID（避免外来 ARN 触发 403 Invalid token）"
        );

        // 5) 显式 Builder ID（view.awsapps.com）→ 走 Builder ID 兜底
        let cred = KiroCredentials {
            auth_method: Some("idc".to_string()),
            client_secret: Some(make_client_secret("https://view.awsapps.com/start")),
            ..Default::default()
        };
        assert!(!cred.is_enterprise_idc(), "view.awsapps.com 不应判为企业 IdC");
        assert_eq!(
            cred.effective_profile_arn().as_deref(),
            Some(BUILDER_ID_PROFILE_ARN),
        );

        // 6) 空白串视为缺失 → 按 auth_method 兜底
        let cred = KiroCredentials {
            auth_method: Some("social".to_string()),
            profile_arn: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(
            cred.effective_profile_arn().as_deref(),
            Some(SOCIAL_PROFILE_ARN),
        );

        // 7) api_key 凭据不兜底（走另一套鉴权）
        let cred = KiroCredentials {
            auth_method: Some("api_key".to_string()),
            kiro_api_key: Some("ksk_xxx".to_string()),
            ..Default::default()
        };
        assert_eq!(cred.effective_profile_arn(), None, "api_key 不应兜底任何 ARN");
    }

    #[test]
    fn test_profile_arn_region() {
        let mut cred = KiroCredentials::default();
        assert_eq!(cred.profile_arn_region(), None);

        cred.profile_arn =
            Some("arn:aws:codewhisperer:ap-southeast-2:111122223333:profile/X".to_string());
        assert_eq!(cred.profile_arn_region(), Some("ap-southeast-2"));
    }

    #[test]
    fn test_to_json() {
        let creds = KiroCredentials {
            id: None,
            access_token: Some("token".to_string()),
            refresh_token: None,
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            client_id: None,
            client_secret: None,
            priority: 0,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            group: None,
            client_mode: None,
            disabled: false,
            kiro_api_key: None,
            rpm_limit: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("accessToken"));
        assert!(json.contains("authMethod"));
        assert!(!json.contains("refreshToken"));
        // priority 为 0 时不序列化
        assert!(!json.contains("priority"));
    }

    #[test]
    fn test_default_credentials_path() {
        assert_eq!(
            KiroCredentials::default_credentials_path(),
            "credentials.json"
        );
    }

    #[test]
    fn test_priority_default() {
        let json = r#"{"refreshToken": "test"}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 0);
    }

    #[test]
    fn test_priority_explicit() {
        let json = r#"{"refreshToken": "test", "priority": 5}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 5);
    }

    #[test]
    fn test_credentials_config_single() {
        let json = r#"{"refreshToken": "test", "expiresAt": "2025-12-31T00:00:00Z"}"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Single(_)));
    }

    #[test]
    fn test_credentials_config_multiple() {
        let json = r#"[
            {"refreshToken": "test1", "priority": 1},
            {"refreshToken": "test2", "priority": 0}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Multiple(_)));
        assert_eq!(config.into_sorted_credentials().len(), 2);
    }

    #[test]
    fn test_credentials_config_priority_sorting() {
        let json = r#"[
            {"refreshToken": "t1", "priority": 2},
            {"refreshToken": "t2", "priority": 0},
            {"refreshToken": "t3", "priority": 1}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        // 验证按优先级排序
        assert_eq!(list[0].refresh_token, Some("t2".to_string())); // priority 0
        assert_eq!(list[1].refresh_token, Some("t3".to_string())); // priority 1
        assert_eq!(list[2].refresh_token, Some("t1".to_string())); // priority 2
    }

    // ============ Region 字段测试 ============

    #[test]
    fn test_region_field_parsing() {
        // 测试解析包含 region 字段的 JSON
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, Some("us-east-1".to_string()));
    }

    #[test]
    fn test_region_field_missing_backward_compat() {
        // 测试向后兼容：不包含 region 字段的旧格式 JSON
        let json = r#"{
            "refreshToken": "test_refresh",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, None);
    }

    #[test]
    fn test_region_field_serialization() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            client_id: None,
            client_secret: None,
            priority: 0,
            region: Some("eu-west-1".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            group: None,
            client_mode: None,
            disabled: false,
            kiro_api_key: None,
            rpm_limit: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("region"));
        assert!(json.contains("eu-west-1"));
    }

    #[test]
    fn test_region_field_none_not_serialized() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            client_id: None,
            client_secret: None,
            priority: 0,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            group: None,
            client_mode: None,
            disabled: false,
            kiro_api_key: None,
            rpm_limit: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("region"));
    }

    // ============ MachineId 字段测试 ============

    #[test]
    fn test_machine_id_field_parsing() {
        let machine_id = "a".repeat(64);
        let json = format!(
            r#"{{
                "refreshToken": "test_refresh",
                "machineId": "{machine_id}"
            }}"#
        );

        let creds = KiroCredentials::from_json(&json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.machine_id, Some(machine_id));
    }

    #[test]
    fn test_machine_id_field_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = Some("b".repeat(64));

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("machineId"));
    }

    #[test]
    fn test_machine_id_field_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("machineId"));
    }

    #[test]
    fn test_multiple_credentials_with_different_regions() {
        // 测试多凭据场景下不同凭据使用各自的 region
        let json = r#"[
            {"refreshToken": "t1", "region": "us-east-1"},
            {"refreshToken": "t2", "region": "eu-west-1"},
            {"refreshToken": "t3"}
        ]"#;

        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        assert_eq!(list[0].region, Some("us-east-1".to_string()));
        assert_eq!(list[1].region, Some("eu-west-1".to_string()));
        assert_eq!(list[2].region, None);
    }

    #[test]
    fn test_region_field_with_all_fields() {
        // 测试包含所有字段的完整 JSON
        let json = r#"{
            "id": 1,
            "accessToken": "access",
            "refreshToken": "refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2025-12-31T00:00:00Z",
            "authMethod": "idc",
            "clientId": "client123",
            "clientSecret": "secret456",
            "priority": 5,
            "region": "ap-northeast-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.id, Some(1));
        assert_eq!(creds.access_token, Some("access".to_string()));
        assert_eq!(creds.refresh_token, Some("refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2025-12-31T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("idc".to_string()));
        assert_eq!(creds.client_id, Some("client123".to_string()));
        assert_eq!(creds.client_secret, Some("secret456".to_string()));
        assert_eq!(creds.priority, 5);
        assert_eq!(creds.region, Some("ap-northeast-1".to_string()));
    }

    #[test]
    fn test_region_roundtrip() {
        // 测试序列化和反序列化的往返一致性
        let original = KiroCredentials {
            id: Some(42),
            access_token: Some("token".to_string()),
            refresh_token: Some("refresh".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            client_id: None,
            client_secret: None,
            priority: 3,
            region: Some("us-west-2".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: Some("c".repeat(64)),
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            group: None,
            client_mode: None,
            disabled: false,
            kiro_api_key: None,
            rpm_limit: None,
        };

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.access_token, original.access_token);
        assert_eq!(parsed.refresh_token, original.refresh_token);
        assert_eq!(parsed.priority, original.priority);
        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.machine_id, original.machine_id);
    }

    // ============ auth_region / api_region 字段测试 ============

    #[test]
    fn test_auth_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "authRegion": "eu-central-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.auth_region, Some("eu-central-1".to_string()));
        assert_eq!(creds.api_region, None);
    }

    #[test]
    fn test_api_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "apiRegion": "ap-southeast-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.api_region, Some("ap-southeast-1".to_string()));
        assert_eq!(creds.auth_region, None);
    }

    #[test]
    fn test_auth_api_region_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = Some("eu-west-1".to_string());
        creds.api_region = Some("us-west-2".to_string());

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("authRegion"));
        assert!(json.contains("eu-west-1"));
        assert!(json.contains("apiRegion"));
        assert!(json.contains("us-west-2"));
    }

    #[test]
    fn test_auth_api_region_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = None;
        creds.api_region = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("authRegion"));
        assert!(!json.contains("apiRegion"));
    }

    #[test]
    fn test_auth_api_region_roundtrip() {
        let mut original = KiroCredentials::default();
        original.refresh_token = Some("refresh".to_string());
        original.region = Some("us-east-1".to_string());
        original.auth_region = Some("eu-west-1".to_string());
        original.api_region = Some("ap-northeast-1".to_string());

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.auth_region, original.auth_region);
        assert_eq!(parsed.api_region, original.api_region);
    }

    #[test]
    fn test_backward_compat_no_auth_api_region() {
        // 旧格式 JSON 不包含 authRegion/apiRegion，应正常解析
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.region, Some("us-east-1".to_string()));
        assert_eq!(creds.auth_region, None);
        assert_eq!(creds.api_region, None);
    }

    // ============ effective_auth_region / effective_api_region 优先级测试 ============

    #[test]
    fn test_effective_auth_region_credential_auth_region_highest() {
        // 凭据.auth_region > 凭据.region > config.auth_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());
        creds.auth_region = Some("cred-auth-region".to_string());

        assert_eq!(creds.effective_auth_region(&config), "cred-auth-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_credential_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());
        // auth_region 未设置

        assert_eq!(creds.effective_auth_region(&config), "cred-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_auth_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let creds = KiroCredentials::default();
        // auth_region 和 region 均未设置

        assert_eq!(creds.effective_auth_region(&config), "config-auth-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        // config.auth_region 未设置

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_auth_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_credential_api_region_highest() {
        // 凭据.api_region > config.api_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.api_region = Some("cred-api-region".to_string());

        assert_eq!(creds.effective_api_region(&config), "cred-api-region");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_api_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-api-region");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_ignores_credential_region() {
        // 凭据.region 不参与 api_region 的回退链
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut creds = KiroCredentials::default();
        creds.auth_region = Some("auth-only".to_string());
        creds.api_region = Some("api-only".to_string());

        assert_eq!(creds.effective_auth_region(&config), "auth-only");
        assert_eq!(creds.effective_api_region(&config), "api-only");
    }

    // ============ 凭据级代理优先级测试 ============

    fn empty_groups() -> BTreeMap<String, ProxyGroupConfig> {
        BTreeMap::new()
    }

    #[test]
    fn test_effective_proxy_credential_overrides_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("socks5://cred:1080".to_string());

        let result = creds.effective_proxy(Some(&global), &empty_groups());
        assert_eq!(result, Some(ProxyConfig::new("socks5://cred:1080")));
    }

    #[test]
    fn test_effective_proxy_credential_with_auth() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("http://proxy:3128".to_string());
        creds.proxy_username = Some("user".to_string());
        creds.proxy_password = Some("pass".to_string());

        let result = creds.effective_proxy(Some(&global), &empty_groups());
        let expected = ProxyConfig::new("http://proxy:3128").with_auth("user", "pass");
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_effective_proxy_direct_bypasses_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("direct".to_string());

        let result = creds.effective_proxy(Some(&global), &empty_groups());
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_direct_case_insensitive() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("DIRECT".to_string());

        let result = creds.effective_proxy(Some(&global), &empty_groups());
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_fallback_to_global() {
        let global = ProxyConfig::new("http://global:8080");
        let creds = KiroCredentials::default();

        let result = creds.effective_proxy(Some(&global), &empty_groups());
        assert_eq!(result, Some(ProxyConfig::new("http://global:8080")));
    }

    #[test]
    fn test_effective_proxy_none_when_no_proxy() {
        let creds = KiroCredentials::default();
        let result = creds.effective_proxy(None, &empty_groups());
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_group_overrides_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut groups = BTreeMap::new();
        groups.insert(
            "us-pool".to_string(),
            ProxyGroupConfig {
                proxy_url: "socks5://us:1080".to_string(),
                proxy_username: None,
                proxy_password: None,
                description: None,
            },
        );

        let mut creds = KiroCredentials::default();
        creds.group = Some("us-pool".to_string());

        let result = creds.effective_proxy(Some(&global), &groups);
        assert_eq!(result, Some(ProxyConfig::new("socks5://us:1080")));
    }

    #[test]
    fn test_effective_proxy_credential_overrides_group() {
        let global = ProxyConfig::new("http://global:8080");
        let mut groups = BTreeMap::new();
        groups.insert(
            "us-pool".to_string(),
            ProxyGroupConfig {
                proxy_url: "socks5://us:1080".to_string(),
                proxy_username: None,
                proxy_password: None,
                description: None,
            },
        );

        let mut creds = KiroCredentials::default();
        creds.group = Some("us-pool".to_string());
        creds.proxy_url = Some("socks5://cred:9999".to_string());

        let result = creds.effective_proxy(Some(&global), &groups);
        assert_eq!(result, Some(ProxyConfig::new("socks5://cred:9999")));
    }

    #[test]
    fn test_effective_proxy_group_with_auth() {
        let mut groups = BTreeMap::new();
        groups.insert(
            "us-pool".to_string(),
            ProxyGroupConfig {
                proxy_url: "http://gp:3128".to_string(),
                proxy_username: Some("gu".to_string()),
                proxy_password: Some("gp".to_string()),
                description: None,
            },
        );

        let mut creds = KiroCredentials::default();
        creds.group = Some("us-pool".to_string());

        let result = creds.effective_proxy(None, &groups);
        let expected = ProxyConfig::new("http://gp:3128").with_auth("gu", "gp");
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_effective_proxy_group_direct_bypasses_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut groups = BTreeMap::new();
        groups.insert(
            "no-proxy".to_string(),
            ProxyGroupConfig {
                proxy_url: "direct".to_string(),
                proxy_username: None,
                proxy_password: None,
                description: None,
            },
        );

        let mut creds = KiroCredentials::default();
        creds.group = Some("no-proxy".to_string());

        let result = creds.effective_proxy(Some(&global), &groups);
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_missing_group_falls_back_to_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.group = Some("undefined-group".to_string());

        let result = creds.effective_proxy(Some(&global), &empty_groups());
        assert_eq!(result, Some(ProxyConfig::new("http://global:8080")));
    }
}
