//! Token 计算模块
//!
//! 使用 `claude-tokenizer` 内嵌的 Claude v3 BPE 表（来自 Anthropic Python SDK
//! 早期 bundle 的 `claude-v3-tokenizer.json`），是目前和官方最接近的开源
//! tokenizer。Claude 4+ 的 tokenizer Anthropic 未公开，理论上可能有细微差异，
//! 但远优于字符启发式；需要 100% 精确值请配置 `count_tokens_api_url` 走远程
//! `/v1/messages/count_tokens`。
//!
//! `claude-tokenizer::count_tokens` 源码里每次都重建 Tokenizer（几十 ms 的
//! 开销），这里用 OnceLock 缓存单例避免热路径重复加载。

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, Message, SystemMessage, Tool,
};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use std::sync::OnceLock;
use tokenizers::Tokenizer;
use tokenizers::tokenizer::{EncodeInput, InputSequence};

/// Count Tokens API 配置
#[derive(Clone, Default)]
pub struct CountTokensConfig {
    /// 外部 count_tokens API 地址
    pub api_url: Option<String>,
    /// count_tokens API 密钥
    pub api_key: Option<String>,
    /// count_tokens API 认证类型（"x-api-key" 或 "bearer"）
    pub auth_type: String,
    /// 代理配置
    pub proxy: Option<ProxyConfig>,

    pub tls_backend: TlsBackend,
}

/// 全局配置存储
static COUNT_TOKENS_CONFIG: OnceLock<CountTokensConfig> = OnceLock::new();

/// 初始化 count_tokens 配置
///
/// 应在应用启动时调用一次
pub fn init_config(config: CountTokensConfig) {
    let _ = COUNT_TOKENS_CONFIG.set(config);
}

/// 获取配置
fn get_config() -> Option<&'static CountTokensConfig> {
    COUNT_TOKENS_CONFIG.get()
}

/// 缓存的 Claude v3 tokenizer 实例。
static CLAUDE_TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();

fn claude_tokenizer() -> &'static Tokenizer {
    CLAUDE_TOKENIZER.get_or_init(claude_tokenizer::get_tokenizer)
}

/// 计算文本的 token 数量（Claude v3 BPE）。空串返回 0。
pub fn count_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let input = EncodeInput::Single(InputSequence::Raw(text.into()));
    match claude_tokenizer().encode(input, false) {
        Ok(encoded) => encoded.len() as u64,
        Err(e) => {
            tracing::warn!(error = ?e, "claude tokenizer encode 失败，回退到字符数/4 估算");
            (text.chars().count() as u64 / 4).max(1)
        }
    }
}

/// 估算请求的输入 tokens
///
/// 优先调用远程 API，失败时回退到本地计算
pub(crate) fn count_all_tokens(
    model: String,
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    // 检查是否配置了远程 API
    if let Some(config) = get_config() {
        if let Some(api_url) = &config.api_url {
            // 尝试调用远程 API
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(call_remote_count_tokens(
                    api_url, config, model, &system, &messages, &tools,
                ))
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!("远程 count_tokens API 返回: {}", tokens);
                    return tokens;
                }
                Err(e) => {
                    tracing::warn!("远程 count_tokens API 调用失败，回退到本地计算: {}", e);
                }
            }
        }
    }

    // 本地计算
    count_all_tokens_local(system, messages, tools)
}

/// 调用远程 count_tokens API
async fn call_remote_count_tokens(
    api_url: &str,
    config: &CountTokensConfig,
    model: String,
    system: &Option<Vec<SystemMessage>>,
    messages: &Vec<Message>,
    tools: &Option<Vec<Tool>>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = build_client(config.proxy.as_ref(), 300, config.tls_backend)?;

    // 构建请求体
    let request = CountTokensRequest {
        model: model, // 模型名称用于 token 计算
        messages: messages.clone(),
        system: system.clone(),
        tools: tools.clone(),
    };

    // 构建请求
    let mut req_builder = client.post(api_url);

    // 设置认证头
    if let Some(api_key) = &config.api_key {
        if config.auth_type == "bearer" {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        } else {
            req_builder = req_builder.header("x-api-key", api_key);
        }
    }

    // 发送请求
    let response = req_builder
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("API 返回错误状态: {}", response.status()).into());
    }

    let result: CountTokensResponse = response.json().await?;
    Ok(result.input_tokens as u64)
}

