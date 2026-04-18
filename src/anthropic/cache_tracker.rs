//! Prompt Caching 本地追踪器
//!
//! 通过在代理内部按 prefix fingerprint 记录缓存 checkpoint，
//! 在 Anthropic API 响应的 usage 字段中补上 `cache_creation_input_tokens` /
//! `cache_read_input_tokens`（及 5m / 1h 细分），使客户端能感知命中情况。
//!
//! 上游 Kiro API 不支持 prompt caching，本追踪器纯本地模拟。
//! 支持两种模式（运行时可切换）：
//! - 全局共享：所有凭据共享同一份 checkpoint 表
//! - 按凭据隔离：每个 credential_id 独立维护 checkpoint
//!
//! 对齐 Anthropic 官方 prompt caching 行为：
//! - 仅在显式 `cache_control` 标记处建立 breakpoint（不自动在 message 边界插入）
//! - 最多保留 4 个 breakpoint（超出取最后 4 个）
//! - `input_tokens` = 最后 breakpoint 之后的未缓存 tokens
//! - `total_processed = cache_read + cache_creation + input_tokens`

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::token::{
    count_message_content_tokens, count_system_message_tokens, count_tool_definition_tokens,
};

use super::types::{CacheControl, Message, MessagesRequest};

const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);
const ONE_HOUR_CACHE_TTL: Duration = Duration::from_secs(3600);
const MAX_BREAKPOINTS: usize = 4;
const MAX_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct CacheResult {
    pub cache_read_input_tokens: i32,
    pub cache_creation_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
    /// 最后 breakpoint 之后的未缓存 tokens，对应 Anthropic 返回的 input_tokens
    pub uncached_input_tokens: i32,
}

#[derive(Debug, Clone)]
pub struct CacheProfile {
    total_input_tokens: i32,
    min_cacheable_tokens: i32,
    blocks: Vec<CacheBlock>,
    breakpoints: Vec<CacheBreakpoint>,
}

#[derive(Debug, Clone)]
struct CacheBlock {
    prefix_fingerprint: [u8; 32],
    cumulative_tokens: i32,
}

#[derive(Debug, Clone)]
struct CacheBreakpoint {
    block_index: usize,
    ttl: Duration,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    #[allow(dead_code)]
    token_count: i32,
    ttl: Duration,
    expires_at: Instant,
}

/// 全局模式下使用的固定 credential_id
const GLOBAL_CREDENTIAL_KEY: u64 = 0;

pub struct CacheTracker {
    entries: Mutex<HashMap<u64, HashMap<[u8; 32], CacheEntry>>>,
    max_supported_ttl: Duration,
    global_cache: AtomicBool,
    /// 手动缓存率 override（0.0-1.0）。设置后，cache_read 会强制按
    /// `total_input_tokens * ratio` 计算，其余为 uncached，cache_creation 归零。
    /// 用于 Kiro 不返回真实缓存时，向客户端呈现一个可控的命中率。
    hit_rate_override: Mutex<Option<f32>>,
}

