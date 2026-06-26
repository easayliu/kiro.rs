//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, Proxy};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 中继 DNS resolver：把上游 host 的解析重定向到中继域名（NLB 等），且**每次连接现场解析**
/// 中继域名 —— 这样中继/NLB 的 IP 变更后下次连接自动跟上，避免固定 IP 失效。
/// SNI / Host 仍是上游 host（reqwest 用请求 URL 的 host 做 SNI），故后端需对该 host 持有有效证书。
#[derive(Debug)]
struct RelayResolver {
    /// 中继地址 `host:port`（如 `kiro-xxx.elb.us-east-1.amazonaws.com:443`）
    authority: String,
}

impl Resolve for RelayResolver {
    fn resolve(&self, _name: Name) -> Resolving {
        let authority = self.authority.clone();
        Box::pin(async move {
            match tokio::net::lookup_host(authority.as_str()).await {
                Ok(iter) => {
                    let addrs: Addrs = Box::new(iter.collect::<Vec<SocketAddr>>().into_iter());
                    Ok(addrs)
                }
                Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
            }
        })
    }
}

/// 代理配置
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    ///
    /// 自动识别 `scheme://user:pass@host:port` 形式的内联认证：
    /// userinfo 会被抽取到 `username`/`password` 字段，URL 字段仅保留
    /// `scheme://host:port[/path...]`。`with_auth()` 显式调用仍可覆盖。
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        if let Some((clean_url, username, password)) = extract_inline_userinfo(&url) {
            return Self {
                url: clean_url,
                username: Some(username),
                password: Some(password),
            };
        }
        Self {
            url,
            username: None,
            password: None,
        }
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// 解析 URL 中的内联 userinfo（`scheme://user:pass@host[...]` → 去除 userinfo 后的 URL + user + pass）
///
/// 行为说明：
/// - 仅当 userinfo 同时包含 `user` 和 `pass`（以 `:` 分隔）时才抽取，单个 user 不处理
/// - 当密码中含 `@` 时按 **最后一个** `@` 分隔（兼容用户输入未编码的密码）
/// - 未识别到内联 userinfo 时返回 `None`
fn extract_inline_userinfo(url: &str) -> Option<(String, String, String)> {
    let scheme_end = url.find("://")?;
    let after_scheme = &url[scheme_end + 3..];

    // authority 部分到第一个 '/', '?', '#' 截止
    let authority_end = after_scheme
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];

    // 在 authority 中按"最后一个 @"分隔，容忍密码中含 '@'
    let at_pos = authority.rfind('@')?;
    let userinfo = &authority[..at_pos];

    // userinfo 必须形如 `user:pass`，单 user 不处理
    let colon_pos = userinfo.find(':')?;
    let user = &userinfo[..colon_pos];
    let pass = &userinfo[colon_pos + 1..];
    if user.is_empty() {
        return None;
    }

    // 重组无 userinfo 的 URL
    let rest = &after_scheme[at_pos + 1..]; // host[:port][/path...]
    let clean_url = format!("{}://{}", &url[..scheme_end], rest);

    Some((clean_url, user.to_string(), pass.to_string()))
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 空闲读超时（秒）：上游在该时长内无任何新字节才超时；
///   非绝对总时限，健康的长流不受总时长限制。
///
/// # Returns
/// 配置好的 reqwest::Client
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    build_client_with_resolve(proxy, timeout_secs, tls_backend, None)
}

