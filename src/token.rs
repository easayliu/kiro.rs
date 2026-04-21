//! Token 计算模块
//!
//! 采用字符启发式算法：非西文字符每个计 4.5 字符单位、西文字符每个计 1 单位，
//! 除以 4 得基础 tokens 后按长度分段放大系数。优点是纯计算、0 依赖、完全
//! 确定性、跨版本/环境/语言实现一致 —— 同一段文本无论何时何地结果恒定，
//! 便于客户端通过稳定倍率做对账校正。
//!
//! 代价：相对 Claude 真实分词，绝对值会系统性偏高（中文尤甚），**不是精度
//! 工具**。需要 100% 精确值请配置 `count_tokens_api_url` 走远程
//! `/v1/messages/count_tokens`，本地计算仅作为回退。

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, Message, SystemMessage, Tool,
};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use std::sync::OnceLock;

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

/// 计算文本的 token 数量（字符启发式）。空串返回 0。
///
/// 非西文字符 × 4.5、西文字符 × 1，除以 4 得基础 tokens，
/// 再按下列分段乘放大系数（短文本放得更多，长文本回归 1.0）：
/// - `< 100` → ×1.5
/// - `< 200` → ×1.3
/// - `< 300` → ×1.25
/// - `< 800` → ×1.2
/// - `≥ 800` → ×1.0
pub fn count_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }

    let char_units: f64 = text
        .chars()
        .map(|c| if is_non_western_char(c) { 4.5 } else { 1.0 })
        .sum();
    let tokens = char_units / 4.0;

    let scaled = if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens
    };

    scaled as u64
}

/// 判断字符是否为非西文字符。
///
/// 西文范围覆盖基本 ASCII、拉丁扩展 A/B、Latin Extended Additional，
/// 以及几块拉丁变体（C/D/E）。其余一律视为非西文（中日韩、阿拉伯、
/// 符号等），按更高系数计 token。
fn is_non_western_char(c: char) -> bool {
    !matches!(
        c,
        '\u{0000}'..='\u{00FF}'
            | '\u{0100}'..='\u{024F}'
            | '\u{1E00}'..='\u{1EFF}'
            | '\u{2C60}'..='\u{2C7F}'
            | '\u{A720}'..='\u{A7FF}'
            | '\u{AB30}'..='\u{AB6F}'
    )
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

/// 本地计算请求的输入 tokens
///
/// 只累加 block 级内容 tokens，不额外加 per-message overhead —— 为保持
/// 确定性和与上游算法一致，任何恒定附加项都会被客户端的对账倍率吸收，
/// 所以这里不做加法，改由 calibration 系数统一消化。
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
        // "Hello, world!" 13 西文字符 → 13/4=3.25 → ×1.5 ≈ 4，区间 [3,6] 防护
        let n = count_tokens("Hello, world!");
        assert!((3..=6).contains(&n), "got {n}");
    }

    #[test]
    fn count_tokens_chinese_sanity() {
        // 字符启发式：非西文 ×4.5、西文 ×1，/4 后按段放大。
        // 该文本主体为中文（非西文），结果稳定落在约 50-80 区间。
        let text = "你好世界，这是一个测试。一个很长的测试文本，用来对比不同 tokenizer 的差异。";
        let n = count_tokens(text);
        assert!((50..=80).contains(&n), "got {n}");
    }

    /// 回归防护：西文字符按 1 单位、非西文按 4.5 单位分别计入。
    #[test]
    fn count_tokens_char_classification() {
        // 纯西文 8 字符 → 8/4=2 → ×1.5 = 3
        assert_eq!(count_tokens("abcdefgh"), 3);
        // 纯非西文 4 字符 → 4*4.5/4=4.5 → ×1.5 = 6
        assert_eq!(count_tokens("你好世界"), 6);
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