/// 每条 message 的结构开销（role token、分隔符等），模拟 Anthropic 内部 overhead
const TOKENS_PER_MESSAGE_OVERHEAD: u64 = 4;

/// 本地计算请求的输入 tokens
///
/// 使用与 cache_tracker 相同的 block 级函数计算内容 tokens，
/// 再加上消息结构 overhead 来模拟 Anthropic 的总量计算。
fn count_all_tokens_local(
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    let mut total = 0;

    if let Some(ref tools) = tools {
        for tool in tools {
            total += count_tool_definition_tokens(tool);
        }
    }

    if let Some(ref system) = system {
        for msg in system {
            total += count_system_message_tokens(msg);
        }
    }

    for msg in &messages {
        total += count_message_content_tokens(&msg.content);
        total += TOKENS_PER_MESSAGE_OVERHEAD;
    }

    total.max(1)
}

/// 估算输出 tokens
pub(crate) fn estimate_output_tokens(content: &[serde_json::Value]) -> i32 {
    let mut total = 0;

    for block in content {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            total += count_tokens(text) as i32;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            // 工具调用开销
            if let Some(input) = block.get("input") {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                total += count_tokens(&input_str) as i32;
            }
        }
    }

    total.max(1)
}

/// 计算系统消息的 tokens
pub(crate) fn count_system_message_tokens(message: &SystemMessage) -> u64 {
    count_tokens(&message.text)
}

/// 计算工具定义的 tokens
pub(crate) fn count_tool_definition_tokens(tool: &Tool) -> u64 {
    let json = serde_json::to_string(tool).unwrap_or_default();
    count_tokens(&json)
}

/// 计算消息内容块的 tokens（用于 cache_tracker 计算每个 block 的 token 数）
pub(crate) fn count_message_content_tokens(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Null => 0,
        serde_json::Value::String(s) => count_tokens(s),
        serde_json::Value::Array(arr) => arr.iter().map(count_message_content_tokens).sum(),
        serde_json::Value::Object(obj) => {
            if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                return count_tokens(text);
            }
            if let Some(thinking) = obj.get("thinking").and_then(|v| v.as_str()) {
                return count_tokens(thinking);
            }
            if let Some(input) = obj.get("input") {
                let json = serde_json::to_string(input).unwrap_or_default();
                return count_tokens(&json);
            }
            if let Some(content) = obj.get("content") {
                return count_message_content_tokens(content);
            }
            0
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_tokens_empty_is_zero() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn count_tokens_english_sanity() {
        // "Hello, world!" 在 Claude v3 BPE 下通常是 4 tokens 左右（≤ 6 可接受）
        let n = count_tokens("Hello, world!");
        assert!((3..=6).contains(&n), "got {n}");
    }

    #[test]
    fn count_tokens_chinese_sanity() {
        // 43 中文字符用 Claude v3 tokenizer 是 27 tokens（实测）。
        // 给个宽松区间 [20, 40] 作为回归防护。
        let text = "你好世界，这是一个测试。一个很长的测试文本，用来对比不同 tokenizer 的差异。";
        let n = count_tokens(text);
        assert!((20..=40).contains(&n), "got {n}");
    }

    /// 回归防护：tokenizer 成功初始化，连续两次调用返回相同结果（单例）。
    #[test]
    fn count_tokens_singleton_stable() {
        let a = count_tokens("the quick brown fox jumps over the lazy dog");
        let b = count_tokens("the quick brown fox jumps over the lazy dog");
        assert_eq!(a, b);
        assert!(a > 0);
    }
}
