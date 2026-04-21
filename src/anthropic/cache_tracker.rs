//! Prompt Caching 本地追踪器
//!
//! 通过在代理内部按 prefix fingerprint 记录缓存 checkpoint，
//! 在 Anthropic API 响应的 usage 字段中补上 `cache_creation_input_tokens` /
//! `cache_read_input_tokens`（及 5m / 1h 细分），使客户端能感知命中情况。
//!
//! 上游 Kiro API 不支持 prompt caching，本追踪器纯本地模拟。
//! 两种分桶模式（运行时可切换，见 `CacheScope`）：
//! - `Global`：按 `metadata.user_id`（device_id + account_uuid + session_id）
//!   分桶，同一用户身份跨 credential 共享，无 metadata 时退化为共享 bucket
//! - `PerCredential`：在用户身份基础上再按 credential_id 细分，同一用户的
//!   不同凭据也互不共享
//!
//! 两种模式都天然按用户身份隔离，对齐 Anthropic 官方的 per-workspace 隔离。
//!
//! 对齐 Anthropic 官方 prompt caching 行为：
//! - 仅在显式 `cache_control` 标记处建立 breakpoint（不自动在 message 边界插入）
//! - 最多保留 4 个 breakpoint（超出取最后 4 个）
//! - `input_tokens` = 最后 breakpoint 之后的未缓存 tokens
//! - `total_processed = cache_read + cache_creation + input_tokens`

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU8, Ordering};
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
/// Anthropic 官方：命中查找从 breakpoint 往前最多扫 20 个 block（含自身）。
const PREFIX_LOOKBACK_LIMIT: usize = 20;

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
    /// 从 metadata.user_id 提取的用户身份 hash（device_id + account_uuid + session_id），
    /// 用作缓存分桶的 bucket key，对齐 Anthropic 官方 per-workspace 隔离。
    identity_key: Option<u64>,
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
    /// 命中或写入时刷新；容量淘汰按此字段升序删最久未用的 entry。
    last_used_at: Instant,
}

/// 全局模式下使用的固定 credential_id
const GLOBAL_CREDENTIAL_KEY: u64 = 0;

/// 缓存分桶策略。两种模式都先按用户身份（metadata.user_id）分桶，保证不同
/// 用户永远不共享 cache（对齐 Anthropic 官方 per-workspace 隔离）。
///
/// - `Global`：bucket 仅由用户身份决定。同一用户的所有
///   credential 共享 cache；无 metadata 时退化到共享 bucket（key=0）。
/// - `PerCredential`：在用户身份基础上再按 credential_id 细分。同一用户
///   的不同凭据**互不共享** cache，适合想严格按凭据隔离的场景。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheScope {
    Global,
    PerCredential,
}

impl CacheScope {
    fn as_u8(self) -> u8 {
        match self {
            Self::Global => 0,
            Self::PerCredential => 1,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::PerCredential,
            _ => Self::Global,
        }
    }

    /// 从配置字符串解析。未知值映射到 `Global`。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "per_credential" | "percredential" => Self::PerCredential,
            _ => Self::Global,
        }
    }
}

pub struct CacheTracker {
    entries: Mutex<HashMap<u64, HashMap<[u8; 32], CacheEntry>>>,
    max_supported_ttl: Duration,
    /// 分桶模式（运行时可切换）。用 AtomicU8 编码 CacheScope。
    scope: AtomicU8,
    /// 缓存查找跳过率（0.0-1.0）。对每个有 breakpoint 的请求，以此概率
    /// 跳过 cache 查找（当作首次请求，cache_read = 0），但仍正常写入 checkpoint；
    /// 用于在自然命中率偏高时整体降低可观察到的缓存命中率。
    cache_skip_rate: Mutex<Option<f32>>,
}

