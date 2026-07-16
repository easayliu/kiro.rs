//! Token 计算模块
//!
//! 本地估算口径：启发式 `ceil(utf8_byte_len / 4)`。`BYTES_PER_TOKEN=4` 取 Claude
//! 家族 BPE 的平均压缩比（约 3.5–4 字节/token）。离线、零依赖、确定性、无 panic。
//!
//! 注意：本模块只负责**本地估算/打底**。需要 Claude 精确值时配置
//! `count_tokens_api_url` 走远程 `/v1/messages/count_tokens`；实际计费仍以上游
//! `contextUsageEvent` 反推的 input_tokens 为准（见 anthropic::stream）。

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, Message, SystemMessage, Tool,
};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use parking_lot::RwLock;

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

/// 远程 count_tokens 的默认地址：Anthropic 官方端点。
///
/// `api_url` 未配置时回落到此值，免去每次填写；填了则用自定义地址（自建中继等）。
pub const DEFAULT_COUNT_TOKENS_API_URL: &str = "https://api.anthropic.com/v1/messages/count_tokens";

/// 全局配置存储。
///
/// 用 `RwLock` 而非 `OnceLock`：Admin API 需要在运行时改 count_tokens 配置并即时
/// 生效（`OnceLock::set` 第二次调用会静默失败，页面显示成功但配置不动）。
static COUNT_TOKENS_CONFIG: RwLock<Option<CountTokensConfig>> = RwLock::new(None);

/// 初始化 count_tokens 配置（启动时调用）
pub fn init_config(config: CountTokensConfig) {
    *COUNT_TOKENS_CONFIG.write() = Some(config);
}

/// 运行时更新 count_tokens 配置（Admin API 调用，即时生效）
pub fn update_config(api_url: Option<String>, api_key: Option<String>, auth_type: String) {
    let mut guard = COUNT_TOKENS_CONFIG.write();
    if let Some(cfg) = guard.as_mut() {
        cfg.api_url = api_url;
        cfg.api_key = api_key;
        cfg.auth_type = auth_type;
    }
}

/// 读取当前 count_tokens 配置（api_url, api_key, auth_type）
pub fn get_config_snapshot() -> (Option<String>, Option<String>, String) {
    let guard = COUNT_TOKENS_CONFIG.read();
    match guard.as_ref() {
        Some(c) => (c.api_url.clone(), c.api_key.clone(), c.auth_type.clone()),
        None => (None, None, default_count_tokens_auth_type()),
    }
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

/// 获取配置（内部使用，返回克隆以避免持锁跨 await）
fn get_config() -> Option<CountTokensConfig> {
    COUNT_TOKENS_CONFIG.read().clone()
}

/// 远程 count_tokens 是否已启用（配了密钥即启用）。
///
/// 供调用方在做「远程失败则整体退回本地」这类兜底前先行短路——未启用不是失败，
/// 不该走告警路径。
pub(crate) fn remote_enabled() -> bool {
    get_config()
        .map(|c| resolve_remote(&c).is_some())
        .unwrap_or(false)
}

/// 解析出「本次是否走远程、走哪个地址」。
///
/// 开关以**密钥**为准：没配密钥就不发远程（避免必然 401 还白跑一次往返）。
/// 地址缺省时回落 [`DEFAULT_COUNT_TOKENS_API_URL`]。
fn resolve_remote(config: &CountTokensConfig) -> Option<String> {
    config.api_key.as_ref()?;
    Some(
        config
            .api_url
            .clone()
            .unwrap_or_else(|| DEFAULT_COUNT_TOKENS_API_URL.to_string()),
    )
}

/// 启发式换算基数（UTF-8 字节 / 4，贴近 Claude BPE 平均压缩比）。
const BYTES_PER_TOKEN: u64 = 4;

/// 计算文本的 token 数量：启发式 `ceil(utf8_byte_len / BYTES_PER_TOKEN)`。空串返回 0。
pub fn count_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(BYTES_PER_TOKEN)
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
    // 检查是否启用远程（以密钥为准，地址缺省回落官方）
    if let Some(config) = get_config() {
        if let Some(api_url) = resolve_remote(&config) {
            // 尝试调用远程 API
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(call_remote_count_tokens(
                    &api_url, &config, model, &system, &messages, &tools,
                ))
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!(
                        source = "remote",
                        api_url = %api_url,
                        tokens,
                        "count_tokens(input) 走远程"
                    );
                    return tokens;
                }
                Err(e) => {
                    tracing::warn!(
                        source = "local_fallback",
                        api_url = %api_url,
                        error = %e,
                        "count_tokens(input) 远程失败，回退本地启发式"
                    );
                }
            }
        }
    }

    // 本地计算
    let tokens = count_all_tokens_local(system, messages, tools);
    tracing::debug!(source = "local", tokens, "count_tokens(input) 走本地启发式");
    tokens
}

