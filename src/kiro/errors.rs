//! Provider 层错误类型
//!
//! 用 typed error 携带上游 HTTP 状态码，避免被 anyhow 字串化后丢失，
//! 让上游 handler 能按 status 精确映射客户端响应（如 429 透传、503 区分内部不可用）。

use std::fmt;

/// 上游 API 返回的 HTTP 错误（4xx/5xx）
///
/// 携带原始 status / body / api_type，供 handlers 决定如何映射给下游。
/// Display 与历史字符串格式保持一致（`{api_type} API 请求失败: {status} {body}`），
/// 现有依赖 `err.to_string().contains(...)` 的代码无需修改。
///
/// `credential_id` 记录产生该错误时实际使用的凭据 id（多凭据重试时为最后一次尝试的凭据），
/// 供失败时序统计按凭据归类；无关联凭据（如请求未发到上游）时为 None。
#[derive(Debug)]
pub struct UpstreamHttpError {
    pub status: u16,
    pub body: String,
    pub api_type: String,
    pub credential_id: Option<u64>,
}

impl fmt::Display for UpstreamHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} API 请求失败: {} {}",
            self.api_type, self.status, self.body
        )
    }
}

impl std::error::Error for UpstreamHttpError {}

/// 内部"无可用凭据"错误（凭据全部 disabled，请求未发到上游）
///
/// Display 保持历史格式 `所有凭据均已禁用（{available}/{total}）`，
/// 既兼容旧测试，又便于人类读取。
#[derive(Debug)]
pub struct NoAvailableCredentialsError {
    pub available: usize,
    pub total: usize,
}

impl fmt::Display for NoAvailableCredentialsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "所有凭据均已禁用（{}/{}）",
            self.available, self.total
        )
    }
}

impl std::error::Error for NoAvailableCredentialsError {}

/// 上游响应体读取失败（已收到 200 响应头，但读取 body 时连接中断/超时/提前 EOF）
///
/// 区别于 UpstreamHttpError（上游返回 4xx/5xx 状态）：这里上游已回 200，是 body
/// 传输阶段出错。携带实际使用的凭据 id，供失败时序统计按凭据归类；映射给下游为 502。
#[derive(Debug)]
pub struct UpstreamBodyError {
    pub credential_id: u64,
    pub message: String,
}

impl fmt::Display for UpstreamBodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "上游响应体读取失败: {}", self.message)
    }
}

impl std::error::Error for UpstreamBodyError {}