impl CacheTracker {
    pub fn new(
        max_supported_ttl: Duration,
        global_cache: bool,
        hit_rate_override: Option<f32>,
    ) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_supported_ttl,
            global_cache: AtomicBool::new(global_cache),
            hit_rate_override: Mutex::new(hit_rate_override.map(clamp_hit_rate)),
        }
    }

    pub fn is_global_cache(&self) -> bool {
        self.global_cache.load(Ordering::Relaxed)
    }

    pub fn set_global_cache(&self, enabled: bool) {
        self.global_cache.store(enabled, Ordering::Relaxed);
    }

    pub fn hit_rate_override(&self) -> Option<f32> {
        *self.hit_rate_override.lock()
    }

    pub fn set_hit_rate_override(&self, ratio: Option<f32>) {
        *self.hit_rate_override.lock() = ratio.map(clamp_hit_rate);
    }

    fn effective_credential_id(&self, credential_id: u64) -> u64 {
        if self.global_cache.load(Ordering::Relaxed) {
            GLOBAL_CREDENTIAL_KEY
        } else {
            credential_id
        }
    }

    pub fn build_profile(
        &self,
        payload: &MessagesRequest,
        total_input_tokens: i32,
    ) -> CacheProfile {
        let flattened = flatten_cacheable_blocks(payload);

        let request_prelude = canonicalize_json(serde_json::json!({
            "model": payload.model,
            "tool_choice": payload.tool_choice,
        }));
        let prelude_bytes = serde_json::to_vec(&request_prelude).unwrap_or_default();
        let mut prefix_hasher = Sha256::new();
        prefix_hasher.update((prelude_bytes.len() as u64).to_be_bytes());
        prefix_hasher.update(&prelude_bytes);

        let mut blocks = Vec::with_capacity(flattened.len());
        let mut breakpoints = Vec::new();
        let mut cumulative_tokens = 0i32;

        for (index, block) in flattened.into_iter().enumerate() {
            cumulative_tokens = cumulative_tokens.saturating_add(block.tokens);

            let block_bytes = serde_json::to_vec(&block.value).unwrap_or_default();
            let block_hash: [u8; 32] = Sha256::digest(&block_bytes).into();

            let mut next_prefix_hasher = prefix_hasher.clone();
            next_prefix_hasher.update(block_hash);
            let prefix_fingerprint: [u8; 32] = next_prefix_hasher.finalize().into();
            prefix_hasher = Sha256::new();
            prefix_hasher.update(prefix_fingerprint);

            blocks.push(CacheBlock {
                prefix_fingerprint,
                cumulative_tokens,
            });

            if let Some(ttl) = block.breakpoint_ttl {
                let ttl = ttl.min(self.max_supported_ttl);
                breakpoints.push(CacheBreakpoint {
                    block_index: index,
                    ttl,
                });
            }
        }

        // Anthropic 限制最多 4 个 breakpoint，超出时只保留最后 4 个
        if breakpoints.len() > MAX_BREAKPOINTS {
            let start = breakpoints.len() - MAX_BREAKPOINTS;
            breakpoints = breakpoints.split_off(start);
        }

        CacheProfile {
            total_input_tokens: total_input_tokens.max(0),
            min_cacheable_tokens: minimum_cacheable_tokens_for_model(&payload.model),
            blocks,
            breakpoints,
        }
    }

    /// 原子地计算缓存命中并更新 checkpoint 表
    ///
    /// 命中查询模拟 Anthropic 原生行为：缓存点只在显式 `cache_control`
    /// 位置建立（写入），但下次请求无论 breakpoint 打在哪，都能从
    /// 之前建立的缓存位置命中 —— 对应到本实现里即从本次请求的所有
    /// block 前缀指纹（倒序扫描，取最长匹配）中找命中。
    pub fn compute_and_update(&self, credential_id: u64, profile: &CacheProfile) -> CacheResult {
        let effective_id = self.effective_credential_id(credential_id);
        let breakpoints_info: Vec<(usize, i32)> = profile
            .cacheable_breakpoints()
            .iter()
            .map(|bp| (bp.block_index, bp.cumulative_tokens))
            .collect();

        let Some(last_breakpoint) = profile.last_cacheable_breakpoint() else {
            tracing::info!(
                credential_id,
                block_count = profile.blocks.len(),
                breakpoints = ?breakpoints_info,
                total_input_tokens = profile.total_input_tokens,
                "缓存分析：无可缓存 breakpoint，整段未缓存"
            );
            let natural = CacheResult {
                uncached_input_tokens: profile.total_input_tokens,
                ..Default::default()
            };
            return self.apply_hit_rate_override(profile.total_input_tokens, natural);
        };
        let last_breakpoint_tokens = last_breakpoint
            .cumulative_tokens
            .min(profile.total_input_tokens);

        let now = Instant::now();
        let mut all_entries = self.entries.lock();
        prune_expired(&mut all_entries, now);

        let mut matched_tokens = 0;
        let mut matched_block_index: Option<usize> = None;

        if let Some(bucket) = all_entries.get_mut(&effective_id) {
            tracing::debug!(
                credential_id,
                effective_id,
                entry_count = bucket.len(),
                "查找缓存匹配"
            );

            // 对齐 Anthropic：扫描从 last_breakpoint 向前的所有 block，
            // 取最长的匹配 prefix（实际 cache 表内只有 breakpoint 位置的 entry，
            // 所以非 breakpoint 位置的 fingerprint 不会误命中）。
            let last_index = last_breakpoint.block_index;
            for idx in (0..=last_index).rev() {
                let block = &profile.blocks[idx];
                if let Some(entry) = bucket.get_mut(&block.prefix_fingerprint) {
                    if entry.expires_at <= now {
                        continue;
                    }
                    entry.expires_at = now + entry.ttl;
                    matched_tokens =
                        block.cumulative_tokens.min(profile.total_input_tokens);
                    matched_block_index = Some(idx);
                    break;
                }
            }
        } else {
            tracing::debug!(credential_id, effective_id, "首次请求，无缓存条目");
        }

        // 更新 checkpoint 表（在同一个锁范围内）
        let bucket = all_entries.entry(effective_id).or_default();
        for breakpoint in profile.cacheable_breakpoints() {
            let block = &profile.blocks[breakpoint.block_index];
            let next_expiry = now + breakpoint.ttl;

            match bucket.get_mut(&block.prefix_fingerprint) {
                Some(existing) => {
                    existing.token_count = existing.token_count.max(block.cumulative_tokens);
                    existing.ttl = existing.ttl.max(breakpoint.ttl);
                    existing.expires_at = existing.expires_at.max(next_expiry);
                }
                None => {
                    bucket.insert(
                        block.prefix_fingerprint,
                        CacheEntry {
                            token_count: block.cumulative_tokens,
                            ttl: breakpoint.ttl,
                            expires_at: next_expiry,
                        },
                    );
                }
            }
        }

        // 容量淘汰：按过期时间删除最旧的条目
        if bucket.len() > MAX_ENTRIES {
            let mut sorted: Vec<_> = bucket
                .iter()
                .map(|(k, v)| (*k, v.expires_at))
                .collect();
            sorted.sort_by_key(|(_, expires)| *expires);
            let to_remove = bucket.len() - MAX_ENTRIES;
            for (key, _) in sorted.into_iter().take(to_remove) {
                bucket.remove(&key);
            }
        }

        let cache_read = matched_tokens.max(0);
        let cache_creation = last_breakpoint_tokens.saturating_sub(matched_tokens).max(0);
        let (cache_5m, cache_1h) = compute_ttl_breakdown(profile, matched_tokens);

        let uncached = profile
            .total_input_tokens
            .saturating_sub(cache_read)
            .saturating_sub(cache_creation)
            .max(0);

        tracing::info!(
            credential_id,
            block_count = profile.blocks.len(),
            breakpoints = ?breakpoints_info,
            matched_block_index = ?matched_block_index,
            matched_cumulative = matched_tokens,
            last_breakpoint_block_index = last_breakpoint.block_index,
            last_breakpoint_cumulative = last_breakpoint.cumulative_tokens,
            total_input_tokens = profile.total_input_tokens,
            cache_read,
            cache_creation,
            uncached,
            cache_5m,
            cache_1h,
            "缓存计算结果"
        );

        let natural = CacheResult {
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
            cache_creation_5m_input_tokens: cache_5m,
            cache_creation_1h_input_tokens: cache_1h,
            uncached_input_tokens: uncached,
        };
        self.apply_hit_rate_override(profile.total_input_tokens, natural)
    }

    /// 应用手动缓存率 override。
    ///
    /// 固定比率语义：`cache_read = total * ratio`，`uncached = total - read`，
    /// `cache_creation*` 清零。Kiro 不返回真实缓存时，由此提供稳定可控的命中率。
    fn apply_hit_rate_override(&self, total_input_tokens: i32, natural: CacheResult) -> CacheResult {
        let Some(ratio) = self.hit_rate_override() else {
            return natural;
        };
        if total_input_tokens <= 0 {
            return natural;
        }
        let ratio = clamp_hit_rate(ratio);
        let cache_read = ((total_input_tokens as f32) * ratio).round() as i32;
        let cache_read = cache_read.clamp(0, total_input_tokens);
        let uncached = total_input_tokens - cache_read;
        tracing::info!(
            ratio,
            total_input_tokens,
            overridden_cache_read = cache_read,
            overridden_uncached = uncached,
            "应用手动缓存率 override"
        );
        CacheResult {
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            uncached_input_tokens: uncached,
        }
    }
}