/// 把 messages 净化成 count_tokens 能接受的形状（仅用于计数，不影响真实请求）。
///
/// **thinking block → 同内容的 text block。**
///
/// history 里的 thinking block 是上游 Kiro 产生的，其 `signature` 由 Kiro 签发
/// （见 `kiro::model::events::reasoning`）。Anthropic 会真的校验该签名，而它不可能通过
/// ——实测伪签名、空签名、连 `signature` 字段都不带，三种全是
/// 400 `Invalid signature in thinking block`；只有换成 text block 才被接受。
/// 这不是签名值填错，是签发方就不对，无解。
///
/// 为何不直接删掉 thinking block：那部分内容**上游是照收 token 的**（thinking 计入
/// contextUsage），删了会少算整段思考的量（agentic 会话里可达数千 token）。转成 text
/// 保留了内容，只在 block 类型标记上有几 token 的出入，比整段漏掉准得多。
///
/// 只在开了 thinking 的会话里才有 thinking block，其余请求走这里零开销（不克隆）。
fn sanitize_for_count(messages: &[Message]) -> Vec<Message> {
    let needs_fix = messages.iter().any(|m| {
        m.content
            .as_array()
            .is_some_and(|bs| bs.iter().any(|b| is_thinking_block(b)))
    });
    if !needs_fix {
        return messages.to_vec();
    }
    messages
        .iter()
        .map(|m| {
            let Some(blocks) = m.content.as_array() else {
                return m.clone();
            };
            let converted: Vec<serde_json::Value> = blocks
                .iter()
                .map(|b| {
                    if !is_thinking_block(b) {
                        return b.clone();
                    }
                    // thinking 用 `thinking` 字段承载文本，redacted_thinking 用 `data`
                    // （已加密，长度不代表 token 数，但保留总比丢弃接近）。
                    let text = b
                        .get("thinking")
                        .or_else(|| b.get("data"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    serde_json::json!({ "type": "text", "text": text })
                })
                // 空 text block 会被 API 拒（同 system 那条约束），顺手滤掉。
                .filter(|b| {
                    b.get("text").and_then(|v| v.as_str()).is_none_or(|t| !t.is_empty())
                })
                .collect();
            Message {
                role: m.role.clone(),
                content: serde_json::Value::Array(converted),
            }
        })
        // 净化后内容为空的 message 会被 API 拒，滤掉。
        .filter(|m| !m.content.as_array().is_some_and(|b| b.is_empty()))
        .collect()
}

fn is_thinking_block(b: &serde_json::Value) -> bool {
    matches!(
        b.get("type").and_then(|v| v.as_str()),
        Some("thinking") | Some("redacted_thinking")
    )
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

    // 构建请求体。messages 需先净化：history 里的 thinking block 带的是**上游 Kiro 的
    // 签名**，Anthropic 会真的去验它、必然不过（实测伪签名/空签名/不带 signature 字段
    // 全部 400 `Invalid signature in thinking block`）。见 [`sanitize_for_count`]。
    let request = CountTokensRequest {
        model: model, // 模型名称用于 token 计算
        messages: sanitize_for_count(messages),
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

    // 发送请求。anthropic-version 为官方 API 必填头，缺失一律 400。
    let response = req_builder
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        // 带上响应体：官方把真正的原因放在 body 里（余额不足 / org 停用 / 请求体不合法
        // 各自的 message 完全不同），只报状态码等于把诊断信息丢掉，排查时只能靠猜。
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let body = body.trim();
        // 截断防止超长 body 灌爆日志。
        let brief: String = body.chars().take(300).collect();
        return Err(format!("API 返回错误状态: {} — {}", status, brief).into());
    }

    let result: CountTokensResponse = response.json().await?;
    Ok(result.input_tokens as u64)
}

/// 远程计数结果的本地缓存：内容 hash → 官方 token 数。
///
/// 同一段 system 前缀 / tools 定义在会话里反复出现，其官方 token 数是内容的纯函数，
/// 算一次即可复用。缓存分段重算尤其吃这个：cache_read 段按定义就是「没变过的前缀」，
/// 每轮都会以同样内容再数一次。命中即省一次跨洋往返，也直接压低 count_tokens 的 RPM
/// （该端点不返回任何 `anthropic-ratelimit-*` 头，触限只会静默 429 → 回退本地）。
/// 满则整体清空（非 LRU）：条目是纯函数结果、无过期语义，清空只是丢失一轮预热，
/// 不会算错；换取零依赖与读路径上的 `read()` 无写锁竞争。
static REMOTE_COUNT_CACHE: RwLock<Option<std::collections::HashMap<u64, u64>>> =
    RwLock::new(None);

/// 远程计数缓存容量（条）。key/value 各 8 字节，几千条也就几十 KB。
const REMOTE_COUNT_CACHE_CAP: usize = 4096;

fn remote_count_cache_get(key: u64) -> Option<u64> {
    REMOTE_COUNT_CACHE.read().as_ref()?.get(&key).copied()
}

fn remote_count_cache_put(key: u64, tokens: u64) {
    let mut guard = REMOTE_COUNT_CACHE.write();
    let cache = guard.get_or_insert_with(Default::default);
    if cache.len() >= REMOTE_COUNT_CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, tokens);
}

/// 计数缓存的 key：模型 + 请求体内容的 hash（模型不同分词不同，必须入 key）。
fn remote_count_cache_key(model: &str, body: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    model.hash(&mut h);
    serde_json::to_string(body).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// 对任意 payload 调远程 count_tokens，返回官方 token 数；未启用远程或失败返回 None。
///
/// 结果按内容 hash 缓存（见 [`REMOTE_COUNT_CACHE`]）。调用方拿到 None 时应保留原口径，
/// 不要用本地估算去补——那会造成单位混用。
pub(crate) async fn count_payload_remote(payload: &crate::anthropic::types::MessagesRequest) -> Option<u64> {
    let config = get_config()?;
    let api_url = resolve_remote(&config)?;

    let request = CountTokensRequest {
        model: payload.model.clone(),
        messages: payload.messages.clone(),
        system: payload.system.clone(),
        tools: payload.tools.clone(),
    };
    let body = serde_json::to_value(&request).ok()?;
    let key = remote_count_cache_key(&payload.model, &body);

    if let Some(hit) = remote_count_cache_get(key) {
        tracing::debug!(source = "remote_cached", tokens = hit, "count_tokens(payload) 命中本地缓存");
        return Some(hit);
    }

    match call_remote_count_tokens(
        &api_url,
        &config,
        payload.model.clone(),
        &payload.system,
        &payload.messages,
        &payload.tools,
    )
    .await
    {
        Ok(tokens) => {
            remote_count_cache_put(key, tokens);
            tracing::debug!(source = "remote", api_url = %api_url, tokens, "count_tokens(payload) 走远程");
            Some(tokens)
        }
        Err(e) => {
            tracing::warn!(
                source = "local_fallback",
                api_url = %api_url,
                error = %e,
                "count_tokens(payload) 远程失败"
            );
            None
        }
    }
}

/// 输出计数差分法的哨兵 user message 内容。
///
/// count_tokens 要求 messages 非空且首条为 user，故 assistant 回合前须垫一条。
/// 内容固定，其贡献在差分中被完全消掉，取什么值都不影响结果。
const OUTPUT_COUNT_SENTINEL: &str = "x";

/// `call_remote_count_tokens` 的带缓存包装（按 model + messages 内容 hash）。
///
/// 差分法的基线只与模型有关，每模型实打一次即可；输出文本本身通常各不相同，
/// 缓存对它基本不命中，但也不产生额外成本。
async fn call_remote_count_tokens_cached(
    api_url: &str,
    config: &CountTokensConfig,
    model: &str,
    messages: &Vec<Message>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let key = remote_count_cache_key(
        model,
        &serde_json::to_value(messages).unwrap_or(serde_json::Value::Null),
    );
    if let Some(hit) = remote_count_cache_get(key) {
        return Ok(hit);
    }
    let tokens =
        call_remote_count_tokens(api_url, config, model.to_string(), &None, messages, &None).await?;
    remote_count_cache_put(key, tokens);
    Ok(tokens)
}

/// 计算一段输出文本的 token 数。
///
/// 优先走远程 count_tokens 的**差分法**（见函数体内说明）；未配置或调用失败时回退
/// 本地启发式 `ceil(字节/4)`。空串返回 0，不产生远程调用。
///
/// output **没有**上游 `contextUsageEvent` 真值兜底，此处算出的即最终上报/计费值，
/// 故差分结果还要经 `converter::adjust_output_tokens` 补上生成侧的 per-model 增量。
/// 实测（对照官方生成的真实 output_tokens）：修正后精确命中；回退本地启发式时则为
/// `ceil(字节/4)`，误差随语种浮动。
pub(crate) fn count_output_tokens(model: &str, text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }

    if let Some(config) = get_config() {
        if let Some(api_url) = resolve_remote(&config) {
            // 差分法：数「哨兵 user + assistant(输出文本)」再减去「只有哨兵 user」，
            // 差额即该 assistant message 的 token 数。
            //
            // 为何不直接把输出文本包成 user message 去数：那样返回值含一整条 message 的
            // role/boundary 开销，且该开销随模型而异（实测 count("x") = 7/12/8 三档）。
            // 差分把开销消掉，且按 assistant 角色计数——输出本就是 assistant message，
            // 用 user 角色是错配。实测（对同一段文本，五个模型）差分均得内容 10 token，
            // 而裸数法误差 +4~+7、差分法 −2~−4，误差减半且不引入任何硬编码常数。
            //
            // 残余的 −2~−4 是生成侧独有的量（结束标记等），随模型而异（opus-4.8/sonnet-5
            // 为 2、sonnet-4.6/haiku-4.5 为 3、opus-4.7 为 4），只能靠 per-model 表消除，
            // 故此处不再修正。
            let with_assistant = vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(OUTPUT_COUNT_SENTINEL.to_string()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(text.to_string()),
                },
            ];
            let sentinel_only = vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String(OUTPUT_COUNT_SENTINEL.to_string()),
            }];
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    // 基线只与模型有关，走内容 hash 缓存，每模型实打一次。
                    let (full, base) = tokio::join!(
                        call_remote_count_tokens_cached(
                            &api_url,
                            &config,
                            model,
                            &with_assistant
                        ),
                        call_remote_count_tokens_cached(&api_url, &config, model, &sentinel_only),
                    );
                    match (full, base) {
                        // 差分得内容 token，再补生成侧的 per-model 增量（结束标记等，
                        // 计数接口数不出来，只能查实测表）。
                        (Ok(f), Ok(b)) => Ok(crate::anthropic::adjust_output_tokens(
                            model,
                            f.saturating_sub(b),
                        )),
                        (Err(e), _) | (_, Err(e)) => Err(e),
                    }
                })
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!(
                        source = "remote",
                        api_url = %api_url,
                        tokens,
                        "count_tokens(output) 走远程"
                    );
                    return tokens;
                }
                Err(e) => {
                    tracing::warn!(
                        source = "local_fallback",
                        api_url = %api_url,
                        error = %e,
                        "count_tokens(output) 远程失败，回退本地启发式"
                    );
                }
            }
        }
    }

    let tokens = count_tokens(text);
    tracing::debug!(source = "local", tokens, "count_tokens(output) 走本地启发式");
    tokens
}