impl CacheTracker {
    pub fn new(
        max_supported_ttl: Duration,
        scope: CacheScope,
        cache_skip_rate: Option<f32>,
    ) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_supported_ttl,
            scope: AtomicU8::new(scope.as_u8()),
            cache_skip_rate: Mutex::new(cache_skip_rate.map(clamp_skip_rate)),
        }
    }

    pub fn cache_scope(&self) -> CacheScope {
        CacheScope::from_u8(self.scope.load(Ordering::Relaxed))
    }

    pub fn set_cache_scope(&self, scope: CacheScope) {
        self.scope.store(scope.as_u8(), Ordering::Relaxed);
    }

    /// 向后兼容：Global → true，其他 → false。
    pub fn is_global_cache(&self) -> bool {
        matches!(self.cache_scope(), CacheScope::Global)
    }

    /// 向后兼容：true → Global，false → PerCredential（保留历史二态行为）。
    pub fn set_global_cache(&self, enabled: bool) {
        let scope = if enabled {
            CacheScope::Global
        } else {
            CacheScope::PerCredential
        };
        self.set_cache_scope(scope);
    }

    pub fn cache_skip_rate(&self) -> Option<f32> {
        *self.cache_skip_rate.lock()
    }

    pub fn set_cache_skip_rate(&self, rate: Option<f32>) {
        *self.cache_skip_rate.lock() = rate.map(clamp_skip_rate);
    }

    /// 按配置的跳过率掷骰子，决定本次请求是否跳过 cache 查找
    fn should_skip_lookup(&self) -> bool {
        let Some(rate) = self.cache_skip_rate() else {
            return false;
        };
        if rate <= 0.0 {
            return false;
        }
        if rate >= 1.0 {
            return true;
        }
        fastrand::f32() < rate
    }

    fn effective_bucket_key(&self, credential_id: u64, profile: &CacheProfile) -> u64 {
        // 所有模式都先按用户身份分桶；无 identity 时退化为
        // 共享 key（Global）或按 credential 隔离（PerCredential）。
        let identity_key = profile.identity_key.unwrap_or(GLOBAL_CREDENTIAL_KEY);
        match self.cache_scope() {
            CacheScope::Global => identity_key,
            CacheScope::PerCredential => {
                // identity 作为 salt 与 credential_id 混合
                let mut hasher = Sha256::new();
                hasher.update(identity_key.to_be_bytes());
                hasher.update(credential_id.to_be_bytes());
                let hash: [u8; 32] = hasher.finalize().into();
                u64::from_be_bytes([
                    hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
                ])
            }
        }
    }

    pub fn build_profile(
        &self,
        payload: &MessagesRequest,
        total_input_tokens: i32,
    ) -> CacheProfile {
        let flattened = flatten_cacheable_blocks(payload);

        // prelude 只含 model，影响所有段；其它参数按段归入 extras。
        let request_prelude = canonicalize_json(serde_json::json!({
            "model": payload.model,
        }));
        let prelude_bytes = serde_json::to_vec(&request_prelude).unwrap_or_default();
        let mut prefix_hasher = Sha256::new();
        prefix_hasher.update((prelude_bytes.len() as u64).to_be_bytes());
        prefix_hasher.update(&prelude_bytes);

        let tools_extras = compute_segment_extras_hash(payload, BlockSegment::Tools);
        let system_extras = compute_segment_extras_hash(payload, BlockSegment::System);
        let messages_extras = compute_segment_extras_hash(payload, BlockSegment::Messages);

        let mut blocks = Vec::with_capacity(flattened.len());
        let mut breakpoints = Vec::new();
        let mut cumulative_tokens = 0i32;

        for (index, block) in flattened.into_iter().enumerate() {
            cumulative_tokens = cumulative_tokens.saturating_add(block.tokens);

            let block_bytes = serde_json::to_vec(&block.value).unwrap_or_default();
            let block_hash: [u8; 32] = Sha256::digest(&block_bytes).into();

            // content_fingerprint 仅随 block 内容级联（不含 extras），
            // 下一轮的级联也用它，确保 extras 只作用于当前段。
            let mut next_prefix_hasher = prefix_hasher.clone();
            next_prefix_hasher.update(block_hash);
            let content_fingerprint: [u8; 32] = next_prefix_hasher.finalize().into();
            prefix_hasher = Sha256::new();
            prefix_hasher.update(content_fingerprint);

            let segment_extras = match block.segment {
                BlockSegment::Tools => &tools_extras,
                BlockSegment::System => &system_extras,
                BlockSegment::Messages => &messages_extras,
            };
            let effective_fingerprint = mix_fingerprint(&content_fingerprint, segment_extras);

            blocks.push(CacheBlock {
                prefix_fingerprint: effective_fingerprint,
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

        // Anthropic 限制最多 4 个 cache_control breakpoint，超出时 API 返回 400；
        // 本地退化为无缓存以贴近真实失败路径。
        if breakpoints.len() > MAX_BREAKPOINTS {
            tracing::warn!(
                breakpoint_count = breakpoints.len(),
                max = MAX_BREAKPOINTS,
                "cache_control breakpoint 超过 4 个上限，Anthropic 会返回 400，本地按无缓存处理"
            );
            breakpoints.clear();
        }

        // Anthropic 要求 1h breakpoint 必须排在所有 5m breakpoint 之前，
        // 违反时 API 返回 400；本地退化为无缓存以贴近真实失败路径。
        let mut seen_5m = false;
        let mut ttl_violation = false;
        for bp in &breakpoints {
            if bp.ttl == ONE_HOUR_CACHE_TTL && seen_5m {
                ttl_violation = true;
                break;
            }
            if bp.ttl == DEFAULT_CACHE_TTL {
                seen_5m = true;
            }
        }
        if ttl_violation {
            tracing::warn!(
                "cache_control TTL 顺序非法：1h breakpoint 出现在 5m 之后，Anthropic 会返回 400，本地按无缓存处理"
            );
            breakpoints.clear();
        }

        let identity_key = extract_identity_key(payload);

        CacheProfile {
            total_input_tokens: total_input_tokens.max(0),
            min_cacheable_tokens: minimum_cacheable_tokens_for_model(&payload.model),
            blocks,
            breakpoints,
            identity_key,
        }
    }

    /// 原子地计算缓存命中并更新 checkpoint 表
    ///
    /// 命中查询模拟 Anthropic 原生行为：缓存点只在显式 `cache_control`
    /// 位置建立（写入），但下次请求无论 breakpoint 打在哪，都能从
    /// 之前建立的缓存位置命中 —— 对应到本实现里即从本次请求的所有
    /// block 前缀指纹（倒序扫描，取最长匹配）中找命中。
    pub fn compute_and_update(&self, credential_id: u64, profile: &CacheProfile) -> CacheResult {
        let effective_id = self.effective_bucket_key(credential_id, profile);
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
            return CacheResult {
                uncached_input_tokens: profile.total_input_tokens,
                ..Default::default()
            };
        };
        let last_breakpoint_tokens = last_breakpoint
            .cumulative_tokens
            .min(profile.total_input_tokens);

        let now = Instant::now();
        let mut all_entries = self.entries.lock();
        prune_expired(&mut all_entries, now);

        let mut matched_tokens = 0;
        let mut matched_block_index: Option<usize> = None;
        let skipped_lookup = self.should_skip_lookup();

        if skipped_lookup {
            tracing::info!(
                credential_id,
                effective_id,
                skip_rate = ?self.cache_skip_rate(),
                "按配置概率跳过 cache 查找，本次请求按首次请求处理"
            );
        } else if let Some(bucket) = all_entries.get_mut(&effective_id) {
            tracing::debug!(
                credential_id,
                effective_id,
                entry_count = bucket.len(),
                "查找缓存匹配"
            );

            // 对齐 Anthropic：每个 cache_control breakpoint 各自回扫最多
            // 20 个 block（含自身），取跨所有 breakpoint 中最长的匹配 prefix。
            // 先只读扫描锁定最佳 (idx, fingerprint)，再单独 get_mut 更新，
            // 避免在循环中同时持有可变借用。
            let mut best: Option<(usize, [u8; 32], i32)> = None;
            for bp in profile.cacheable_breakpoints() {
                let mut scanned = 0usize;
                for idx in (0..=bp.block_index).rev() {
                    if scanned >= PREFIX_LOOKBACK_LIMIT {
                        break;
                    }
                    scanned += 1;

                    let block = &profile.blocks[idx];
                    let Some(entry) = bucket.get(&block.prefix_fingerprint) else {
                        continue;
                    };
                    if entry.expires_at <= now {
                        continue;
                    }
                    let candidate_tokens =
                        block.cumulative_tokens.min(profile.total_input_tokens);
                    // 同一 bp 内回扫 idx 递减，cumulative_tokens 单调递减，
                    // 第一个命中即该 bp 的最佳匹配；break 去跑下一个 bp。
                    match best {
                        Some((_, _, existing)) if existing >= candidate_tokens => {}
                        _ => {
                            best = Some((idx, block.prefix_fingerprint, candidate_tokens));
                        }
                    }
                    break;
                }
            }

            if let Some((idx, fingerprint, cum_tokens)) = best {
                if let Some(entry) = bucket.get_mut(&fingerprint) {
                    entry.expires_at = now + entry.ttl;
                    entry.last_used_at = now;
                }
                matched_tokens = cum_tokens;
                matched_block_index = Some(idx);
            }
        } else {
            tracing::debug!(credential_id, effective_id, "首次请求，无缓存条目");
        }

        // 更新 checkpoint 表（在同一个锁范围内）。
        // 同位置重复写入时直接覆盖 ttl / expires_at，支持 1h → 5m 的 downgrade。
        let bucket = all_entries.entry(effective_id).or_default();
        for breakpoint in profile.cacheable_breakpoints() {
            let block = &profile.blocks[breakpoint.block_index];
            let next_expiry = now + breakpoint.ttl;

            match bucket.get_mut(&block.prefix_fingerprint) {
                Some(existing) => {
                    existing.token_count = existing.token_count.max(block.cumulative_tokens);
                    existing.ttl = breakpoint.ttl;
                    existing.expires_at = next_expiry;
                    existing.last_used_at = now;
                }
                None => {
                    bucket.insert(
                        block.prefix_fingerprint,
                        CacheEntry {
                            token_count: block.cumulative_tokens,
                            ttl: breakpoint.ttl,
                            expires_at: next_expiry,
                            last_used_at: now,
                        },
                    );
                }
            }
        }

        // 容量淘汰：按 last_used_at 升序删最久未用的条目（LRU）。
        if bucket.len() > MAX_ENTRIES {
            let mut sorted: Vec<_> = bucket
                .iter()
                .map(|(k, v)| (*k, v.last_used_at))
                .collect();
            sorted.sort_by_key(|(_, last_used)| *last_used);
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
            skipped_lookup,
            "缓存计算结果"
        );

        CacheResult {
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
            cache_creation_5m_input_tokens: cache_5m,
            cache_creation_1h_input_tokens: cache_1h,
            uncached_input_tokens: uncached,
        }
    }
}

fn clamp_skip_rate(rate: f32) -> f32 {
    if rate.is_nan() {
        0.0
    } else {
        rate.clamp(0.0, 1.0)
    }
}

/// 按每个 cacheable breakpoint 的 TTL 分段累加 cache_creation。
/// 每个 breakpoint 覆盖 [prev_cum, cum] 区间，已命中的 [0, matched] 部分扣除。
fn compute_ttl_breakdown(profile: &CacheProfile, matched_tokens: i32) -> (i32, i32) {
    let total_limit = profile.total_input_tokens;
    let mut five_min = 0i32;
    let mut one_hour = 0i32;
    let mut prev_cum = 0i32;

    for bp in profile.cacheable_breakpoints() {
        let cum = bp.cumulative_tokens.min(total_limit);
        if cum <= prev_cum {
            continue;
        }
        let segment_start = prev_cum.max(matched_tokens);
        let new_tokens = cum.saturating_sub(segment_start).max(0);
        if new_tokens > 0 {
            if bp.ttl == ONE_HOUR_CACHE_TTL {
                one_hour = one_hour.saturating_add(new_tokens);
            } else {
                five_min = five_min.saturating_add(new_tokens);
            }
        }
        prev_cum = cum;
    }

    (five_min, one_hour)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockSegment {
    Tools,
    System,
    Messages,
}

#[derive(Debug)]
struct PendingBlock {
    value: serde_json::Value,
    tokens: i32,
    breakpoint_ttl: Option<Duration>,
    segment: BlockSegment,
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
                segment: BlockSegment::Tools,
            });
        }
    }

    if let Some(system) = &payload.system {
        for (system_index, block) in system.iter().enumerate() {
            let mut value = serde_json::to_value(block).unwrap_or(serde_json::Value::Null);
            let breakpoint_ttl = extract_cache_ttl(&value);
            strip_cache_control(&mut value);
            strip_billing_header_line(&mut value);

            // token 计数用 strip 后的文本，与 fingerprint 一致；
            // billing header 是 Claude Code 注入的元数据，不属于真实 prompt 内容。
            let tokens = value
                .get("text")
                .and_then(|v| v.as_str())
                .map(|t| crate::token::count_tokens(t) as i32)
                .unwrap_or_else(|| count_system_message_tokens(block) as i32);

            blocks.push(PendingBlock {
                value: canonicalize_json(serde_json::json!({
                    "kind": "system",
                    "system_index": system_index,
                    "block": value,
                })),
                tokens,
                breakpoint_ttl,
                segment: BlockSegment::System,
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
        segment: BlockSegment::Messages,
    }
}

fn extract_cache_ttl(value: &serde_json::Value) -> Option<Duration> {
    let cache_control = value.get("cache_control")?;
    let cache_control: CacheControl = serde_json::from_value(cache_control.clone()).ok()?;
    if cache_control.cache_type != "ephemeral" {
        return None;
    }

    // Anthropic 不允许 thinking / 空 text block 被 cache_control 标记。
    if let Some(block_type) = value.get("type").and_then(|v| v.as_str()) {
        if block_type == "thinking" || block_type == "redacted_thinking" {
            return None;
        }
        if block_type == "text" {
            let text = value.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if text.is_empty() {
                return None;
            }
        }
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

/// 从 system block 的 text 字段中移除 `x-anthropic-billing-header: ...` 行。
/// billing header 的 cch 字段每次请求都变，如果留在 fingerprint 里会导致
/// system prefix 永远无法命中缓存。
fn strip_billing_header_line(value: &mut serde_json::Value) {
    if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
        let filtered: String = text
            .lines()
            .filter(|line| !line.trim_start().starts_with("x-anthropic-billing-header:"))
            .collect::<Vec<_>>()
            .join("\n");
        if filtered.len() != text.len() {
            value["text"] = serde_json::Value::String(filtered);
        }
    }
}

/// 按官方 invalidation 表把请求级参数分类到对应段的 extras hash。
///
/// 参考 https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching
/// 的 "Cache Invalidation Summary"（✘ = 失效，✓ = 保留）：
/// - tool_choice ✓✓✘ / thinking ✓✓✘：只失效 messages
/// - output_config（speed/citations 类）✓✘✘：失效 system + messages
/// - Images ✓✓✘：靠 message block 内容级联天然满足
/// - Tool definitions ✘✘✘：靠 tool block 内容级联天然满足
/// - metadata：不影响 cache（session 级噪声）
fn compute_segment_extras_hash(payload: &MessagesRequest, segment: BlockSegment) -> [u8; 32] {
    let extras = match segment {
        // tools 段保留：只受 tool block 内容变化（级联）影响，不混入任何请求级参数。
        BlockSegment::Tools => serde_json::Value::Null,
        // system 段：受 speed/citations 类（output_config）影响，不受 tool_choice/thinking 影响。
        BlockSegment::System => serde_json::json!({
            "output_config": payload.output_config,
        }),
        // messages 段：受 tool_choice / thinking / output_config 影响。
        BlockSegment::Messages => serde_json::json!({
            "tool_choice": payload.tool_choice,
            "thinking": payload.thinking,
            "output_config": payload.output_config,
        }),
    };
    let bytes = serde_json::to_vec(&canonicalize_json(extras)).unwrap_or_default();
    Sha256::digest(&bytes).into()
}

/// 从 metadata.user_id 提取用户身份并压成 u64 bucket key。
///
/// user_id 支持两种格式：
/// 1. JSON: `{"device_id":"...","account_uuid":"...","session_id":"..."}`
/// 2. 字符串: `user_xxx_account__session_UUID`（fallback 整串 hash）
///
/// 用 device_id + account_uuid + session_id 拼接后 SHA256 取前 8 字节。
/// 这三个字段在同一会话内稳定，不像 billing header 的 cch 每次请求都变。
fn extract_identity_key(payload: &MessagesRequest) -> Option<u64> {
    let user_id = payload.metadata.as_ref()?.user_id.as_ref()?;
    let user_id = user_id.trim();
    if user_id.is_empty() {
        return None;
    }

    let identity_str = if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
        let device_id = json.get("device_id").and_then(|v| v.as_str()).unwrap_or("");
        let account_uuid = json.get("account_uuid").and_then(|v| v.as_str()).unwrap_or("");
        let session_id = json.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        format!("{device_id}\x00{account_uuid}\x00{session_id}")
    } else {
        user_id.to_string()
    };

    let hash: [u8; 32] = Sha256::digest(identity_str.as_bytes()).into();
    Some(u64::from_be_bytes([
        hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
    ]))
}

fn mix_fingerprint(content: &[u8; 32], extras: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hasher.update(extras);
    hasher.finalize().into()
}

/// 对齐 Anthropic 官方 prompt caching 最小可缓存 tokens。
/// 参考: https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching
fn minimum_cacheable_tokens_for_model(model: &str) -> i32 {
    let m = model.to_lowercase();

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

    if m.contains("opus") || m.contains("sonnet") {
        return 1024;
    }

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
    use super::super::types::{CacheControl, Message, Metadata, MessagesRequest, SystemMessage};
    use serde_json::json;

    const LARGE_SYSTEM_CHARS: usize = 20_000; // 约 5k tokens（按 ~4 字符/token 估算，超过 sonnet-4.6 的 2048 门槛）

    fn tracker() -> CacheTracker {
        CacheTracker::new(DEFAULT_CACHE_TTL, CacheScope::Global, None)
    }

    fn tracker_per_credential() -> CacheTracker {
        CacheTracker::new(DEFAULT_CACHE_TTL, CacheScope::PerCredential, None)
    }

    /// 构造一个 JSON 格式的 metadata.user_id
    fn make_metadata(device_id: &str, account_uuid: &str, session_id: &str) -> Option<Metadata> {
        Some(Metadata {
            user_id: Some(
                serde_json::json!({
                    "device_id": device_id,
                    "account_uuid": account_uuid,
                    "session_id": session_id,
                })
                .to_string(),
            ),
        })
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

    fn ephemeral_1h() -> CacheControl {
        CacheControl {
            cache_type: "ephemeral".to_string(),
            ttl: Some("1h".to_string()),
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
        build_request_with_metadata(system_text, messages, None)
    }

    fn build_request_with_metadata(
        system_text: &str,
        messages: Vec<Message>,
        metadata: Option<Metadata>,
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
            metadata,
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

    /// 混合 TTL：每个 breakpoint 按自己的 TTL 单独计 cache_creation。
    #[test]
    fn mixed_ttl_breakpoints_segmented_into_own_buckets() {
        let tracker = CacheTracker::new(ONE_HOUR_CACHE_TTL, CacheScope::Global, None);
        let system = large_text("S ", LARGE_SYSTEM_CHARS);

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            stream: false,
            system: Some(vec![SystemMessage {
                text: system,
                block_type: Some("text".to_string()),
                cache_control: Some(ephemeral_1h()),
            }]),
            messages: vec![
                user_message(vec![json!({
                    "type": "text",
                    "text": large_text("U1 ", 12_000),
                    "cache_control": ephemeral_5m(),
                })]),
                assistant_text("reply"),
                user_message(vec![json!({
                    "type": "text",
                    "text": large_text("U2 ", 12_000),
                    "cache_control": ephemeral_5m(),
                })]),
            ],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let profile = tracker.build_profile(&req, 20_000);
        let r = tracker.compute_and_update(1, &profile);

        assert!(
            r.cache_creation_1h_input_tokens > 0,
            "system 1h breakpoint 应贡献 1h 桶，got={:?}",
            r
        );
        assert!(
            r.cache_creation_5m_input_tokens > 0,
            "user 5m breakpoints 应贡献 5m 桶，got={:?}",
            r
        );
        assert_eq!(
            r.cache_creation_5m_input_tokens + r.cache_creation_1h_input_tokens,
            r.cache_creation_input_tokens,
            "5m + 1h 之和应等于总 cache_creation，got={:?}",
            r
        );
    }

    /// TTL 顺序违规（1h 出现在 5m 之后）：Anthropic 会返回 400，
    /// 本地退化为无缓存。
    #[test]
    fn invalid_ttl_ordering_falls_back_to_uncached() {
        let tracker = CacheTracker::new(ONE_HOUR_CACHE_TTL, CacheScope::Global, None);
        let system = large_text("S ", LARGE_SYSTEM_CHARS);

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            stream: false,
            system: Some(vec![SystemMessage {
                text: system,
                block_type: Some("text".to_string()),
                cache_control: Some(ephemeral_5m()),
            }]),
            messages: vec![user_message(vec![json!({
                "type": "text",
                "text": large_text("U ", 12_000),
                "cache_control": ephemeral_1h(),
            })])],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let profile = tracker.build_profile(&req, 15_000);
        let r = tracker.compute_and_update(1, &profile);
        assert_eq!(r.cache_read_input_tokens, 0);
        assert_eq!(r.cache_creation_input_tokens, 0);
        assert_eq!(r.uncached_input_tokens, 15_000);
    }

    /// Cache Invalidation Summary: tool_choice ✓ ✓ ✘ —— 保留 tools/system，
    /// 失效 messages。messages 段 breakpoint 在 tool_choice 变化后应完全未命中。
    #[test]
    fn tool_choice_change_invalidates_messages_cache() {
        let tracker = tracker();
        let credential_id = 1;
        let system = large_text("S ", LARGE_SYSTEM_CHARS);

        let build = |tool_choice: serde_json::Value| MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            stream: false,
            system: Some(vec![SystemMessage {
                text: system.clone(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            messages: vec![user_message(vec![json!({
                "type": "text",
                "text": large_text("U ", 5_000),
                "cache_control": ephemeral_5m(),
            })])],
            tools: None,
            tool_choice: Some(tool_choice),
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let req1 = build(json!({"type": "auto"}));
        let p1 = tracker.build_profile(&req1, 10_000);
        let r1 = tracker.compute_and_update(credential_id, &p1);
        assert!(r1.cache_creation_input_tokens > 0);

        let req2 = build(json!({"type": "any"}));
        let p2 = tracker.build_profile(&req2, 10_000);
        let r2 = tracker.compute_and_update(credential_id, &p2);
        assert_eq!(
            r2.cache_read_input_tokens, 0,
            "tool_choice 变化应让 messages 段 breakpoint 完全未命中: {:?}",
            r2
        );
        assert!(r2.cache_creation_input_tokens > 0, "应重新创建缓存");
    }

    /// tool_choice 变化不应影响 tools 段 breakpoint 的命中（✓ tools 保留）。
    #[test]
    fn tool_choice_change_preserves_tools_cache() {
        let tracker = CacheTracker::new(DEFAULT_CACHE_TTL, CacheScope::Global, None);
        let credential_id = 1;

        let large_tool_desc = large_text("tool description ", 20_000);
        let tool = json!({
            "name": "big_tool",
            "description": large_tool_desc,
            "input_schema": {"type": "object"},
            "cache_control": {"type": "ephemeral"},
        });

        let build = |tool_choice: serde_json::Value| MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            stream: false,
            system: None,
            messages: vec![user_message(vec![json!({"type": "text", "text": "hi"})])],
            tools: Some(vec![serde_json::from_value(tool.clone()).unwrap()]),
            tool_choice: Some(tool_choice),
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let p1 = tracker.build_profile(&build(json!({"type": "auto"})), 6_000);
        let r1 = tracker.compute_and_update(credential_id, &p1);
        assert!(r1.cache_creation_input_tokens > 0);

        let p2 = tracker.build_profile(&build(json!({"type": "any"})), 6_000);
        let r2 = tracker.compute_and_update(credential_id, &p2);
        assert_eq!(
            r2.cache_read_input_tokens, r1.cache_creation_input_tokens,
            "tool_choice 变化不应失效 tools 段: {:?}",
            r2
        );
    }

    /// thinking 块上的 cache_control 应被忽略（Anthropic 不允许）。
    #[test]
    fn cache_control_on_thinking_block_ignored() {
        let tracker = tracker();
        let system = large_text("S ", LARGE_SYSTEM_CHARS);
        let req = build_request(
            &system,
            vec![user_message(vec![json!({
                "type": "thinking",
                "thinking": "internal reasoning",
                "cache_control": ephemeral_5m(),
            })])],
        );

        let profile = tracker.build_profile(&req, 10_000);
        let r = tracker.compute_and_update(1, &profile);
        assert_eq!(r.cache_read_input_tokens, 0);
        assert_eq!(r.cache_creation_input_tokens, 0);
        assert_eq!(
            r.uncached_input_tokens, 10_000,
            "thinking 块的 cache_control 无效，整段应为未缓存"
        );
    }

    /// 超过 4 个 cache_control breakpoint 时按 Anthropic 行为退化为无缓存。
    #[test]
    fn too_many_breakpoints_falls_back_to_uncached() {
        let tracker = tracker();
        let system = large_text("S ", LARGE_SYSTEM_CHARS);
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            stream: false,
            system: Some(vec![SystemMessage {
                text: system,
                block_type: Some("text".to_string()),
                cache_control: Some(ephemeral_5m()),
            }]),
            messages: vec![user_message(
                (0..5)
                    .map(|i| {
                        json!({
                            "type": "text",
                            "text": large_text(&format!("msg{} ", i), 3_000),
                            "cache_control": ephemeral_5m(),
                        })
                    })
                    .collect(),
            )],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let profile = tracker.build_profile(&req, 15_000);
        let r = tracker.compute_and_update(1, &profile);
        assert_eq!(r.cache_read_input_tokens, 0);
        assert_eq!(r.cache_creation_input_tokens, 0);
        assert_eq!(r.uncached_input_tokens, 15_000);
    }

    /// 同一 breakpoint 位置从 1h 写入后被 5m 覆盖，TTL 应 downgrade。
    #[test]
    fn breakpoint_ttl_downgrades_from_1h_to_5m() {
        let tracker = CacheTracker::new(ONE_HOUR_CACHE_TTL, CacheScope::Global, None);
        let system = large_text("S ", LARGE_SYSTEM_CHARS);

        let build = |ttl_1h: bool| MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            stream: false,
            system: Some(vec![SystemMessage {
                text: system.clone(),
                block_type: Some("text".to_string()),
                cache_control: Some(if ttl_1h { ephemeral_1h() } else { ephemeral_5m() }),
            }]),
            messages: vec![user_message(vec![json!({"type": "text", "text": "hi"})])],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let p1 = tracker.build_profile(&build(true), 10_000);
        let r1 = tracker.compute_and_update(1, &p1);
        assert!(r1.cache_creation_1h_input_tokens > 0, "首轮应写入 1h 桶");

        let p2 = tracker.build_profile(&build(false), 10_000);
        let r2 = tracker.compute_and_update(1, &p2);
        assert!(
            r2.cache_read_input_tokens > 0,
            "覆盖写入时仍能命中之前的 entry"
        );

        let entries = tracker.entries.lock();
        let bucket = entries.get(&GLOBAL_CREDENTIAL_KEY).expect("bucket 存在");
        let entry = bucket.values().next().expect("至少有一条 entry");
        assert_eq!(
            entry.ttl, DEFAULT_CACHE_TTL,
            "覆盖写入后 TTL 应 downgrade 为 5m"
        );
    }

    /// Global 模式：同一用户身份的不同 credential 共享 cache。
    #[test]
    fn global_scope_shares_by_identity_across_credentials() {
        let tracker = tracker(); // Global
        let sys = large_text("S ", LARGE_SYSTEM_CHARS);
        let meta = make_metadata("device-42", "acct-42", "sess-42");
        let req = build_request_with_metadata(
            &sys,
            vec![user_message(vec![json!({
                "type": "text",
                "text": "hello",
                "cache_control": ephemeral_5m(),
            })])],
            meta,
        );

        let p1 = tracker.build_profile(&req, 10_000);
        let r1 = tracker.compute_and_update(7, &p1);
        assert!(r1.cache_creation_input_tokens > 0);

        let p2 = tracker.build_profile(&req, 10_000);
        let r2 = tracker.compute_and_update(99, &p2);
        assert_eq!(
            r2.cache_read_input_tokens, r1.cache_creation_input_tokens,
            "同一用户身份跨 credential 应共享 cache: {:?}",
            r2
        );
    }

    /// Global 模式：不同用户身份自动隔离。
    #[test]
    fn global_scope_isolates_different_identities() {
        let tracker = tracker();
        let sys = large_text("S ", LARGE_SYSTEM_CHARS);
        let user = vec![user_message(vec![json!({
            "type": "text",
            "text": "hello",
            "cache_control": ephemeral_5m(),
        })])];

        let req_a = build_request_with_metadata(&sys, user.clone(), make_metadata("dev-alice", "acct-a", "sess-a"));
        let p_a = tracker.build_profile(&req_a, 10_000);
        let r_a = tracker.compute_and_update(1, &p_a);
        assert!(r_a.cache_creation_input_tokens > 0);

        let req_b = build_request_with_metadata(&sys, user, make_metadata("dev-bob", "acct-b", "sess-b"));
        let p_b = tracker.build_profile(&req_b, 10_000);
        let r_b = tracker.compute_and_update(1, &p_b);
        assert_eq!(
            r_b.cache_read_input_tokens, 0,
            "不同用户身份应相互隔离: {:?}",
            r_b
        );
    }

    /// PerCredential 模式：同一用户身份的不同 credential 互不共享。
    #[test]
    fn per_credential_scope_isolates_credentials_even_with_same_identity() {
        let tracker = tracker_per_credential();
        let sys = large_text("S ", LARGE_SYSTEM_CHARS);
        let meta = make_metadata("same-dev", "same-acct", "same-sess");
        let req = build_request_with_metadata(
            &sys,
            vec![user_message(vec![json!({
                "type": "text",
                "text": "hello",
                "cache_control": ephemeral_5m(),
            })])],
            meta,
        );

        let p1 = tracker.build_profile(&req, 10_000);
        let r1 = tracker.compute_and_update(1, &p1);
        assert!(r1.cache_creation_input_tokens > 0);

        let p2 = tracker.build_profile(&req, 10_000);
        let r2 = tracker.compute_and_update(2, &p2);
        assert_eq!(
            r2.cache_read_input_tokens, 0,
            "PerCredential 模式下即使同一用户身份也按 credential 隔离: {:?}",
            r2
        );
    }

    /// last_breakpoint 与更早的稳定 breakpoint（如 system）之间间隔 > 20 block
    /// 时，仍应通过 per-breakpoint 回扫命中更早的 breakpoint。
    #[test]
    fn per_breakpoint_lookback_finds_stable_prefix_beyond_20_blocks() {
        let tracker = tracker();
        let credential_id = 1;
        let system = large_text("S ", LARGE_SYSTEM_CHARS);

        // Turn 1: system 打 cache_control,user 不打 —— 只在 block 0 写 entry。
        let req1 = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            stream: false,
            system: Some(vec![SystemMessage {
                text: system.clone(),
                block_type: Some("text".to_string()),
                cache_control: Some(ephemeral_5m()),
            }]),
            messages: vec![user_message(vec![json!({ "type": "text", "text": "hi" })])],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let p1 = tracker.build_profile(&req1, 10_000);
        let r1 = tracker.compute_and_update(credential_id, &p1);
        assert!(
            r1.cache_creation_input_tokens > 0,
            "turn 1 应为 system 建立缓存"
        );
        let system_tokens = r1.cache_creation_input_tokens;

        // Turn 2: 同样 system 打 cache_control,中间塞 25 个 padding block,
        // 只在最后一个 block 打 cache_control。last_bp 位于 ≥ block 26,
        // 与 system bp（block 0）相距 > 20。
        let mut padding: Vec<serde_json::Value> = (0..25)
            .map(|i| json!({ "type": "text", "text": format!("pad {}", i) }))
            .collect();
        padding.push(json!({
            "type": "text",
            "text": "final",
            "cache_control": ephemeral_5m(),
        }));

        let req2 = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            stream: false,
            system: Some(vec![SystemMessage {
                text: system,
                block_type: Some("text".to_string()),
                cache_control: Some(ephemeral_5m()),
            }]),
            messages: vec![user_message(padding)],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let p2 = tracker.build_profile(&req2, 15_000);
        let r2 = tracker.compute_and_update(credential_id, &p2);

        assert_eq!(
            r2.cache_read_input_tokens, system_tokens,
            "system bp 距 last_bp > 20 block 时,per-breakpoint 回扫仍应命中 system: {:?}",
            r2
        );
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
