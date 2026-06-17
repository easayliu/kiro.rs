use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// 客户端模拟模式
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ClientMode {
    /// 模拟 Kiro IDE（默认，原有行为）
    #[default]
    KiroIde,
    /// 模拟 Kiro CLI
    KiroCli,
}

impl ClientMode {
    /// 获取 origin 字段值
    pub fn origin(&self) -> &'static str {
        match self {
            ClientMode::KiroIde => "AI_EDITOR",
            ClientMode::KiroCli => "KIRO_CLI",
        }
    }

    /// 是否为 kiro-cli 模式
    pub fn is_cli(&self) -> bool {
        matches!(self, ClientMode::KiroCli)
    }
}

/// 代理分组配置（共享给一组凭据使用）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyGroupConfig {
    /// 代理 URL，支持 http/https/socks5
    /// 特殊值 "direct" 表示显式不使用代理（覆盖全局代理）
    pub proxy_url: String,

    /// 代理认证用户名（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,

    /// 分组说明（可选，仅用于前端展示）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    /// API Host 模板（用于 generateAssistantResponse / mcp 的 URL 与 host 头），
    /// 模板中的 `{region}` 占位符会被替换为有效 API region。
    /// 未配置时默认 `q.{region}.amazonaws.com`；可设为 `runtime.{region}.kiro.dev` 切换到 Kiro runtime 端点。
    /// 注意：`ListAvailableModels` 不受此项影响（runtime 端点不提供该 API），始终走 `q.{region}.amazonaws.com`。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_host_template: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// 代理分组（可选）
    /// 凭据通过 `group` 字段引用对应分组的代理配置
    /// 解析优先级：凭据自身代理 > 凭据所属分组代理 > 全局代理 > 无
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub proxy_groups: BTreeMap<String, ProxyGroupConfig>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 游客 API 密钥列表（可选，仅授予只读权限）
    ///
    /// 命中其中任一 key 时只允许 GET 请求；写操作（POST/PUT/DELETE）一律 403。
    /// 仅在配置了 `admin_api_key` 时生效——Admin API 整体由 admin key 决定是否启用。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guest_api_keys: Vec<String>,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块，与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 全局缓存模式（默认 true）
    ///
    /// 启用后，所有凭据共享同一份 prompt cache checkpoint 表；
    /// 关闭后，每个凭据独立维护 checkpoint，互不影响。
    /// 注：存在 `cache_scope` 时以 `cache_scope` 为准。
    #[serde(default = "default_global_cache")]
    pub global_cache: bool,

    /// 缓存分桶策略（覆盖 `global_cache`）。可选值：
    /// - `"global"`：按用户身份（metadata.user_id）分桶，同一用户跨 credential 共享
    /// - `"per_credential"`：在用户身份基础上再按 credential 隔离
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<String>,

    /// 缓存查找跳过率（0.0-1.0，默认 None 不启用）
    ///
    /// 启用后，每个有 breakpoint 的请求以此概率跳过 cache 查找（当作
    /// 首次请求，cache_read = 0），但仍正常写入 checkpoint；用于在
    /// 自然命中率偏高时整体降低可观察到的缓存命中率。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_skip_rate: Option<f32>,

    /// 客户端模拟模式（"kiro-ide" 或 "kiro-cli"）
    #[serde(default)]
    pub client_mode: ClientMode,

    /// 全局默认 RPM 上限（每分钟请求数）
    ///
    /// 凭据未单独配置 `rpmLimit` 时回退到此值；都未配置则不限流。
    /// 触发限流的凭据会被本地冷却到当前滑动窗口结束（最多 60s），
    /// 期间自动切换到其他凭据。设置为 0 表示禁用全局默认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_rpm_limit: Option<u32>,

    /// 全局默认并发上限（每个凭据同时在途请求数）
    ///
    /// 凭据未单独配置 `concurrencyLimit` 时回退到此值；都未配置则不限并发。
    /// 选号时在途数已达上限的凭据会被跳过、自动切换到其他凭据；
    /// 当所有凭据都达上限时回退到负载最轻者（不硬拒绝请求）。
    /// 设置为 0 表示禁用全局默认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_concurrency_limit: Option<u32>,

    /// 上报给客户端的 usage token 放大倍率（> 0，默认 1.0 = 不放大）
    ///
    /// 仅放大响应中返回给客户端的 token 计数（input/output/cache），用于按倍率抬高
    /// 下游「按 token 计费」的费用；不影响内部记录的真实上游成本与官方价。可在管理
    /// 后台运行时调整。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_multiplier: Option<f64>,

    /// kiro-cli 版本号（仅 kiro-cli 模式使用）
    #[serde(default = "default_kiro_cli_version")]
    pub kiro_cli_version: String,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "0.12.263".to_string()
}