/// 构建 HTTP Client，可选中继。
///
/// `relay = Some("host:port")` 时安装 [`RelayResolver`]：上游 host 的连接被定向到中继地址，
/// 中继域名每次连接现场解析（IP 变更自动跟上），但 TLS SNI / HTTP Host 仍是上游 host
/// —— 用于"经中继（如 NLB→PrivateLink）连上游、证书/Host 仍校验原服务域名"的场景。
pub fn build_client_with_resolve(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
    relay: Option<&str>,
) -> anyhow::Result<Client> {
    // 固定 HTTP/1.1：
    // 1. 与真实 Kiro IDE（aws-sdk-js / node）一致——抓包显示其用 HTTP/1.1 + Connection: close；
    // 2. 规避上游 HTTP/2 在传 body 中途发 RST_STREAM(INTERNAL_ERROR) 导致的
    //    "stream error received" 502（不固定时 reqwest 会经 ALPN 协商成 h2）。
    // TCP keepalive：对齐真实 Kiro IDE（aws-sdk-js/Node 的 socket 默认开 keepAlive）。
    // opus 长时间思考期间上游连接会进入静默，若无 keepalive 探针，路径上的
    // NAT/防火墙/边缘会按 ~4 分钟 idle 上限把空闲 TLS 连接丢弃，rustls 随即报
    // "peer closed connection without sending TLS close_notify"，表现为回复中途截断。
    // 设 30s 首探针间隔（远小于观测到的 ~240s 截断点），空闲期持续产生探针保活。
    //
    // 超时策略：用 read_timeout（空闲读超时）而非 timeout（绝对总时限）。
    // reqwest 的 `.timeout()` 是从请求起算的绝对 deadline，且对 `bytes_stream()` 的
    // 流式读取同样生效——哪怕数据一直在健康流动，整条流累计超过该值就会在流中途被掐断
    // （stop_reason 缺失、无 [DONE]，表现为回复截断）。长生成（如接近 128k 输出，
    // 按 ~50 tok/s 约需 40min）会被这道墙拦在 ~timeout 处。
    // `.read_timeout()` 改为"每次读操作的间隔超时、成功读一次即重置"：只在上游真正卡住
    // （在 timeout_secs 内没有任何新字节）时才超时，健康的长流不论总时长都不受限。
    // connect_timeout 单独给连接/TLS 握手兜底（原先这一段也靠总 timeout 兜，拆掉后需补），
    // 顺带让连不通的端点更快故障转移。
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(timeout_secs))
        .tcp_keepalive(Duration::from_secs(30))
        .http1_only();

    if tls_backend == TlsBackend::Rustls {
        builder = builder.use_rustls_tls();
    }

    // 中继：用自定义 resolver 把上游 host 解析到中继域名的当前 IP（SNI/Host 不变）。
    // 连接失败时由调用方降级到直连。
    if let Some(authority) = relay {
        builder = builder.dns_resolver(Arc::new(RelayResolver {
            authority: authority.to_string(),
        }));
    }

    if let Some(proxy_config) = proxy {
        let mut proxy = Proxy::all(&proxy_config.url)?;

        // 设置代理认证
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(username, password);
        }

        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 使用代理: {}", proxy_config.url);
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    // ============ 内联 userinfo 解析测试 ============

    #[test]
    fn test_inline_userinfo_socks5() {
        // 用户提供的真实样例
        let config = ProxyConfig::new(
            "socks5://sub2:sfijenwpaongpiuhsfdjwDFOSwe@98.115.241.78:40031",
        );
        assert_eq!(config.url, "socks5://98.115.241.78:40031");
        assert_eq!(config.username, Some("sub2".to_string()));
        assert_eq!(config.password, Some("sfijenwpaongpiuhsfdjwDFOSwe".to_string()));
    }

    #[test]
    fn test_inline_userinfo_http() {
        let config = ProxyConfig::new("http://user:pass@proxy.example.com:3128");
        assert_eq!(config.url, "http://proxy.example.com:3128");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_inline_userinfo_with_path() {
        let config = ProxyConfig::new("http://user:pass@host:8080/path?q=1");
        assert_eq!(config.url, "http://host:8080/path?q=1");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_inline_userinfo_password_with_at() {
        // 密码里含 '@'：按最后一个 '@' 分隔
        let config = ProxyConfig::new("socks5://u:p@ss@host:1080");
        assert_eq!(config.url, "socks5://host:1080");
        assert_eq!(config.username, Some("u".to_string()));
        assert_eq!(config.password, Some("p@ss".to_string()));
    }

    #[test]
    fn test_no_inline_userinfo_when_absent() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_no_inline_userinfo_when_user_only() {
        // 只有用户名没有密码：不抽取，保留原样让 reqwest 处理
        let config = ProxyConfig::new("http://user@host:8080");
        assert_eq!(config.url, "http://user@host:8080");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_with_auth_overrides_inline() {
        // 显式 with_auth 应该覆盖从 URL 解析出来的认证
        let config = ProxyConfig::new("http://inlineuser:inlinepass@host:8080")
            .with_auth("override_user", "override_pass");
        assert_eq!(config.url, "http://host:8080");
        assert_eq!(config.username, Some("override_user".to_string()));
        assert_eq!(config.password, Some("override_pass".to_string()));
    }

    #[test]
    fn test_inline_userinfo_does_not_eat_at_in_path() {
        // path 中的 '@' 不应被当作 userinfo 分隔符
        let config = ProxyConfig::new("http://host:8080/p@th");
        assert_eq!(config.url, "http://host:8080/p@th");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }
}
