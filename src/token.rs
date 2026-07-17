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
use reqwest::Client;

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
    // proxy / tls_backend 只在此处设定，重置缓存的 client 让下次按新配置重建。
    *COUNT_CLIENT.write() = None;
}

/// count_tokens 专用 HTTP client 缓存。
///
/// 每次 count 都 `build_client` 会另建连接池、重做 TLS 握手（跨洋握手是 2–3 个 RTT），
/// 400 RPM 下这些握手既压端点又烧本地 CPU。client 只依赖 `proxy` + `tls_backend`——二者仅在
/// [`init_config`] 设定、`update_config` 不动（只改 url/key/auth_type），故整进程复用一个即可。
/// `reqwest::Client` 内部 Arc 共享连接池，clone 便宜。首建竞态无害（后写者胜，两者都是合法 client）。
static COUNT_CLIENT: RwLock<Option<Client>> = RwLock::new(None);

/// 取（或懒建）count 专用 client。构建失败不缓存，下次重试。
fn count_http_client(config: &CountTokensConfig) -> anyhow::Result<Client> {
    if let Some(c) = COUNT_CLIENT.read().clone() {
        return Ok(c);
    }
    let client = build_client(config.proxy.as_ref(), 300, config.tls_backend)?;
    *COUNT_CLIENT.write() = Some(client.clone());
    Ok(client)
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
                    true, // 输入总量是单个独立数，可压平兜底
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

/// 官方口径输入计数的**异步、带缓存、带 sanitize** 版本。
///
/// 供把官方 count 从首字关键路径挪到「与生成并发、收尾取回」的调用方使用（见
/// `anthropic::handlers`）。与同步的 [`count_all_tokens`] 远程分支口径完全一致：
/// - 走 [`call_remote_count_tokens`]，因此对历史 thinking block 做了同样的 `sanitize_for_count`
///   （不 sanitize 会撞上 Anthropic 校验 Kiro 签名的 400，见 [`sanitize_for_count`]）——
///   这也是**不能**改用 [`count_payload_remote`] 的原因（后者不 sanitize）。
/// - 结果按内容 hash 进 [`REMOTE_COUNT_CACHE`]，相同前缀不重复跨洋。
///
/// 未启用远程或调用失败返回 `None`；调用方应保留本地预估作预备值，不要混用口径。
pub(crate) async fn count_all_tokens_remote_cached(
    model: String,
    system: &Option<Vec<SystemMessage>>,
    messages: &[Message],
    tools: &Option<Vec<Tool>>,
) -> Option<u64> {
    let config = get_config()?;
    let api_url = resolve_remote(&config)?;

    // 缓存 key 要与 call_remote_count_tokens 实际发出的请求同形（含 sanitize），否则命中错位。
    let request = CountTokensRequest {
        model: model.clone(),
        messages: sanitize_for_count(messages),
        system: system.clone(),
        tools: tools.clone(),
    };
    let body = serde_json::to_value(&request).ok()?;
    let key = remote_count_cache_key(&model, &body);
    if let Some(hit) = remote_count_cache_get(key) {
        tracing::debug!(source = "remote_cached", tokens = hit, "count_tokens(input) 命中本地缓存（延迟并发）");
        return Some(hit);
    }

    let messages = messages.to_vec();
    // 输入总量是单个独立数，允许压平兜底（结构性 400 不至于退回本地启发式）。
    match call_remote_count_tokens(&api_url, &config, model, system, &messages, tools, true).await {
        Ok(tokens) => {
            remote_count_cache_put(key, tokens);
            tracing::debug!(source = "remote", api_url = %api_url, tokens, "count_tokens(input) 走远程（延迟并发）");
            Some(tokens)
        }
        Err(e) => {
            tracing::warn!(
                source = "local_fallback",
                api_url = %api_url,
                error = %e,
                "count_tokens(input) 远程失败（延迟并发），调用方保留本地预估"
            );
            None
        }
    }
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
/// 始终执行（count 本就是网络重活，多一次 messages clone 可忽略）：
/// - **thinking/redacted_thinking → 同内容 text block**（Kiro 签名过不了 Anthropic 校验，见上）。
/// - **丢弃空 text block（`text==""`）与净化后为空的 message**（否则 400
///   `text content blocks must be non-empty`；客户端历史里的空块、以及下方 trim 裁空的块都在此滤掉）。
/// - **裁掉末条 assistant 的尾随空白**（否则 400 `final assistant content cannot end with
///   trailing whitespace`——Anthropic 把末条 assistant 当待续写 prefill）。
///
/// 内容本就干净时输出与输入等价（无块被改/删）。
fn sanitize_for_count(messages: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = messages
        .iter()
        .filter_map(|m| {
            let Some(blocks) = m.content.as_array() else {
                // 字符串 content 原样保留（空串极少见，交由端点/回退处理）。
                return Some(m.clone());
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
                // 空 text block 会被 API 拒，滤掉（非 text 块如 tool_use 保留）。
                .filter(|b| {
                    b.get("text").and_then(|v| v.as_str()).is_none_or(|t| !t.is_empty())
                })
                .collect();
            // 净化后内容为空的 message 会被 API 拒，丢掉整条。
            if converted.is_empty() {
                return None;
            }
            Some(Message {
                role: m.role.clone(),
                content: serde_json::Value::Array(converted),
            })
        })
        .collect();
    trim_trailing_assistant_whitespace(&mut out);
    out
}

/// 去掉**末条 assistant 消息**内容的尾随空白（仅用于计数，不影响真实请求）。
///
/// Anthropic 把最后一条 assistant 消息当作待续写的 prefill，`messages` 与
/// `count_tokens` 端点都拒绝其内容以空白结尾（400 `final assistant content cannot
/// end with trailing whitespace`）。这在两种 count payload 上会撞到：客户端带
/// assistant prefill、以及 recount 把前缀切在 assistant/tool 边界上时。真实上游请求
/// 走 Kiro 不受此限，故只在送去计数前就地裁掉尾随空白。
///
/// content 为字符串则整体 `trim_end`；为块数组则裁最后一个含 `text` 的块，**裁成空则删掉
/// 该块**（否则留下空 text block 反而触发另一条 400）。末条 assistant 恰为纯空白这种
/// 极端情形（删空块后 message 可能空）不再特殊处理——极罕见，交由调用方回退本地。
fn trim_trailing_assistant_whitespace(messages: &mut [Message]) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    if last.role != "assistant" {
        return;
    }
    match &mut last.content {
        serde_json::Value::String(s) => {
            let trimmed = s.trim_end();
            if trimmed.len() != s.len() {
                *s = trimmed.to_string();
            }
        }
        serde_json::Value::Array(blocks) => {
            if let Some(idx) = blocks
                .iter()
                .rposition(|b| b.get("text").and_then(|v| v.as_str()).is_some())
            {
                if let Some(t) = blocks[idx].get("text").and_then(|v| v.as_str()) {
                    let trimmed = t.trim_end().to_string();
                    if trimmed.is_empty() {
                        blocks.remove(idx);
                    } else if trimmed.len() != t.len() {
                        blocks[idx]["text"] = serde_json::Value::String(trimmed);
                    }
                }
            }
        }
        _ => {}
    }
}

fn is_thinking_block(b: &serde_json::Value) -> bool {
    matches!(
        b.get("type").and_then(|v| v.as_str()),
        Some("thinking") | Some("redacted_thinking")
    )
}

/// 把任意内容 Value 里的**文本信号**追加进 buf（用于 count 结构性失败后的压平兜底）。
///
/// 与 [`count_message_content_tokens`] 同构地取 text / thinking / tool_use 入参 / 嵌套
/// content / redacted data，**天然跳过** count_tokens 会拒的结构字段（thinking 签名、
/// tool_use_id、search_result 的 encrypted_content、图片 base64 等）——正是这些字段触发
/// 各类 400，压平后一并甩掉。
fn append_content_text(buf: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            buf.push_str(s);
            buf.push(' ');
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                append_content_text(buf, v);
            }
        }
        serde_json::Value::Object(obj) => {
            if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                buf.push_str(t);
                buf.push(' ');
            } else if let Some(t) = obj.get("thinking").and_then(|v| v.as_str()) {
                buf.push_str(t);
                buf.push(' ');
            } else if let Some(input) = obj.get("input") {
                buf.push_str(&serde_json::to_string(input).unwrap_or_default());
                buf.push(' ');
            } else if let Some(content) = obj.get("content") {
                append_content_text(buf, content);
            } else if let Some(t) = obj.get("data").and_then(|v| v.as_str()) {
                buf.push_str(t);
                buf.push(' ');
            }
        }
        _ => {}
    }
}