fn default_system_version() -> String {
    const SYSTEM_VERSIONS: &[&str] = &["darwin#24.6.0", "win32#10.0.22631"];
    SYSTEM_VERSIONS[fastrand::usize(..SYSTEM_VERSIONS.len())].to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_global_cache() -> bool {
    true
}

fn default_kiro_cli_version() -> String {
    "1.29.3".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            api_host_template: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            proxy_groups: BTreeMap::new(),
            admin_api_key: None,
            guest_api_keys: Vec::new(),
            load_balancing_mode: default_load_balancing_mode(),
            extract_thinking: default_extract_thinking(),
            global_cache: default_global_cache(),
            cache_scope: None,
            cache_skip_rate: None,
            client_mode: ClientMode::default(),
            default_rpm_limit: None,
            default_concurrency_limit: None,
            usage_multiplier: None,
            kiro_cli_version: default_kiro_cli_version(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 默认 API Host 模板（`q.{region}.amazonaws.com`）。
    pub const DEFAULT_API_HOST_TEMPLATE: &'static str = "q.{region}.amazonaws.com";

    /// 把 API host 模板里的 `{region}` 替换为入参 region，返回最终 host。
    /// 调用方传入的应是 `effective_api_region` 解析后的 region。
    pub fn effective_api_host(&self, region: &str) -> String {
        self.api_host_template
            .as_deref()
            .unwrap_or(Self::DEFAULT_API_HOST_TEMPLATE)
            .replace("{region}", region)
    }

    /// 生成 API 请求的 user-agent（streaming API）
    pub fn streaming_user_agent(&self, machine_id: &str, mode: ClientMode) -> String {
        match mode {
            ClientMode::KiroCli => format!(
                "aws-sdk-rust/1.3.14 ua/2.1 api/codewhispererstreaming/0.1.14474 os/linux lang/rust/1.92.0 md/appVersion-{} app/AmazonQ-For-CLI",
                self.kiro_cli_version
            ),
            ClientMode::KiroIde => format!(
                "aws-sdk-js/1.0.39 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.39 m/N KiroIDE-{}-{}",
                self.system_version, self.node_version, self.kiro_version, machine_id
            ),
        }
    }

    /// 生成 API 请求的 x-amz-user-agent（streaming API）
    pub fn streaming_x_amz_user_agent(&self, machine_id: &str, mode: ClientMode) -> String {
        match mode {
            ClientMode::KiroCli => "aws-sdk-rust/1.3.14 ua/2.1 api/codewhispererstreaming/0.1.14474 os/linux lang/rust/1.92.0 m/F app/AmazonQ-For-CLI".to_string(),
            ClientMode::KiroIde => format!("aws-sdk-js/1.0.39 KiroIDE-{}-{}", self.kiro_version, machine_id),
        }
    }

    /// 生成 runtime API 的 user-agent（非 streaming）
    pub fn runtime_user_agent(&self, machine_id: &str, mode: ClientMode) -> String {
        match mode {
            ClientMode::KiroCli => format!(
                "aws-sdk-rust/1.3.14 ua/2.1 api/codewhispererruntime/0.1.14474 os/linux lang/rust/1.92.0 md/appVersion-{} app/AmazonQ-For-CLI",
                self.kiro_cli_version
            ),
            ClientMode::KiroIde => format!(
                "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
                self.system_version, self.node_version, self.kiro_version, machine_id
            ),
        }
    }

    /// 生成 runtime API 的 x-amz-user-agent（非 streaming）
    pub fn runtime_x_amz_user_agent(&self, machine_id: &str, mode: ClientMode) -> String {
        match mode {
            ClientMode::KiroCli => "aws-sdk-rust/1.3.14 ua/2.1 api/codewhispererruntime/0.1.14474 os/linux lang/rust/1.92.0 m/F app/AmazonQ-For-CLI".to_string(),
            ClientMode::KiroIde => format!("aws-sdk-js/1.0.0 KiroIDE-{}-{}", self.kiro_version, machine_id),
        }
    }

    /// 生成 token 刷新的 user-agent
    pub fn refresh_user_agent(&self, machine_id: &str, mode: ClientMode) -> String {
        match mode {
            ClientMode::KiroCli => "Kiro-CLI".to_string(),
            ClientMode::KiroIde => format!("KiroIDE-{}-{}", self.kiro_version, machine_id),
        }
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());
        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content).with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}