fn clamp_hit_rate(ratio: f32) -> f32 {
    if ratio.is_nan() {
        0.0
    } else {
        ratio.clamp(0.0, 1.0)
    }
}

fn compute_ttl_breakdown(profile: &CacheProfile, matched_tokens: i32) -> (i32, i32) {
    let Some(last_breakpoint) = profile.last_cacheable_breakpoint() else {
        return (0, 0);
    };

    let new_tokens = last_breakpoint
        .cumulative_tokens
        .min(profile.total_input_tokens)
        .saturating_sub(matched_tokens)
        .max(0);

    if new_tokens == 0 {
        return (0, 0);
    }

    if last_breakpoint.ttl == ONE_HOUR_CACHE_TTL {
        (0, new_tokens)
    } else {
        (new_tokens, 0)
    }
}

impl CacheProfile {
    fn cacheable_breakpoints(&self) -> Vec<ResolvedBreakpoint> {
        self.breakpoints
            .iter()
            .filter_map(|breakpoint| {
                let block = self.blocks.get(breakpoint.block_index)?;
                if block.cumulative_tokens < self.min_cacheable_tokens {
                    return None;
                }

                Some(ResolvedBreakpoint {
                    block_index: breakpoint.block_index,
                    cumulative_tokens: block.cumulative_tokens,
                    ttl: breakpoint.ttl,
                })
            })
            .collect()
    }