/// count_tokens 结构性 400 的兜底 payload：把整段对话的文本压进**单条 user 消息**。
///
/// count_tokens 按发消息规则严格校验（工具配对、加密 search_result、directive 消息、
/// 截断历史的孤儿 tool_result……），而真实请求走 Kiro 不受这些限制。逐条去满足是打地鼠；
/// 压成一条纯文本 user 消息则**必过**（无任何结构约束），仍由真实分词器计数——丢的是
/// 每消息边界/角色开销（几 token/条），远好过退回 `ceil(字节/4)` 本地启发式。仅在原生
/// payload 被拒后兜底，干净 payload 仍走原生、精度不受影响。空内容返回空（交回退本地）。
fn flatten_messages_for_count(messages: &[Message]) -> Vec<Message> {
    let mut buf = String::new();
    for m in messages {
        append_content_text(&mut buf, &m.content);
    }
    let text = buf.trim().to_string();
    if text.is_empty() {
        return Vec::new();
    }
    vec![Message {
        role: "user".to_string(),
        content: serde_json::Value::String(text),
    }]
}

/// 发一次 count_tokens 请求。错误里带 `is_client_error`（4xx）标志，供调用方判断
/// 是否值得压平兜底重试（4xx=请求体不合法，可救；5xx/网络=重试同样内容无益）。
async fn post_count_tokens(
    client: &Client,
    api_url: &str,
    config: &CountTokensConfig,
    request: &CountTokensRequest,
) -> Result<u64, (bool, Box<dyn std::error::Error + Send + Sync>)> {
    let mut req_builder = client.post(api_url);
    if let Some(api_key) = &config.api_key {
        if config.auth_type == "bearer" {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        } else {
            req_builder = req_builder.header("x-api-key", api_key);
        }
    }
    // anthropic-version 为官方 API 必填头，缺失一律 400。
    let response = req_builder
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(request)
        .send()
        .await
        .map_err(|e| (false, Box::new(e) as Box<dyn std::error::Error + Send + Sync>))?;

    if !response.status().is_success() {
        // 带上响应体：官方把真正的原因放在 body 里（余额不足 / org 停用 / 请求体不合法
        // 各自的 message 完全不同），只报状态码等于把诊断信息丢掉，排查时只能靠猜。
        let status = response.status();
        let is_client_error = status.is_client_error();
        let body = response.text().await.unwrap_or_default();
        // 截断防止超长 body 灌爆日志。
        let brief: String = body.trim().chars().take(300).collect();
        return Err((
            is_client_error,
            format!("API 返回错误状态: {} — {}", status, brief).into(),
        ));
    }

    let result: CountTokensResponse = response
        .json()
        .await
        .map_err(|e| (false, Box::new(e) as Box<dyn std::error::Error + Send + Sync>))?;
    Ok(result.input_tokens as u64)
}