/// 每条 message 的结构开销（role token、boundary 分隔符等）。
///
/// 对齐 Anthropic prompt caching 的 `input_tokens` 语义：该字段定义为
/// "既不在 cache_read 也不在 cache_creation 的 tokens"，包含了不进缓存的
/// per-message 结构开销。因此 total_input_tokens 必须比 Σ block.tokens
/// 多出 `n_messages × OVERHEAD`，这样即使客户端把 cache_control 打到
/// prompt 末尾（全缓存场景），uncached 也会等于 overhead 总和而非 0，与
/// 官方返回的 input_tokens 语义一致。
const TOKENS_PER_MESSAGE_OVERHEAD: u64 = 4;

/// 本地计算请求的输入 tokens
///
/// 累加 block 级内容 tokens 后再为每条 message 加上结构 overhead，让
/// `total_input_tokens > cache_tracker 内部累计的 block token 总和`，保证
/// `uncached = total - (read + creation)` 在全缓存场景也 > 0（对齐 Anthropic）。
pub(crate) fn count_all_tokens_local(
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
            total += count_system_message_tokens_stripped(msg);
        }
    }

    for msg in &messages {
        total += count_message_content_tokens(&msg.content);
        total += TOKENS_PER_MESSAGE_OVERHEAD;
    }

    total.max(1)
}