    fn last_cacheable_breakpoint(&self) -> Option<ResolvedBreakpoint> {
        self.cacheable_breakpoints().into_iter().last()
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolvedBreakpoint {
    block_index: usize,
    cumulative_tokens: i32,
    ttl: Duration,
}

#[derive(Debug)]
struct PendingBlock {
    value: serde_json::Value,
    tokens: i32,
    breakpoint_ttl: Option<Duration>,
}

fn flatten_cacheable_blocks(payload: &MessagesRequest) -> Vec<PendingBlock> {
    let mut blocks = Vec::new();

    if let Some(tools) = &payload.tools {
        for (tool_index, tool) in tools.iter().enumerate() {
            let mut value = serde_json::to_value(tool).unwrap_or(serde_json::Value::Null);
            let breakpoint_ttl = extract_cache_ttl(&value);
            strip_cache_control(&mut value);

            blocks.push(PendingBlock {
                value: canonicalize_json(serde_json::json!({
                    "kind": "tool",
                    "tool_index": tool_index,
                    "tool": value,
                })),
                tokens: count_tool_definition_tokens(tool) as i32,
                breakpoint_ttl,
            });
        }
    }

    if let Some(system) = &payload.system {
        for (system_index, block) in system.iter().enumerate() {
            let mut value = serde_json::to_value(block).unwrap_or(serde_json::Value::Null);
            let breakpoint_ttl = extract_cache_ttl(&value);
            strip_cache_control(&mut value);

            blocks.push(PendingBlock {
                value: canonicalize_json(serde_json::json!({
                    "kind": "system",
                    "system_index": system_index,
                    "block": value,
                })),
                tokens: count_system_message_tokens(block) as i32,
                breakpoint_ttl,
            });
        }
    }

    for (message_index, message) in payload.messages.iter().enumerate() {
        blocks.extend(flatten_message_blocks(message_index, message));
    }

    blocks
}

fn flatten_message_blocks(message_index: usize, message: &Message) -> Vec<PendingBlock> {
    match &message.content {
        serde_json::Value::String(text) => vec![build_message_block(
            message_index,
            &message.role,
            0,
            serde_json::json!({
                "type": "text",
                "text": text,
            }),
            None,
        )],
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .enumerate()
            .map(|(block_index, block)| {
                let breakpoint_ttl = extract_cache_ttl(block);
                let mut normalized = block.clone();
                strip_cache_control(&mut normalized);
                build_message_block(
                    message_index,
                    &message.role,
                    block_index,
                    normalized,
                    breakpoint_ttl,
                )
            })
            .collect(),
        other => vec![build_message_block(
            message_index,
            &message.role,
            0,
            other.clone(),
            None,
        )],
    }
}

fn build_message_block(
    message_index: usize,
    role: &str,
    block_index: usize,
    block: serde_json::Value,
    breakpoint_ttl: Option<Duration>,
) -> PendingBlock {
    PendingBlock {
        tokens: count_message_content_tokens(&block) as i32,
        value: canonicalize_json(serde_json::json!({
            "kind": "message",
            "message_index": message_index,
            "role": role,
            "block_index": block_index,
            "block": block,
        })),
        breakpoint_ttl,
    }
}

fn extract_cache_ttl(value: &serde_json::Value) -> Option<Duration> {
    let cache_control = value.get("cache_control")?;
    let cache_control: CacheControl = serde_json::from_value(cache_control.clone()).ok()?;
    if cache_control.cache_type != "ephemeral" {
        return None;
    }

    Some(match cache_control.ttl.as_deref() {
        Some("1h") => ONE_HOUR_CACHE_TTL,
        _ => DEFAULT_CACHE_TTL,
    })
}

fn strip_cache_control(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(arr) => {
            for item in arr {
                strip_cache_control(item);
            }
        }
        serde_json::Value::Object(map) => {
            map.remove("cache_control");
            for item in map.values_mut() {
                strip_cache_control(item);
            }
        }
        _ => {}
    }
}

/// 对齐 Anthropic 官方 prompt caching 最小可缓存 tokens
/// 参考: https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching
fn minimum_cacheable_tokens_for_model(model: &str) -> i32 {
    let m = model.to_lowercase();

    // 4096 tokens: Opus 4.5+, Haiku 4.5, Haiku 3, Mythos Preview
    if m.contains("mythos")
        || m.contains("opus-4-5")
        || m.contains("opus-4.5")
        || m.contains("opus-4-6")
        || m.contains("opus-4.6")
        || m.contains("opus-4-7")
        || m.contains("opus-4.7")
        || m.contains("haiku-4-5")
        || m.contains("haiku-4.5")
        || m.contains("haiku_4_5")
        || m.contains("haiku_4.5")
    {
        return 4096;
    }

    // 2048 tokens: Sonnet 4.6, Haiku 3.5
    if m.contains("sonnet-4-6")
        || m.contains("sonnet-4.6")
        || m.contains("sonnet_4_6")
        || m.contains("haiku-3-5")
        || m.contains("haiku-3.5")
        || m.contains("haiku_3_5")
        || m.contains("haiku_3.5")
    {
        return 2048;
    }

    // 1024 tokens: Opus 4/4.1, Sonnet 3.5/3.7/4/4.5
    if m.contains("opus") || m.contains("sonnet") {
        return 1024;
    }

    // 未知 haiku 版本按 2048 兜底（最常见的 3.5）
    if m.contains("haiku") {
        return 2048;
    }

    1024
}

fn prune_expired(entries: &mut HashMap<u64, HashMap<[u8; 32], CacheEntry>>, now: Instant) {
    entries.retain(|_, bucket| {
        bucket.retain(|_, entry| entry.expires_at > now);
        !bucket.is_empty()
    });
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(map) => {
            let ordered: BTreeMap<_, _> = map
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect();

            let mut out = serde_json::Map::new();
            for (key, value) in ordered {
                out.insert(key, value);
            }
            serde_json::Value::Object(out)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
    use serde_json::json;

    const LARGE_SYSTEM_CHARS: usize = 20_000; // 约 5k tokens（按 ~4 字符/token 估算，超过 sonnet-4.6 的 2048 门槛）

    fn tracker() -> CacheTracker {
        CacheTracker::new(DEFAULT_CACHE_TTL, true, None)
    }

    fn tracker_per_credential() -> CacheTracker {
        CacheTracker::new(DEFAULT_CACHE_TTL, false, None)
    }

    fn large_text(prefix: &str, size: usize) -> String {
        let mut s = String::with_capacity(size + prefix.len());
        s.push_str(prefix);
        while s.len() < size {
            s.push_str(" lorem ipsum dolor sit amet");
        }
        s
    }

    fn ephemeral_5m() -> CacheControl {
        CacheControl {
            cache_type: "ephemeral".to_string(),
            ttl: None,
        }
    }

    fn user_message(blocks: Vec<serde_json::Value>) -> Message {
        Message {
            role: "user".to_string(),
            content: serde_json::Value::Array(blocks),
        }
    }

    fn assistant_text(text: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: json!([{ "type": "text", "text": text }]),
        }
    }

    fn build_request(
        system_text: &str,
        messages: Vec<Message>,
    ) -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            messages,
            stream: false,
            system: Some(vec![SystemMessage {
                text: system_text.to_string(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    /// 第二轮请求的用户 breakpoint 指纹 ≠ 第一轮，但前缀（tools+system+第一轮 user+assistant）
    /// 在第二轮的 blocks 中同样存在，应命中 cache_read。
    #[test]
    fn multi_turn_conversation_hits_cache_read() {
        let tracker = tracker();
        let credential_id = 42;

        let system = large_text("SYSTEM ", LARGE_SYSTEM_CHARS);
        let turn1_user = user_message(vec![
            json!({
                "type": "text",
                "text": "first user message",
                "cache_control": ephemeral_5m(),
            }),
        ]);
        let req1 = build_request(&system, vec![turn1_user.clone()]);
        // 第一轮：token 数随便给个覆盖 min_cacheable（由内部重新估算）
        let profile1 = tracker.build_profile(&req1, 10_000);
        let r1 = tracker.compute_and_update(credential_id, &profile1);
        assert_eq!(r1.cache_read_input_tokens, 0, "第一轮应全部 creation");
        assert!(
            r1.cache_creation_input_tokens > 0,
            "第一轮应建立缓存，got={:?}",
            r1
        );

        // 第二轮：前缀保留第一轮的 user + assistant，再追加新 user（打新 breakpoint）
        let assistant = assistant_text("assistant reply");
        let turn2_user = user_message(vec![
            json!({
                "type": "text",
                "text": "second user message",
                "cache_control": ephemeral_5m(),
            }),
        ]);
        let req2 = build_request(
            &system,
            vec![turn1_user, assistant, turn2_user],
        );
        let profile2 = tracker.build_profile(&req2, 12_000);
        let r2 = tracker.compute_and_update(credential_id, &profile2);

        assert!(
            r2.cache_read_input_tokens > 0,
            "第二轮应命中第一轮的前缀缓存，cache_read={}, full={:?}",
            r2.cache_read_input_tokens,
            r2
        );
        // 命中 token 应等于第一轮最后 breakpoint 的累计
        assert_eq!(
            r2.cache_read_input_tokens, r1.cache_creation_input_tokens,
            "cache_read 应等于第一轮 creation（=第一轮 breakpoint 累计）"
        );
        assert!(
            r2.cache_creation_input_tokens > 0,
            "第二轮仍会给新 breakpoint 建缓存"
        );
    }

    /// 第一轮无任何 breakpoint：不写表，也不报 creation。
    #[test]
    fn no_breakpoint_no_cache_activity() {
        let tracker = tracker();
        let req = build_request(
            "small system",
            vec![user_message(vec![json!({
                "type": "text",
                "text": "no cache markers",
            })])],
        );
        let profile = tracker.build_profile(&req, 5_000);
        let r = tracker.compute_and_update(7, &profile);
        assert_eq!(r.cache_read_input_tokens, 0);
        assert_eq!(r.cache_creation_input_tokens, 0);
        assert_eq!(r.uncached_input_tokens, 5_000);
    }

    /// 两次请求完全相同：第二次应 cache_read 全量。
    #[test]
    fn identical_requests_fully_cached_on_second() {
        let tracker = tracker();
        let cred = 1;
        let system = large_text("S ", LARGE_SYSTEM_CHARS);
        let user = user_message(vec![json!({
            "type": "text",
            "text": "same text",
            "cache_control": ephemeral_5m(),
        })]);
        let req = build_request(&system, vec![user]);

        let p1 = tracker.build_profile(&req, 10_000);
        let r1 = tracker.compute_and_update(cred, &p1);
        assert_eq!(r1.cache_read_input_tokens, 0);
        let creation = r1.cache_creation_input_tokens;
        assert!(creation > 0);

        let p2 = tracker.build_profile(&req, 10_000);
        let r2 = tracker.compute_and_update(cred, &p2);
        assert_eq!(
            r2.cache_read_input_tokens, creation,
            "完全相同的请求第二次应命中全部前缀"
        );
        assert_eq!(r2.cache_creation_input_tokens, 0);
    }

    /// 全局模式：不同 credential 共享缓存。
    #[test]
    fn cache_shared_across_credentials_in_global_mode() {
        let tracker = tracker(); // global = true
        let system = large_text("S ", LARGE_SYSTEM_CHARS);
        let req = build_request(
            &system,
            vec![user_message(vec![json!({
                "type": "text",
                "text": "hello",
                "cache_control": ephemeral_5m(),
            })])],
        );
        let p = tracker.build_profile(&req, 10_000);
        let r1 = tracker.compute_and_update(1, &p);
        assert!(r1.cache_creation_input_tokens > 0);

        let p2 = tracker.build_profile(&req, 10_000);
        let r2 = tracker.compute_and_update(2, &p2);
        assert_eq!(
            r2.cache_read_input_tokens, r1.cache_creation_input_tokens,
            "全局模式下 credential 2 应能命中 credential 1 建立的缓存"
        );
        assert_eq!(r2.cache_creation_input_tokens, 0);
    }

    /// 按凭据隔离模式：不同 credential 互不影响。
    #[test]
    fn cache_isolated_between_credentials_in_per_credential_mode() {
        let tracker = tracker_per_credential(); // global = false
        let system = large_text("S ", LARGE_SYSTEM_CHARS);
        let req = build_request(
            &system,
            vec![user_message(vec![json!({
                "type": "text",
                "text": "hello",
                "cache_control": ephemeral_5m(),
            })])],
        );
        let p = tracker.build_profile(&req, 10_000);
        tracker.compute_and_update(1, &p);

        let p2 = tracker.build_profile(&req, 10_000);
        let r = tracker.compute_and_update(2, &p2);
        assert_eq!(
            r.cache_read_input_tokens, 0,
            "按凭据隔离模式下 credential 2 应看不到 credential 1 的缓存"
        );
    }
}