/// 调用远程 count_tokens API。
///
/// 先用净化后的原生 payload 计数（见 [`sanitize_for_count`]）；若遭遇**结构性 4xx**
/// （count_tokens 按发消息规则拒收 thinking 签名/工具配对/加密 search_result/directive/
/// 截断历史等，而真实 Kiro 请求不受限），自动用 [`flatten_messages_for_count`] 压平文本
/// 兜底重算一次——一处根治所有结构性 400，无需逐条打补丁。
///
/// `allow_flatten`：**仅**用于「结果是单个独立数」的调用方（输入总量计数）。凡是要把
/// 多个计数相减的调用方（recount 的 total/前缀相减、output 差分的 full−base）**必须传
/// false**——否则一个走压平、另一个走原生就会**口径混用相减**，把缓存分段/输出量算错。
/// 传 false 时遇结构性 4xx 照旧返回 Err，由调用方整体回退本地（三段同口径自洽）。
async fn call_remote_count_tokens(
    api_url: &str,
    config: &CountTokensConfig,
    model: String,
    system: &Option<Vec<SystemMessage>>,
    messages: &Vec<Message>,
    tools: &Option<Vec<Tool>>,
    allow_flatten: bool,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = count_http_client(config)?;

    let native = CountTokensRequest {
        model: model.clone(),
        messages: sanitize_for_count(messages),
        system: system.clone(),
        tools: tools.clone(),
    };

    match post_count_tokens(&client, api_url, config, &native).await {
        Ok(tokens) => Ok(tokens),
        // 结构性 4xx 且允许压平：压成单条 user 文本重算（system/tools 不变——报错都在
        // messages.N）。压平必过，退化只是丢每消息边界开销。相减类调用方不走这里。
        Err((true, first_err)) if allow_flatten => {
            let flat = flatten_messages_for_count(messages);
            if flat.is_empty() {
                return Err(first_err);
            }
            tracing::debug!(error = %first_err, "count_tokens 原生结构被拒，改用压平文本兜底重算");
            let flat_req = CountTokensRequest {
                model,
                messages: flat,
                system: system.clone(),
                tools: tools.clone(),
            };
            post_count_tokens(&client, api_url, config, &flat_req)
                .await
                .map_err(|(_, e)| e)
        }
        Err((_, e)) => Err(e),
    }
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
        // 不压平：recount 要把 total 与前缀相减，压平/原生混用会算错分段；
        // 遇结构性 400 返回 None，由 recount 整体回退本地（三段同口径自洽）。
        false,
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
    // 不压平：输出差分法要把 full 与 base 相减，口径必须一致；遇 400 返回 Err，
    // 由 count_output_tokens 整体回退本地启发式。
    let tokens =
        call_remote_count_tokens(api_url, config, model.to_string(), &None, messages, &None, false)
            .await?;
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

    /// 末条 assistant 的尾随空白会让官方 count_tokens 400，需在计数前裁掉。
    #[test]
    fn trim_trailing_assistant_whitespace_cases() {
        let msg = |role: &str, c: serde_json::Value| Message {
            role: role.to_string(),
            content: c,
        };

        // 字符串 content：整体 trim_end。
        let mut m = vec![msg("user", serde_json::json!("q")), msg("assistant", serde_json::json!("answer \n"))];
        trim_trailing_assistant_whitespace(&mut m);
        assert_eq!(m[1].content, serde_json::json!("answer"));

        // 块数组：裁最后一个含 text 的块，其余块不动。
        let mut m = vec![msg(
            "assistant",
            serde_json::json!([
                {"type": "text", "text": "keep"},
                {"type": "text", "text": "tail  "}
            ]),
        )];
        trim_trailing_assistant_whitespace(&mut m);
        assert_eq!(
            m[0].content,
            serde_json::json!([
                {"type": "text", "text": "keep"},
                {"type": "text", "text": "tail"}
            ])
        );

        // 末条是 user：不动（约束只针对末条 assistant）。
        let mut m = vec![msg("assistant", serde_json::json!("a ")), msg("user", serde_json::json!("u "))];
        trim_trailing_assistant_whitespace(&mut m);
        assert_eq!(m[1].content, serde_json::json!("u "));
        // 前面的 assistant 也不动（只看末条）。
        assert_eq!(m[0].content, serde_json::json!("a "));

        // 无尾随空白：原样。
        let mut m = vec![msg("assistant", serde_json::json!("clean"))];
        trim_trailing_assistant_whitespace(&mut m);
        assert_eq!(m[0].content, serde_json::json!("clean"));
    }

    /// 空 text block（客户端历史里带的，或 trim 裁空的）会被官方 count_tokens 400，
    /// sanitize 需滤掉；净化后为空的 message 整条丢弃；非 text 块（tool_use）保留。
    #[test]
    fn sanitize_drops_empty_text_blocks_and_messages() {
        let msg = |role: &str, c: serde_json::Value| Message {
            role: role.to_string(),
            content: c,
        };
        let out = sanitize_for_count(&[
            msg("user", serde_json::json!("q")),
            msg(
                "assistant",
                serde_json::json!([
                    {"type": "text", "text": ""},
                    {"type": "text", "text": "real"},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]),
            ),
            // 全空块 → 整条 message 丢弃。
            msg("user", serde_json::json!([{"type": "text", "text": ""}])),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, serde_json::json!("q"));
        assert_eq!(
            out[1].content,
            serde_json::json!([
                {"type": "text", "text": "real"},
                {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
            ])
        );
    }

    /// 末条 assistant 的 text 块若整块是空白，trim 应删掉该块而非留下空块。
    #[test]
    fn sanitize_removes_whitespace_only_trailing_assistant_block() {
        let msg = |role: &str, c: serde_json::Value| Message {
            role: role.to_string(),
            content: c,
        };
        let out = sanitize_for_count(&[msg(
            "assistant",
            serde_json::json!([
                {"type": "text", "text": "kept"},
                {"type": "text", "text": "   "}
            ]),
        )]);
        assert_eq!(out[0].content, serde_json::json!([{"type": "text", "text": "kept"}]));
    }

    /// 压平兜底：抽取各类块的文本（text/thinking/tool_use 入参/tool_result 内容），
    /// 跳过 count_tokens 会拒的结构字段（encrypted_content、tool_use_id、签名），压成
    /// 单条 user 文本。用于原生 payload 结构性 400 后必过重算。
    #[test]
    fn flatten_messages_extracts_text_and_skips_structural_fields() {
        let msg = |role: &str, c: serde_json::Value| Message {
            role: role.to_string(),
            content: c,
        };
        let out = flatten_messages_for_count(&[
            msg("user", serde_json::json!("hello")),
            msg(
                "assistant",
                serde_json::json!([
                    {"type": "thinking", "thinking": "ponder", "signature": "kiro_sig"},
                    {"type": "tool_use", "id": "t1", "name": "search", "input": {"q": "rust"}}
                ]),
            ),
            // 孤儿 tool_result + 加密 search_result：结构字段应被跳过、只留文本。
            msg(
                "user",
                serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "search_result", "title": "T",
                         "content": [{"type": "text", "text": "found it"}],
                         "encrypted_content": "OPAQUE_BLOB_DO_NOT_COUNT"}
                    ]}
                ]),
            ),
        ]);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        let text = out[0].content.as_str().unwrap();
        assert!(text.contains("hello"));
        assert!(text.contains("ponder"));
        assert!(text.contains("rust")); // tool_use 入参
        assert!(text.contains("found it")); // search_result 内嵌 text
        // 结构字段一律不进文本（正是它们触发 400）。
        assert!(!text.contains("kiro_sig"));
        assert!(!text.contains("OPAQUE_BLOB"));
        assert!(!text.contains("t1"));
    }

    /// 空/无文本内容压平后为空，交由调用方回退本地。
    #[test]
    fn flatten_messages_empty_is_empty() {
        assert!(flatten_messages_for_count(&[]).is_empty());
        let m = vec![Message {
            role: "assistant".to_string(),
            content: serde_json::json!([{"type": "image", "source": {"data": ""}}]),
        }];
        // image 块无文本信号（source 非 text/thinking/input/content/data 顶层键）→ 空。
        assert!(flatten_messages_for_count(&m).is_empty());
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