/// 估算输出 tokens（非流式路径）。
///
/// 对输出内容（正文 + tool 入参）计数：配置了 `count_tokens_api_url` 时走远程精确
/// 口径（见 [`count_output_tokens`]），否则用本地启发式 `ceil(utf8_byte_len / 4)`。
/// output **没有**上游 `contextUsageEvent` 真值兜底，此处算出的即最终上报/计费值。
/// 各 block 先拼成整串再一次性计数（与流式 `output_buf` 口径一致：整串切分可避免
/// 逐 block 在 BPE 边界处的系统性偏差，也把远程调用收敛为每响应一次）。
pub(crate) fn estimate_output_tokens(model: &str, content: &[serde_json::Value]) -> i32 {
    let mut buf = String::new();

    for block in content {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            buf.push_str(text);
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            // 工具调用开销
            if let Some(input) = block.get("input") {
                buf.push_str(&serde_json::to_string(input).unwrap_or_default());
            }
        }
    }

    (count_output_tokens(model, &buf) as i32).max(1)
}

/// 计算系统消息的 tokens
pub(crate) fn count_system_message_tokens(message: &SystemMessage) -> u64 {
    count_tokens(&message.text)
}

/// 计算系统消息的 tokens（剥离 billing header 行）。
///
/// 与 cache_tracker 的 `strip_billing_header_line` 对齐：billing header 的
/// cch 字段每次请求都变，cache_tracker 在计算 block token 时已将其剥离，
/// total_input_tokens 也需要同样剥离，否则 uncached 会虚高。
fn count_system_message_tokens_stripped(message: &SystemMessage) -> u64 {
    let filtered: String = message
        .text
        .lines()
        .filter(|line| !line.trim_start().starts_with("x-anthropic-billing-header:"))
        .collect::<Vec<_>>()
        .join("\n");
    count_tokens(&filtered)
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

    /// thinking block 带的是上游 Kiro 的签名，Anthropic 校验必然不过（实测伪签名/
    /// 空签名/无 signature 字段全部 400）。计数前须转成同内容的 text block —— 不能删，
    /// 因为那部分内容上游照收 token。
    #[test]
    fn sanitize_converts_thinking_to_text() {
        let msg = |role: &str, c: serde_json::Value| Message {
            role: role.to_string(),
            content: c,
        };
        let out = sanitize_for_count(&[
            msg("user", serde_json::json!("q")),
            msg(
                "assistant",
                serde_json::json!([
                    {"type":"thinking","thinking":"deep thought","signature":"kiro_sig_xyz"},
                    {"type":"text","text":"answer"}
                ]),
            ),
        ]);
        // 纯字符串 content 原样保留。
        assert_eq!(out[0].content, serde_json::json!("q"));
        // thinking → text，内容保留、签名丢弃；原有 text 不动。
        assert_eq!(
            out[1].content,
            serde_json::json!([
                {"type":"text","text":"deep thought"},
                {"type":"text","text":"answer"}
            ])
        );
    }

    /// redacted_thinking 的文本在 `data` 字段；空块会被 API 拒，须滤掉。
    #[test]
    fn sanitize_handles_redacted_and_empty() {
        let msg = |role: &str, c: serde_json::Value| Message {
            role: role.to_string(),
            content: c,
        };
        let out = sanitize_for_count(&[
            msg("user", serde_json::json!("q")),
            msg(
                "assistant",
                serde_json::json!([
                    {"type":"redacted_thinking","data":"encrypted_blob"},
                    {"type":"thinking","thinking":"","signature":"s"},
                ]),
            ),
        ]);
        // 空 thinking 被滤掉，只剩 redacted 转出来的 text。
        assert_eq!(
            out[1].content,
            serde_json::json!([{"type":"text","text":"encrypted_blob"}])
        );
    }

    /// 无 thinking 的请求不应被改写（也不该白白重建）。
    #[test]
    fn sanitize_is_noop_without_thinking() {
        let m = vec![Message {
            role: "user".to_string(),
            content: serde_json::json!([{"type":"text","text":"hi"}]),
        }];
        let out = sanitize_for_count(&m);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, m[0].content);
    }

    #[test]
    fn count_tokens_empty_is_zero() {
        assert_eq!(count_tokens(""), 0);
    }

    /// 启发式定点：ceil(utf8_byte_len / 4)。
    #[test]
    fn count_tokens_heuristic_fixed_points() {
        // ASCII：1 字节/字符。
        assert_eq!(count_tokens("abcdefgh"), 2); // 8 / 4
        assert_eq!(count_tokens("Hello, world!"), 4); // ceil(13 / 4)
        assert_eq!(count_tokens("abcdefghijklmno"), 4); // ceil(15 / 4)
        // CJK：UTF-8 3 字节/字符。
        assert_eq!(count_tokens("你好世界"), 3); // ceil(12 / 4)
    }

    /// 回归防护：纯函数、确定性，连续两次调用返回相同结果。
    #[test]
    fn count_tokens_deterministic() {
        let a = count_tokens("the quick brown fox jumps over the lazy dog");
        let b = count_tokens("the quick brown fox jumps over the lazy dog");
        assert_eq!(a, b);
        assert!(a > 0);
    }
}
